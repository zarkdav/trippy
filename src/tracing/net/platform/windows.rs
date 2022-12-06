use super::byte_order::PlatformIpv4FieldByteOrder;
use crate::tracing::error::{TraceResult, TracerError};
use crate::tracing::net::channel::MAX_PACKET_SIZE;
use core::convert;
use std::alloc::{alloc, dealloc, Layout};
use std::ffi::c_void;
use std::fmt;
use std::io::{Error, ErrorKind};
use std::mem::MaybeUninit;
use std::mem::{align_of, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use widestring::WideCString;
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR, WAIT_TIMEOUT};
use windows_sys::Win32::NetworkManagement::IpHelper;
use windows_sys::Win32::Networking::WinSock::{
    bind, closesocket, sendto, setsockopt, socket, WSACleanup, WSACloseEvent, WSACreateEvent,
    WSAGetOverlappedResult, WSAIoctl, WSARecvFrom, WSAStartup, WSAWaitForMultipleEvents,
    ADDRESS_FAMILY, AF_INET, AF_INET6, FIONBIO, IN6_ADDR, IN6_ADDR_0, INVALID_SOCKET, IN_ADDR,
    IN_ADDR_0, IPPROTO, IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_RAW,
    IPPROTO_TCP, IPPROTO_UDP, IPV6_UNICAST_HOPS, IP_HDRINCL, SIO_ROUTING_INTERFACE_QUERY, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKET, SOCKET_ERROR, SOCK_DGRAM, SOCK_RAW,
    SOCK_STREAM, SOL_SOCKET, SO_PORT_SCALABILITY, WSABUF, WSADATA, WSA_WAIT_FAILED,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

#[derive(Debug)]
pub struct Socket {
    pub s: SOCKET,
    pub ol: Overlapped,
    pub wbuf: WsaBuf,
    pub from: *mut SOCKADDR,
}
impl Socket {
    #[allow(unsafe_code)]
    pub fn create(af: ADDRESS_FAMILY, r#type: u16, protocol: IPPROTO) -> TraceResult<Self> {
        let s = make_socket(af, r#type, protocol)?;

        let from = MaybeUninit::<SOCKADDR>::zeroed().as_mut_ptr();
        let ptr = [0u8; MAX_PACKET_SIZE].as_mut_ptr();

        let wbuf = WSABUF {
            buf: ptr,
            len: MAX_PACKET_SIZE as u32,
        };

        // let ol = create_overlapped_event()?;
        let ol = unsafe { core::mem::zeroed() };

        Ok(Self {
            s,
            ol: Overlapped(ol),
            wbuf: WsaBuf(wbuf),
            from,
        })
    }

    pub fn udp_from(target: IpAddr) -> TraceResult<Self> {
        let s = match target {
            IpAddr::V4(_) => Self::create(AF_INET, SOCK_DGRAM, IPPROTO_UDP),
            IpAddr::V6(_) => Self::create(AF_INET6, SOCK_DGRAM, IPPROTO_UDP),
        }?;
        Ok(s)
    }

    #[allow(unsafe_code)]
    pub fn bind(&self, source_addr: IpAddr) -> TraceResult<&Self> {
        let (addr, addrlen) = ipaddr_to_sockaddr(source_addr);
        if unsafe {
            bind(
                self.s,
                std::ptr::addr_of!(addr).cast(),
                addrlen.try_into().unwrap(),
            )
        } == SOCKET_ERROR
        {
            eprintln!("bind: failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        eprintln!("bind OK");
        Ok(self)
    }

    #[allow(unsafe_code)]
    pub fn sendto(&self, packet: &[u8], dest_addr: IpAddr) -> TraceResult<()> {
        let (addr, addrlen) = ipaddr_to_sockaddr(dest_addr);
        let rc = unsafe {
            sendto(
                self.s,
                std::ptr::addr_of!(packet).cast(),
                packet.len().try_into().unwrap(),
                0,
                std::ptr::addr_of!(addr).cast(),
                addrlen.try_into().unwrap(),
            )
        };
        if rc == SOCKET_ERROR {
            eprintln!("dispatch_icmp_probe: sendto failed with error");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    pub fn get_overlapped_result(&self) -> bool {
        // let ol = self.ol.0;
        (unsafe { WSAGetOverlappedResult(self.s, &self.ol.0, &mut 0, 0, &mut 0) } != 0)
    }

    #[allow(unsafe_code)]
    pub fn close(&self) -> TraceResult<()> {
        if unsafe { closesocket(self.s) } == SOCKET_ERROR {
            eprintln!("closesocket: failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        Ok(())
    }

    // NOTE FIONBIO is really unsigned (in WinSock2.h)
    #[allow(clippy::cast_sign_loss)]
    #[allow(unsafe_code)]
    fn set_non_blocking(self, is_non_blocking: bool) -> TraceResult<Self> {
        let non_blocking: u32 = if is_non_blocking { 1 } else { 0 };
        if unsafe {
            WSAIoctl(
                self.s,
                FIONBIO as u32,
                std::ptr::addr_of!(non_blocking).cast(),
                size_of::<u32>().try_into().unwrap(),
                std::ptr::null_mut(),
                0,
                &mut 0,
                std::ptr::null_mut(),
                None,
            )
        } == SOCKET_ERROR
        {
            eprintln!("set_non_blocking: WSAIoctl failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        eprintln!("WSAIoctl(non_blocking) OK");
        Ok(self)
    }

    #[allow(unsafe_code)]
    fn set_header_included(self, is_header_included: bool) -> TraceResult<Self> {
        let u32_header_included: u32 = if is_header_included { 1 } else { 0 };
        let header_included = u32_header_included.to_ne_bytes();
        let optval = std::ptr::addr_of!(header_included).cast();
        if unsafe {
            setsockopt(
                self.s,
                IPPROTO_IP.try_into().unwrap(),
                IP_HDRINCL.try_into().unwrap(),
                optval,
                std::mem::size_of::<u32>().try_into().unwrap(),
            )
        } == SOCKET_ERROR
        {
            eprintln!("set_header_included: setsockopt failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        eprintln!("setsockopt(header_included) OK");
        Ok(self)
    }

    #[allow(unsafe_code)]
    fn set_reuse_port(self, is_reuse_port: bool) -> TraceResult<Self> {
        let u32_reuse_port: u32 = if is_reuse_port { 1 } else { 0 };
        let reuse_port = u32_reuse_port.to_ne_bytes();
        let optval = std::ptr::addr_of!(reuse_port).cast();
        if unsafe {
            setsockopt(
                self.s,
                SOL_SOCKET.try_into().unwrap(),
                SO_PORT_SCALABILITY.try_into().unwrap(),
                optval,
                std::mem::size_of::<u32>().try_into().unwrap(),
            )
        } == SOCKET_ERROR
        {
            eprintln!("set_reuse_port: setsockopt failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        eprintln!("setsockopt(reuse_port) OK");
        Ok(self)
    }

    #[allow(unsafe_code)]
    pub fn set_ipv6_max_hops(&self, max_hops: u8) -> TraceResult<&Self> {
        if unsafe {
            setsockopt(
                self.s,
                IPPROTO_IPV6,
                IPV6_UNICAST_HOPS.try_into().unwrap(),
                &max_hops, // TODO check LE/BE
                std::mem::size_of::<u32>().try_into().unwrap(),
            )
        } == SOCKET_ERROR
        {
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        Ok(self)
    }

    #[allow(unsafe_code)]
    pub fn recv_from(&mut self) -> TraceResult<()> {
        let mut fromlen = std::mem::size_of::<SOCKADDR>().try_into().unwrap();

        let ret = unsafe {
            WSARecvFrom(
                self.s,
                &self.wbuf.0,
                1,
                &mut 0,
                &mut 0,
                self.from,
                &mut fromlen,
                &mut self.ol.0,
                None,
            )
        };
        if ret == SOCKET_ERROR {
            eprintln!("WSARecvFrom failed");
            return Err(TracerError::IoError(Error::last_os_error()));
        };
        eprintln!("WSARecvFrom OK");
        Ok(())
    }
}

impl convert::From<Socket> for SOCKET {
    fn from(sock: Socket) -> Self {
        sock.s
    }
}
impl convert::From<&Socket> for SOCKET {
    fn from(sock: &Socket) -> Self {
        sock.s
    }
}
impl convert::From<&mut Socket> for SOCKET {
    fn from(sock: &mut Socket) -> Self {
        sock.s
    }
}
pub struct Overlapped(pub OVERLAPPED);
impl fmt::Debug for Overlapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Overlapped")
            .field("hEvent", &self.0.hEvent)
            .finish()
    }
}
impl convert::From<Overlapped> for OVERLAPPED {
    fn from(ol: Overlapped) -> Self {
        ol.0
    }
}
impl convert::From<&Overlapped> for OVERLAPPED {
    fn from(ol: &Overlapped) -> Self {
        ol.0
    }
}
impl convert::From<&mut Overlapped> for OVERLAPPED {
    fn from(ol: &mut Overlapped) -> Self {
        ol.0
    }
}

pub struct WsaBuf(pub WSABUF);
impl fmt::Debug for WsaBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsaBuf")
            .field("buf", &self.0.buf)
            .field("len", &self.0.len)
            .finish()
    }
}

#[allow(unsafe_code)]
pub fn startup() -> TraceResult<()> {
    const WINSOCK_VERSION: u16 = 0x202; // 2.2

    let mut wsd = MaybeUninit::<WSADATA>::zeroed();
    let rc = unsafe { WSAStartup(WINSOCK_VERSION, wsd.as_mut_ptr()) };
    unsafe { wsd.assume_init() }; // extracts the WSDATA to ensure it gets dropped (it's not used ATM)
    if rc == 0 {
        eprintln!("WSAStartup OK");
        Ok(())
    } else {
        eprintln!("WSAStartup failed");
        Err(TracerError::IoError(Error::last_os_error()))
    }
}

#[allow(unsafe_code)]
pub fn cleanup(sockets: &[Socket]) -> TraceResult<()> {
    let layout = Layout::from_size_align(MAX_PACKET_SIZE, std::mem::align_of::<WSABUF>()).unwrap();
    for sock in sockets {
        if unsafe { closesocket(sock.s) } == SOCKET_ERROR {
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        if sock.ol.0.hEvent != 0 && unsafe { WSACloseEvent(sock.ol.0.hEvent) } == 0 {
            return Err(TracerError::IoError(Error::last_os_error()));
        }
        unsafe { dealloc(sock.wbuf.0.buf, layout) };
        // should we cleanup sock.from too?
    }
    if unsafe { WSACleanup() } == SOCKET_ERROR {
        return Err(TracerError::IoError(Error::last_os_error()));
    };
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
pub fn for_address(_src_addr: IpAddr) -> TraceResult<PlatformIpv4FieldByteOrder> {
    Ok(PlatformIpv4FieldByteOrder::Network)
}

// inspired by <https://github.com/EstebanBorai/network-interface/blob/main/src/target/windows.rs>
#[allow(unsafe_code)]
fn lookup_interface_addr(family: ADDRESS_FAMILY, name: &str) -> TraceResult<IpAddr> {
    // Max tries allowed to call `GetAdaptersAddresses` on a loop basis
    const MAX_TRIES: usize = 3;
    let flags = IpHelper::GAA_FLAG_SKIP_ANYCAST
        | IpHelper::GAA_FLAG_SKIP_MULTICAST
        | IpHelper::GAA_FLAG_SKIP_DNS_SERVER;
    // Initial buffer size is 15k per <https://learn.microsoft.com/en-us/windows/win32/api/iphlpapi/nf-iphlpapi-getadaptersaddresses>
    let mut buf_len: u32 = 15000;
    let mut layout;
    let mut list_ptr;
    let mut ip_adapter_address;
    let mut res;
    let mut i = 0;

    loop {
        layout = match Layout::from_size_align(
            buf_len as usize,
            align_of::<IpHelper::IP_ADAPTER_ADDRESSES_LH>(),
        ) {
            Ok(layout) => layout,
            Err(e) => {
                return Err(TracerError::ErrorString(format!(
                    "Could not compute layout for {} words: {}",
                    buf_len, e
                )))
            }
        };
        list_ptr = unsafe { alloc(layout) };
        if list_ptr.is_null() {
            return Err(TracerError::ErrorString(format!(
                "Could not allocate {} words for layout {:?}",
                buf_len, layout
            )));
        }
        ip_adapter_address = list_ptr.cast();

        res = unsafe {
            IpHelper::GetAdaptersAddresses(
                family,
                flags,
                std::ptr::null_mut(),
                ip_adapter_address,
                &mut buf_len,
            )
        };
        i += 1;

        if res != ERROR_BUFFER_OVERFLOW || i > MAX_TRIES {
            break;
        }

        unsafe { dealloc(list_ptr, layout) };
    }

    if res != NO_ERROR {
        return Err(TracerError::ErrorString(format!(
            "GetAdaptersAddresses returned error: {}",
            Error::from_raw_os_error(res.try_into().unwrap())
        )));
    }

    while !ip_adapter_address.is_null() {
        let friendly_name = unsafe {
            let friendly_name = (*ip_adapter_address).FriendlyName;
            WideCString::from_ptr_str(friendly_name)
                .to_string()
                .unwrap()
        };
        if name == friendly_name {
            // PANIC should not occur as GetAdaptersAddress should return valid PWSTR
            // NOTE this really should be a while over the linked list of FistUnicastAddress, and current_unicast would then be mutable
            // however, this is not supported by our function signature
            let current_unicast = unsafe { (*ip_adapter_address).FirstUnicastAddress };
            // while !current_unicast.is_null() {
            unsafe {
                let socket_address = (*current_unicast).Address;
                let ip_addr = sockaddrptr_to_ipaddr(socket_address.lpSockaddr);
                dealloc(list_ptr, layout);
                return ip_addr;
            }
            // current_unicast = unsafe { (*current_unicast).Next };
            // }
        }
        ip_adapter_address = unsafe { (*ip_adapter_address).Next };
    }

    unsafe {
        dealloc(list_ptr, layout);
    }

    Err(TracerError::UnknownInterface(format!(
        "could not find address for {}",
        name
    )))
}

#[allow(unsafe_code)]
pub fn sockaddrptr_to_ipaddr(ptr: *const SOCKADDR) -> TraceResult<IpAddr> {
    let af = unsafe { u32::from((*ptr).sa_family) };
    if af == AF_INET {
        let ipv4addr = unsafe { (*(ptr.cast::<SOCKADDR_IN>())).sin_addr };
        Ok(IpAddr::V4(Ipv4Addr::from(unsafe {
            ipv4addr.S_un.S_addr.to_ne_bytes()
        })))
    } else if af == AF_INET6 {
        #[allow(clippy::cast_ptr_alignment)]
        let ipv6addr = unsafe { (*(ptr.cast::<SOCKADDR_IN6>())).sin6_addr };
        Ok(IpAddr::V6(Ipv6Addr::from(unsafe { ipv6addr.u.Byte })))
    } else {
        Err(TracerError::IoError(Error::new(
            ErrorKind::Unsupported,
            format!("Unsupported address family: {}", af),
        )))
    }
}

#[allow(unsafe_code)]
pub fn ipaddr_to_sockaddr(source_addr: IpAddr) -> (SOCKADDR, u32) {
    let (paddr, addrlen): (*const SOCKADDR, u32) = match source_addr {
        IpAddr::V4(ipv4addr) => {
            let sai = SOCKADDR_IN {
                sin_family: AF_INET.try_into().unwrap(),
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from(ipv4addr).to_be(),
                    },
                },
                sin_zero: [0; 8],
            };
            (
                std::ptr::addr_of!(sai).cast(),
                size_of::<SOCKADDR_IN>().try_into().unwrap(),
            )
        }
        IpAddr::V6(ipv6addr) => {
            let sai = SOCKADDR_IN6 {
                sin6_family: AF_INET6.try_into().unwrap(),
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: ipv6addr.octets(),
                    },
                },
                Anonymous: SOCKADDR_IN6_0 { sin6_scope_id: 0 },
            };
            (
                std::ptr::addr_of!(sai).cast(),
                size_of::<SOCKADDR_IN6>().try_into().unwrap(),
            )
        }
    };
    unsafe { (*paddr, addrlen) }
}

pub fn lookup_interface_addr_ipv4(name: &str) -> TraceResult<IpAddr> {
    lookup_interface_addr(AF_INET, name)
}

pub fn lookup_interface_addr_ipv6(name: &str) -> TraceResult<IpAddr> {
    lookup_interface_addr(AF_INET6, name)
}

#[allow(unsafe_code)]
pub fn routing_interface_query(target: IpAddr) -> TraceResult<IpAddr> {
    let src: *mut c_void = [0; 1024].as_mut_ptr().cast();
    let bytes = MaybeUninit::<u32>::uninit().as_mut_ptr();
    let s = Socket::udp_from(target)?;
    let (dest, destlen) = ipaddr_to_sockaddr(target);
    let rc = unsafe {
        WSAIoctl(
            s.s,
            SIO_ROUTING_INTERFACE_QUERY,
            std::ptr::addr_of!(dest).cast(),
            destlen,
            src,
            1024,
            bytes,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc == SOCKET_ERROR {
        eprintln!("routing_interface_query: WSAIoctl failed");
        return Err(TracerError::IoError(Error::last_os_error()));
    }
    eprintln!("WSAIoctl(routing_interface_query) OK");

    /*
    NOTE The WSAIoctl call potentially returns multiple results (see
    <https://www.winsocketdotnetworkprogramming.com/winsock2programming/winsock2advancedsocketoptionioctl7h.html>),
    TBD We choose the first one arbitrarily.
     */
    sockaddrptr_to_ipaddr(src.cast())
}

#[allow(unsafe_code)]
fn make_socket(af: ADDRESS_FAMILY, r#type: u16, protocol: IPPROTO) -> TraceResult<SOCKET> {
    let s = unsafe { socket(af.try_into().unwrap(), i32::from(r#type), protocol) };
    if s == INVALID_SOCKET {
        eprintln!("make_socket: socket failed");
        Err(TracerError::IoError(Error::last_os_error()))
    } else {
        eprintln!("socket OK");
        Ok(s)
    }
}

#[allow(unsafe_code)]
fn create_overlapped_event() -> TraceResult<OVERLAPPED> {
    let mut recv_ol: OVERLAPPED = unsafe { core::mem::zeroed() };
    recv_ol.hEvent = unsafe { WSACreateEvent() };
    if recv_ol.hEvent == 0 {
        eprintln!("create_overlapped_event: WSACreateEvent failed");
        return Err(TracerError::IoError(Error::last_os_error()));
    }
    eprintln!("WSACreateEvent OK: {:?}", recv_ol.hEvent);
    Ok(recv_ol)
}

#[allow(unsafe_code)]
pub fn make_icmp_send_socket_ipv4() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET, SOCK_RAW, IPPROTO_RAW)?;
    eprintln!("created ICMP send Socket {:?}", sock);
    sock.set_non_blocking(true)?.set_header_included(true)
}

#[allow(unsafe_code)]
pub fn make_udp_send_socket_ipv4() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET, SOCK_RAW, IPPROTO_RAW)?;
    eprintln!("created UDP send Socket {:?}", sock);
    sock.set_non_blocking(true)?.set_header_included(true)
}

#[allow(unsafe_code)]
pub fn make_recv_socket_ipv4(src_addr: Ipv4Addr) -> TraceResult<Socket> {
    let mut sock = Socket::create(AF_INET, SOCK_RAW, IPPROTO_ICMP)?;
    sock.bind(IpAddr::V4(src_addr))?;
    sock.ol.0 = create_overlapped_event()?;
    sock.recv_from()?;
    eprintln!("Created ICMP recv Socket {:?}", sock);
    sock.set_non_blocking(true)?.set_header_included(true)
}

#[allow(unsafe_code)]
pub fn make_icmp_send_socket_ipv6() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)?;
    eprintln!("created ICMP send Socket {:?}", sock);
    sock.set_non_blocking(true)
}

#[allow(unsafe_code)]
pub fn make_udp_send_socket_ipv6() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET6, SOCK_RAW, IPPROTO_UDP)?;
    sock.set_non_blocking(true)
}

#[allow(unsafe_code)]
pub fn make_recv_socket_ipv6(src_addr: Ipv6Addr) -> TraceResult<Socket> {
    let mut sock = Socket::create(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)?;
    sock.bind(IpAddr::V6(src_addr))?;
    sock.ol.0 = create_overlapped_event()?;
    sock.recv_from()?;
    eprintln!("WSACreateEvent OK\nCreated ICMP recv Socket {:?}", sock);
    sock.set_non_blocking(true)
}

#[allow(unsafe_code)]
pub fn make_stream_socket_ipv4() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET, SOCK_STREAM, IPPROTO_TCP)?;
    sock.set_non_blocking(true)?.set_reuse_port(true)
}

#[allow(unsafe_code)]
pub fn make_stream_socket_ipv6() -> TraceResult<Socket> {
    let sock = Socket::create(AF_INET6, SOCK_STREAM, IPPROTO_TCP)?;
    sock.set_non_blocking(true)?.set_reuse_port(true)
}

#[allow(unsafe_code)]
pub fn is_readable(sock: &Socket, timeout: Duration) -> TraceResult<bool> {
    let millis = timeout.as_millis().try_into().unwrap();
    let ev = sock.ol.0.hEvent;
    let rc = unsafe { WSAWaitForMultipleEvents(1, &[ev] as *const _, 1, millis, 0) };
    eprintln!(
        "WSAWaitForMultipleEvents on Overlapped {:?} returned {}",
        ev, rc
    );
    if rc == WSA_WAIT_FAILED {
        eprintln!("is_readable: WSAWaitForMultipleEvents failed");
        return Err(TracerError::IoError(Error::last_os_error()));
    }
    Ok(rc != WAIT_TIMEOUT)
}

/// TODO
pub fn is_writable(_sock: &Socket) -> TraceResult<bool> {
    unimplemented!()
}

/// TODO
pub fn is_not_in_progress_error(_code: i32) -> bool {
    unimplemented!()
}

/// TODO
pub fn is_conn_refused_error(_code: i32) -> bool {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn test_ipv4_interface_lookup() {
        let res = lookup_interface_addr_ipv4("vEthernet (External Switch)").unwrap();
        let addr: IpAddr = "192.168.2.2".parse().unwrap();
        assert_eq!(res, addr);
    }

    #[test]
    fn test_ipv6_interface_lookup() {
        let res = lookup_interface_addr_ipv6("vEthernet (External Switch)").unwrap();
        let addr: IpAddr = "fe80::f31a:9c2f:4f14:105b".parse().unwrap();
        assert_eq!(res, addr);
    }
}
