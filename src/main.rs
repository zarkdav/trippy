#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(
    clippy::module_name_repetitions,
    clippy::option_if_let_else,
    clippy::missing_const_for_fn,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::redundant_pub_crate
)]
#![forbid(unsafe_code)]

use crate::backend::Trace;
use crate::config::{
    validate_grace_duration, validate_max_inflight, validate_packet_size, validate_read_timeout,
    validate_report_cycles, validate_round_duration, validate_source_port, validate_ttl,
    validate_tui_refresh_rate, Mode, TraceProtocol,
};
use crate::dns::DnsResolver;
use crate::frontend::TuiConfig;
use crate::report::{
    run_report_csv, run_report_json, run_report_stream, run_report_table_markdown,
    run_report_table_pretty,
};
use clap::Parser;
use config::Args;
use parking_lot::RwLock;
use std::net::IpAddr;
use std::sync::Arc;
use std::thread;

mod backend;
mod config;
mod dns;
mod frontend;
mod report;

fn main() -> anyhow::Result<()> {
    let pid = u16::try_from(std::process::id() % u32::from(u16::MAX))?;
    let args = Args::parse();
    let hostname = args.hostname;
    let protocol = match args.protocol {
        TraceProtocol::Icmp => trippy::tracing::TracerProtocol::Icmp,
        TraceProtocol::Udp => trippy::tracing::TracerProtocol::Udp,
        TraceProtocol::Tcp => trippy::tracing::TracerProtocol::Tcp,
    };
    let first_ttl = args.first_ttl;
    let max_ttl = args.max_ttl;
    let max_inflight = args.max_inflight;
    let min_sequence = args.min_sequence;
    let read_timeout = humantime::parse_duration(&args.read_timeout)?;
    let min_round_duration = humantime::parse_duration(&args.min_round_duration)?;
    let max_round_duration = humantime::parse_duration(&args.max_round_duration)?;
    let packet_size = args.packet_size;
    let payload_pattern = args.payload_pattern;
    let grace_duration = humantime::parse_duration(&args.grace_duration)?;
    let tui_preserve_screen = args.tui_preserve_screen;
    let source_port = args.source_port.unwrap_or_else(|| pid.max(1024));
    let tui_refresh_rate = humantime::parse_duration(&args.tui_refresh_rate)?;
    let tui_address_mode = args.tui_address_mode;
    let max_addrs = args.tui_max_addresses_per_hop;
    let report_cycles = args.report_cycles;
    validate_ttl(first_ttl, max_ttl);
    validate_max_inflight(max_inflight);
    validate_read_timeout(read_timeout);
    validate_round_duration(min_round_duration, max_round_duration);
    validate_grace_duration(grace_duration);
    validate_packet_size(packet_size);
    validate_source_port(source_port);
    validate_tui_refresh_rate(tui_refresh_rate);
    validate_report_cycles(report_cycles);
    let resolver = DnsResolver::new();
    let trace_data = Arc::new(RwLock::new(Trace::default()));
    let target_addr: IpAddr = resolver.lookup(&hostname)?[0];
    let trace_identifier = pid;
    let max_rounds = match args.mode {
        Mode::Stream | Mode::Tui => None,
        Mode::Pretty | Mode::Markdown | Mode::Csv | Mode::Json => Some(report_cycles),
    };
    let tracer_config = trippy::tracing::TracerConfig::new(
        target_addr,
        protocol,
        max_rounds,
        trace_identifier,
        first_ttl,
        max_ttl,
        grace_duration,
        max_inflight,
        min_sequence,
        read_timeout,
        min_round_duration,
        max_round_duration,
        packet_size,
        payload_pattern,
        source_port,
    );

    // Run the backend on a separate thread
    {
        let trace_data = trace_data.clone();
        thread::Builder::new()
            .name("backend".into())
            .spawn(move || {
                backend::run_backend(&tracer_config, trace_data).expect("backend failed");
            })?;
    }

    match args.mode {
        Mode::Tui => {
            let tui_config = TuiConfig::new(
                target_addr,
                hostname,
                tui_refresh_rate,
                tui_preserve_screen,
                tui_address_mode,
                max_addrs,
            );
            frontend::run_frontend(&trace_data, tracer_config, tui_config)?;
        }
        Mode::Stream => run_report_stream(&hostname, target_addr, min_round_duration, &trace_data),
        Mode::Csv => run_report_csv(&hostname, target_addr, report_cycles, &trace_data),
        Mode::Json => run_report_json(&hostname, target_addr, report_cycles, &trace_data),
        Mode::Pretty => run_report_table_pretty(report_cycles, &trace_data),
        Mode::Markdown => run_report_table_markdown(report_cycles, &trace_data),
    }
    Ok(())
}
