use crate::icmp::{IcmpTracer, Probe, ProbeStatus};
use crate::{IcmpTracerConfig, MAX_HOPS};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

/// The maximum number of historic samples to keep per hop.
const MAX_SAMPLES: usize = 256;

/// The state of all hops in a trace.
#[derive(Debug, Clone)]
pub struct Trace {
    /// The TTL of the target host or the last host when responded if the target did not respond.
    highest_ttl: u8,
    /// Information about each hop.
    hops: Vec<Hop>,
}

impl Trace {
    pub fn highest_ttl(&self) -> u8 {
        self.highest_ttl
    }

    /// Information about each hop.
    pub fn hops(&self) -> &[Hop] {
        &self.hops
    }

    /// Return the target `Hop`.
    pub fn target_hop(&self) -> &Hop {
        if self.highest_ttl > 0 {
            &self.hops[usize::from(self.highest_ttl) - 1]
        } else {
            &self.hops[0]
        }
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            highest_ttl: 0,
            hops: (0..MAX_HOPS).map(|_| Hop::default()).collect(),
        }
    }
}

/// Information about a single `Hop` within a `Trace`.
#[derive(Debug, Clone)]
pub struct Hop {
    ttl: u8,
    addrs: HashSet<IpAddr>,
    total_sent: usize,
    total_recv: usize,
    total_time: Duration,
    last: Option<Duration>,
    best: Option<Duration>,
    worst: Option<Duration>,
    mean: f64,
    m2: f64,
    samples: Vec<Duration>,
}

impl Hop {
    /// The time-to-live of this hop.
    pub fn ttl(&self) -> u8 {
        self.ttl
    }

    /// The set of addresses that have responded for this time-to-live.
    pub fn addrs(&self) -> impl Iterator<Item = &IpAddr> {
        self.addrs.iter()
    }

    /// The number of unique address observed for this time-to-live.
    pub fn addr_count(&self) -> usize {
        self.addrs.len()
    }

    /// The total number of probes sent.
    pub fn total_sent(&self) -> usize {
        self.total_sent
    }

    /// The total number of probes responses received.
    pub fn total_recv(&self) -> usize {
        self.total_recv
    }

    /// The duration of the last probe.
    pub fn last_ms(&self) -> Option<f64> {
        self.last.map(|last| last.as_secs_f64() * 1000_f64)
    }

    /// The duration of the best probe observed.
    pub fn best_ms(&self) -> Option<f64> {
        self.best.map(|last| last.as_secs_f64() * 1000_f64)
    }

    /// The duration of the worst probe observed.
    pub fn worst_ms(&self) -> Option<f64> {
        self.worst.map(|last| last.as_secs_f64() * 1000_f64)
    }

    /// The average duration of all probes.
    pub fn avg_ms(&self) -> f64 {
        (self.total_time.as_secs_f64() * 1000_f64) / self.total_recv as f64
    }

    /// The standard deviation of all probes.
    pub fn stddev_ms(&self) -> f64 {
        if self.total_recv > 1 {
            (self.m2 / (self.total_recv - 1) as f64).sqrt()
        } else {
            0_f64
        }
    }

    /// The last N samples.
    pub fn samples(&self) -> &[Duration] {
        &self.samples
    }
}

impl Default for Hop {
    fn default() -> Self {
        Self {
            ttl: 0,
            addrs: HashSet::default(),
            total_sent: 0,
            total_recv: 0,
            total_time: Duration::default(),
            last: None,
            best: None,
            worst: None,
            mean: 0f64,
            m2: 0f64,
            samples: Vec::default(),
        }
    }
}

/// Run the tracing backend.
///
/// Note that this implementation blocks the tracer on the `RwLock` and so any delays in the the TUI will delay the
/// next round of the started.
///
/// Note that currently each `Probe` is published individually at the end of a round and so the lock is taken multiple
/// times per round.
pub fn run_backend(
    config: &IcmpTracerConfig,
    trace_data: Arc<RwLock<Trace>>,
) -> anyhow::Result<()> {
    let tracer = IcmpTracer::new(config, move |probe| {
        update_trace_data(*probe, &mut trace_data.write());
    });
    Ok(tracer.trace()?)
}

fn update_trace_data(probe: Probe, trace_data: &mut Trace) {
    let index = usize::from(probe.ttl.0) - 1;
    trace_data.highest_ttl = trace_data.highest_ttl.max(probe.ttl.0);
    match probe.status {
        ProbeStatus::Complete => {
            let hop = &mut trace_data.hops[index];
            hop.ttl = probe.ttl.0;
            hop.total_sent += 1;
            hop.total_recv += 1;
            let dur = probe.duration();
            let dur_ms = dur.as_secs_f64() * 1000_f64;
            hop.total_time += dur;
            hop.last = Some(dur);
            hop.samples.insert(0, dur);
            hop.best = hop.best.map_or(Some(dur), |d| Some(d.min(dur)));
            hop.worst = hop.worst.map_or(Some(dur), |d| Some(d.max(dur)));
            hop.mean += (dur_ms - hop.mean) / hop.total_recv as f64;
            hop.m2 += (dur_ms - hop.mean) * (dur_ms - hop.mean);
            if hop.samples.len() > MAX_SAMPLES {
                hop.samples.pop();
            }
            let host = probe.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            hop.addrs.insert(host);
        }
        ProbeStatus::Awaited => {
            trace_data.hops[index].total_sent += 1;
            trace_data.hops[index].ttl = probe.ttl.0;
            trace_data.hops[index]
                .samples
                .insert(0, Duration::default());
            if trace_data.hops[index].samples.len() > MAX_SAMPLES {
                trace_data.hops[index].samples.pop();
            }
        }
        ProbeStatus::NotSent => {}
    }
}
