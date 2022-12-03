use crate::tracing::error::TracerError::InvalidSourceAddr;
use crate::tracing::error::{TraceResult, TracerError};
use crate::tracing::net::platform::PlatformIpv4FieldByteOrder;
use crate::tracing::net::{ipv4, ipv6};
use crate::tracing::net::{platform, Network};
use crate::tracing::probe::ProbeResponse;
use crate::tracing::types::{PacketSize, PayloadPattern, Port, Sequence, TraceId, TypeOfService};
use crate::tracing::util::Required;
use crate::tracing::{
    MultipathStrategy, PortDirection, Probe, TracerAddrFamily, TracerChannelConfig, TracerProtocol,
};
use arrayvec::ArrayVec;
use itertools::Itertools;
#[cfg(not(windows))]
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::ffi::c_void;
use std::io::Error;
use std::mem::MaybeUninit;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};
#[cfg(windows)]
use windows::Win32::Networking::WinSock::{
    bind, socket, WSAIoctl, AF_INET, AF_INET6, INVALID_SOCKET, IPPROTO_UDP,
    SIO_ROUTING_INTERFACE_QUERY, SOCKET, SOCKET_ERROR, SOCK_DGRAM,
};
#[cfg(windows)]
use windows::Win32::System::IO::OVERLAPPED;

#[cfg(windows)]
type Socket = SOCKET;

/// The maximum size of the IP packet we allow.
pub const MAX_PACKET_SIZE: usize = 1024;

/// The maximum number of TCP probes we allow.
const MAX_TCP_PROBES: usize = 256;

/// The port used for local address discovery if not dest port is available.
const DISCOVERY_PORT: Port = Port(80);

/// A channel for sending and receiving `Probe` packets.
pub struct TracerChannel {
    protocol: TracerProtocol,
    addr_family: TracerAddrFamily,
    src_addr: IpAddr,
    ipv4_length_order: PlatformIpv4FieldByteOrder,
    dest_addr: IpAddr,
    identifier: TraceId,
    packet_size: PacketSize,
    payload_pattern: PayloadPattern,
    tos: TypeOfService,
    initial_sequence: Sequence,
    multipath_strategy: MultipathStrategy,
    port_direction: PortDirection,
    read_timeout: Duration,
    tcp_connect_timeout: Duration,
    icmp_send_socket: Socket,
    udp_send_socket: Socket,
    recv_socket: Socket,
    #[cfg(windows)]
    recv_ol: OVERLAPPED,
    tcp_probes: ArrayVec<TcpProbe, MAX_TCP_PROBES>,
}

impl TracerChannel {
    /// Create an `IcmpChannel`.
    ///
    /// This operation requires the `CAP_NET_RAW` capability on Linux.
    pub fn connect(config: &TracerChannelConfig) -> TraceResult<Self> {
        if usize::from(config.packet_size.0) > MAX_PACKET_SIZE {
            return Err(TracerError::InvalidPacketSize(usize::from(
                config.packet_size.0,
            )));
        }
        let src_addr = make_src_addr(
            config.source_addr,
            config.target_addr,
            config.port_direction,
            config.interface.as_deref(),
            config.addr_family,
        )?;
        let ipv4_length_order = PlatformIpv4FieldByteOrder::for_address(src_addr)?;
        let icmp_send_socket = make_icmp_send_socket(config.addr_family)?;
        let udp_send_socket = make_udp_send_socket(config.addr_family)?;
        #[cfg(unix)]
        let recv_socket = make_recv_socket(config.addr_family)?;
        #[cfg(windows)]
        let (recv_socket, recv_ol) = make_recv_socket(config.addr_family)?;
        Ok(Self {
            protocol: config.protocol,
            addr_family: config.addr_family,
            src_addr,
            ipv4_length_order,
            dest_addr: config.target_addr,
            identifier: config.identifier,
            packet_size: config.packet_size,
            payload_pattern: config.payload_pattern,
            tos: config.tos,
            initial_sequence: config.initial_sequence,
            multipath_strategy: config.multipath_strategy,
            port_direction: config.port_direction,
            read_timeout: config.read_timeout,
            tcp_connect_timeout: config.tcp_connect_timeout,
            icmp_send_socket,
            udp_send_socket,
            recv_socket,
            #[cfg(windows)]
            recv_ol,
            tcp_probes: ArrayVec::new(),
        })
    }

    /// Get the source `IpAddr` of the channel.
    #[must_use]
    pub fn src_addr(&self) -> IpAddr {
        self.src_addr
    }
}

impl Network for TracerChannel {
    fn send_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match self.protocol {
            TracerProtocol::Icmp => self.dispatch_icmp_probe(probe),
            TracerProtocol::Udp => self.dispatch_udp_probe(probe),
            TracerProtocol::Tcp => self.dispatch_tcp_probe(probe),
        }
    }

    fn recv_probe(&mut self) -> TraceResult<Option<ProbeResponse>> {
        match self.protocol {
            TracerProtocol::Icmp | TracerProtocol::Udp => self.recv_icmp_probe(),
            TracerProtocol::Tcp => Ok(self.recv_tcp_sockets()?.or(self.recv_icmp_probe()?)),
        }
    }
}

impl TracerChannel {
    /// Dispatch a ICMP probe.
    fn dispatch_icmp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match (self.addr_family, self.src_addr, self.dest_addr) {
            (TracerAddrFamily::Ipv4, IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => {
                ipv4::dispatch_icmp_probe(
                    &mut self.icmp_send_socket,
                    probe,
                    src_addr,
                    dest_addr,
                    self.identifier,
                    self.packet_size,
                    self.payload_pattern,
                    self.ipv4_length_order,
                )
            }
            (TracerAddrFamily::Ipv6, IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => {
                ipv6::dispatch_icmp_probe(
                    &mut self.icmp_send_socket,
                    probe,
                    src_addr,
                    dest_addr,
                    self.identifier,
                    self.packet_size,
                    self.payload_pattern,
                )
            }
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    /// Dispatch a UDP probe.
    fn dispatch_udp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        match (self.addr_family, self.src_addr, self.dest_addr) {
            (TracerAddrFamily::Ipv4, IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => {
                ipv4::dispatch_udp_probe(
                    &mut self.udp_send_socket,
                    probe,
                    src_addr,
                    dest_addr,
                    self.initial_sequence,
                    self.multipath_strategy,
                    self.port_direction,
                    self.packet_size,
                    self.payload_pattern,
                    self.ipv4_length_order,
                )
            }
            (TracerAddrFamily::Ipv6, IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => {
                ipv6::dispatch_udp_probe(
                    &mut self.udp_send_socket,
                    probe,
                    src_addr,
                    dest_addr,
                    self.port_direction,
                    self.packet_size,
                    self.payload_pattern,
                )
            }
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    /// Dispatch a TCP probe.
    fn dispatch_tcp_probe(&mut self, probe: Probe) -> TraceResult<()> {
        let socket = match (self.addr_family, self.src_addr, self.dest_addr) {
            (TracerAddrFamily::Ipv4, IpAddr::V4(src_addr), IpAddr::V4(dest_addr)) => {
                ipv4::dispatch_tcp_probe(probe, src_addr, dest_addr, self.port_direction, self.tos)
            }
            (TracerAddrFamily::Ipv6, IpAddr::V6(src_addr), IpAddr::V6(dest_addr)) => {
                ipv6::dispatch_tcp_probe(probe, src_addr, dest_addr, self.port_direction)
            }
            _ => unreachable!(),
        }?;
        self.tcp_probes
            .push(TcpProbe::new(socket, SystemTime::now()));
        Ok(())
    }

    #[cfg(unix)]
    /// Generate a `ProbeResponse` for the next available ICMP packet, if any
    fn recv_icmp_probe(&mut self) -> TraceResult<Option<ProbeResponse>> {
        if platform::is_readable(&self.recv_socket, self.read_timeout)? {
            match self.addr_family {
                TracerAddrFamily::Ipv4 => ipv4::recv_icmp_probe(
                    &mut self.recv_socket,
                    self.protocol,
                    self.multipath_strategy,
                    self.port_direction,
                ),
                TracerAddrFamily::Ipv6 => {
                    ipv6::recv_icmp_probe(&mut self.recv_socket, self.protocol, self.port_direction)
                }
            }
        } else {
            Ok(None)
        }
    }

    #[cfg(windows)]
    fn recv_icmp_probe(&mut self) -> TraceResult<Option<ProbeResponse>> {
        if platform::is_readable(&self.recv_socket, &self.recv_ol, self.read_timeout)? {
            match self.addr_family {
                TracerAddrFamily::Ipv4 => ipv4::recv_icmp_probe(
                    &mut self.recv_socket,
                    &mut self.recv_ol,
                    self.protocol,
                    self.multipath_strategy,
                    self.port_direction,
                ),
                TracerAddrFamily::Ipv6 => ipv6::recv_icmp_probe(
                    &mut self.recv_socket,
                    &mut self.recv_ol,
                    self.protocol,
                    self.port_direction,
                ),
            }
        } else {
            Ok(None)
        }
    }

    #[cfg(unix)]
    /// Generate synthetic `ProbeResponse` if a TCP socket is connected or if the connection was refused.
    ///
    /// Any TCP socket which has not connected or failed after a timeout wil be removed.
    fn recv_tcp_sockets(&mut self) -> TraceResult<Option<ProbeResponse>> {
        self.tcp_probes
            .retain(|probe| probe.start.elapsed().unwrap_or_default() < self.tcp_connect_timeout);
        let found_index = self
            .tcp_probes
            .iter()
            .find_position(|&probe| platform::is_writable(&probe.socket).unwrap_or_default())
            .map(|(i, _)| i);
        if let Some(i) = found_index {
            let probe = self.tcp_probes.remove(i);
            match self.addr_family {
                TracerAddrFamily::Ipv4 => ipv4::recv_tcp_socket(&probe.socket, self.dest_addr),
                TracerAddrFamily::Ipv6 => ipv6::recv_tcp_socket(&probe.socket, self.dest_addr),
            }
        } else {
            Ok(None)
        }
    }
}

/// An entry in the TCP probes array.
#[derive(Debug)]
struct TcpProbe {
    socket: Socket,
    start: SystemTime,
}

impl TcpProbe {
    pub fn new(socket: Socket, start: SystemTime) -> Self {
        Self { socket, start }
    }
}

/// Validate, Lookup or discover the source `IpAddr`.
fn make_src_addr(
    source_addr: Option<IpAddr>,
    target_addr: IpAddr,
    port_direction: PortDirection,
    interface: Option<&str>,
    addr_family: TracerAddrFamily,
) -> TraceResult<IpAddr> {
    match (source_addr, interface.as_ref()) {
        (Some(addr), None) => validate_local_addr(addr_family, addr),
        (None, Some(interface)) => lookup_interface_addr(addr_family, interface),
        (None, None) => discover_local_addr(
            addr_family,
            target_addr,
            port_direction.dest().unwrap_or(DISCOVERY_PORT).0,
        ),
        (Some(_), Some(_)) => unreachable!(),
    }
}

/// Lookup the address for a named interface.
fn lookup_interface_addr(addr_family: TracerAddrFamily, name: &str) -> TraceResult<IpAddr> {
    match addr_family {
        TracerAddrFamily::Ipv4 => platform::lookup_interface_addr_ipv4(name),
        TracerAddrFamily::Ipv6 => platform::lookup_interface_addr_ipv6(name),
    }
}

/// Discover the local `IpAddr` that will be used to communicate with the given target `IpAddr`.
///
/// Note that no packets are transmitted by this method.
#[cfg(unix)]
fn discover_local_addr(
    addr_family: TracerAddrFamily,
    target: IpAddr,
    port: u16,
) -> TraceResult<IpAddr> {
    let socket = udp_socket_for_addr_family(addr_family)?;
    socket.connect(&SockAddr::from(SocketAddr::new(target, port)))?;
    Ok(socket.local_addr()?.as_socket().req()?.ip())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn discover_local_addr(
    addr_family: TracerAddrFamily,
    target: IpAddr,
    _port: u16,
) -> TraceResult<IpAddr> {
    /*
    NOTE under Windows, we cannot use a blind connect/getsockname as "If the socket
    is using a connectionless protocol, the address may not be available until I/O
    occurs on the socket."
    We use SIO_ROUTING_INTERFACE_QUERY instead.
    */

    let src: *mut c_void = [0; 1024].as_mut_ptr().cast();
    let bytes = MaybeUninit::<u32>::uninit().as_mut_ptr();
    let s = udp_socket_for_addr_family(addr_family)?;
    let (dest, destlen) = platform::ipaddr_to_sockaddr(target);
    let rc = unsafe {
        WSAIoctl(
            s,
            SIO_ROUTING_INTERFACE_QUERY,
            Some(std::ptr::addr_of!(dest).cast()),
            destlen,
            Some(src),
            1024,
            bytes,
            None,
            None,
        )
    };
    if rc == SOCKET_ERROR {
        eprintln!(
            "discover_local_addr: WSAIoctl failed with error: {}",
            Error::last_os_error()
        );
        return Err(TracerError::IoError(Error::last_os_error()));
    }

    /*
    NOTE The WSAIoctl call potentially returns multiple results (see
    <https://www.winsocketdotnetworkprogramming.com/winsock2programming/winsock2advancedsocketoptionioctl7h.html>),
    TBD We choose the first one arbitrarily.
     */
    platform::sockaddrptr_to_ipaddr(src.cast())
}

#[cfg(unix)]
/// Validate that we can bind to the source address.
fn validate_local_addr(addr_family: TracerAddrFamily, source_addr: IpAddr) -> TraceResult<IpAddr> {
    let socket = udp_socket_for_addr_family(addr_family)?;
    let addr = SocketAddr::new(source_addr, 0);
    match socket.bind(&SockAddr::from(addr)) {
        Ok(_) => Ok(source_addr),
        Err(_) => Err(InvalidSourceAddr(addr.ip())),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn validate_local_addr(addr_family: TracerAddrFamily, source_addr: IpAddr) -> TraceResult<IpAddr> {
    let s = udp_socket_for_addr_family(addr_family)?;
    let (addr, addrlen) = platform::ipaddr_to_sockaddr(source_addr);
    if unsafe {
        bind(
            s,
            std::ptr::addr_of!(addr).cast(),
            addrlen.try_into().unwrap(),
        )
    } == SOCKET_ERROR
    {
        return Err(TracerError::IoError(Error::last_os_error()));
    }
    Ok(source_addr)
}

#[cfg(unix)]
/// Create a socket suitable for a given address.
fn udp_socket_for_addr_family(addr_family: TracerAddrFamily) -> TraceResult<Socket> {
    Ok(match addr_family {
        TracerAddrFamily::Ipv4 => Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?,
        TracerAddrFamily::Ipv6 => Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?,
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn udp_socket_for_addr_family(addr_family: TracerAddrFamily) -> TraceResult<Socket> {
    let res = match addr_family {
        TracerAddrFamily::Ipv4 => unsafe {
            socket(
                AF_INET.0.try_into().unwrap(),
                i32::from(SOCK_DGRAM),
                IPPROTO_UDP.0,
            )
        },
        TracerAddrFamily::Ipv6 => unsafe {
            socket(
                AF_INET6.0.try_into().unwrap(),
                i32::from(SOCK_DGRAM),
                IPPROTO_UDP.0,
            )
        },
    };
    if res == INVALID_SOCKET {
        Err(TracerError::IoError(Error::last_os_error()))
    } else {
        Ok(res)
    }
}

/// Make a socket for sending raw `ICMP` packets.
fn make_icmp_send_socket(addr_family: TracerAddrFamily) -> TraceResult<Socket> {
    match addr_family {
        TracerAddrFamily::Ipv4 => platform::make_icmp_send_socket_ipv4(),
        TracerAddrFamily::Ipv6 => platform::make_icmp_send_socket_ipv6(),
    }
}

/// Make a socket for sending `UDP` packets.
fn make_udp_send_socket(addr_family: TracerAddrFamily) -> TraceResult<Socket> {
    match addr_family {
        TracerAddrFamily::Ipv4 => platform::make_udp_send_socket_ipv4(),
        TracerAddrFamily::Ipv6 => platform::make_udp_send_socket_ipv6(),
    }
}

/// Make a socket for receiving raw `ICMP` packets.
#[cfg(unix)]
fn make_recv_socket(addr_family: TracerAddrFamily) -> TraceResult<Socket> {
    match addr_family {
        TracerAddrFamily::Ipv4 => platform::make_recv_socket_ipv4(),
        TracerAddrFamily::Ipv6 => platform::make_recv_socket_ipv6(),
    }
}

#[cfg(windows)]
fn make_recv_socket(addr_family: TracerAddrFamily) -> TraceResult<(Socket, OVERLAPPED)> {
    match addr_family {
        TracerAddrFamily::Ipv4 => platform::make_recv_socket_ipv4(),
        TracerAddrFamily::Ipv6 => platform::make_recv_socket_ipv6(),
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::sync::Once;
    use windows::Win32::Networking::WinSock::{WSAStartup, WSADATA};

    static INIT: Once = Once::new();

    fn startup() {
        const WINSOCK_VERSION: u16 = 0x202; // 2.2

        INIT.call_once(|| {
            let mut wsadata = MaybeUninit::<WSADATA>::zeroed();
            unsafe {
                if WSAStartup(WINSOCK_VERSION, wsadata.as_mut_ptr()) != 0 {
                    eprintln!("WSAStartup failed: {}", Error::last_os_error());
                }
                wsadata.assume_init(); // extracts the WSADATA to ensure it gets dropped (as we don't need it ATM)
            }
        });
    }

    #[test]
    fn test_discover_local_addr() {
        startup();
        let res = discover_local_addr(
            TracerAddrFamily::Ipv4,
            IpAddr::V4("212.82.100.150".parse().unwrap()),
            443,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn test_validate_local_addr() {
        startup();
        let res = validate_local_addr(
            TracerAddrFamily::Ipv4,
            IpAddr::V4("192.168.2.2".parse().unwrap()),
        );
        assert!(res.is_ok());
    }
}
