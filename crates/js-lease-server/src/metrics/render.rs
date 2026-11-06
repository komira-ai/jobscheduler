//! The text a scrape answers: the snapshot it read and the events the recorder counted, in
//! the Prometheus text format, every family with its `HELP` and `TYPE` lines and in a fixed
//! order, so the same state renders the same text.

use std::fmt::Write;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_jobs::JobState;
use js_registry::HostMark;
use js_types::HostId;

use super::{FAILURE_CODES, Histogram, Recorder};
use crate::hosts::mark_text;

/// The job states, in the order they are written. A job that failed to start is counted under
/// `failed-to-start` whatever its code; `js_job_start_failures_total` has the codes.
const STATES: [&str; 9] = [
    "queued",
    "placed",
    "starting",
    "running",
    "succeeded",
    "failed",
    "cancelled",
    "lost",
    "failed-to-start",
];

fn state_slot(state: JobState) -> usize {
    match state {
        JobState::Queued => 0,
        JobState::Placed => 1,
        JobState::Starting => 2,
        JobState::Running => 3,
        JobState::Succeeded => 4,
        JobState::Failed => 5,
        JobState::Cancelled => 6,
        JobState::Lost => 7,
        JobState::FailedToStart(_) => 8,
    }
}

/// What a scrape read of the job tables: how many jobs are in each state, and how long the
/// oldest queued job has waited (zero with none queued).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct JobsSeen {
    by_state: [u64; 9],
    oldest_queued: Duration,
}

impl JobsSeen {
    /// Counts a job in `state`, submitted `waited` ago.
    pub(crate) fn count(&mut self, state: JobState, waited: Duration) {
        self.by_state[state_slot(state)] += 1;
        if state == JobState::Queued {
            self.oldest_queued = self.oldest_queued.max(waited);
        }
    }
}

/// Booked and free, of one resource of a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Split {
    pub(crate) booked: u64,
    pub(crate) free: u64,
}

impl Split {
    /// `booked` of `total`; none is free once all is booked (a CPU slack books past it).
    pub(crate) fn of(total: u64, booked: u64) -> Self {
        Self {
            booked,
            free: total.saturating_sub(booked),
        }
    }
}

/// What a scrape read of one host.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HostSeen {
    pub(crate) host: HostId,
    /// In thousandths of a core.
    pub(crate) cpu: Split,
    /// In bytes.
    pub(crate) memory: Split,
    pub(crate) mark: Option<HostMark>,
}

/// What a scrape read: the job tables (`None` without the job service) and the hosts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) jobs: Option<JobsSeen>,
    pub(crate) hosts: Vec<HostSeen>,
}

/// `value` as a label value: a backslash, a double quote and a line feed escaped.
pub(crate) fn label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// A duration in seconds, as the text writes a number.
fn seconds(value: Duration) -> String {
    value.as_secs_f64().to_string()
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn histogram(out: &mut String, name: &str, help: &str, histogram: &Histogram) {
    family(out, name, "histogram", help);
    let (below, count, sum_micros) = histogram.read();
    for (le, seen) in below {
        let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {seen}");
    }
    let sum = Duration::from_micros(sum_micros);
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
    let _ = writeln!(out, "{name}_sum {}", seconds(sum));
    let _ = writeln!(out, "{name}_count {count}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    family(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

/// The text of a scrape that read `seen`, with the events `recorder` counted.
pub(crate) fn render(seen: &Snapshot, recorder: &Recorder) -> String {
    let mut out = String::new();
    if let Some(jobs) = &seen.jobs {
        family(
            &mut out,
            "js_jobs",
            "gauge",
            "Jobs in the job tables, by state.",
        );
        for (state, count) in STATES.iter().zip(jobs.by_state) {
            let _ = writeln!(out, "js_jobs{{state=\"{state}\"}} {count}");
        }
        let oldest = "js_queue_oldest_age_seconds";
        let help =
            "How long the oldest queued job has waited since its submit; 0 with none queued.";
        family(&mut out, oldest, "gauge", help);
        let _ = writeln!(out, "{oldest} {}", seconds(jobs.oldest_queued));
    }
    let placed = recorder.placed.load(Ordering::Relaxed);
    let help = "Queued jobs placed on a host.";
    counter(&mut out, "js_job_placements_total", help, placed);
    let help = "How long each placed job waited in the queue, from its submit to its placement.";
    histogram(
        &mut out,
        "js_queue_wait_seconds",
        help,
        &recorder.queue_wait,
    );
    let name = "js_job_start_failures_total";
    let help = "Jobs that failed to start, by failure code.";
    family(&mut out, name, "counter", help);
    for (code, count) in FAILURE_CODES.iter().zip(&recorder.start_failures) {
        let count = count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}{{code=\"{code}\"}} {count}");
    }
    let lost = recorder.lost.load(Ordering::Relaxed);
    let help = "Jobs lost with their host or container.";
    counter(&mut out, "js_jobs_lost_total", help, lost);
    let failed = recorder.store_write_failures.load(Ordering::Relaxed);
    let help = "Passes over the job tables the server makes itself (the job pass, a placement, a heartbeat's job listing) that failed, so their write did not commit.";
    counter(&mut out, "js_store_write_failures_total", help, failed);
    let help = "How long each job pass of the tick took.";
    histogram(
        &mut out,
        "js_job_pass_duration_seconds",
        help,
        &recorder.job_pass,
    );
    acquires(&mut out, recorder);
    hosts(&mut out, seen);
    out
}

fn acquires(out: &mut String, recorder: &Recorder) {
    let pools = recorder.acquires();
    let asked = "js_acquires_total";
    let help = "Launches the scaler ran, by pool, whatever their answer.";
    family(out, asked, "counter", help);
    for (pool, seen) in &pools {
        let _ = writeln!(out, "{asked}{{pool=\"{}\"}} {}", label(pool), seen.asked);
    }
    let over = "js_acquire_failovers_total";
    let help = "Launches refused for capacity or quota, after which the scaler tries another offer, by pool.";
    family(out, over, "counter", help);
    for (pool, seen) in &pools {
        let _ = writeln!(
            out,
            "{over}{{pool=\"{}\"}} {}",
            label(pool),
            seen.failed_over
        );
    }
}

/// A gauge of each host: its name, its help, and how it is read from a host.
struct PerHost {
    name: &'static str,
    help: &'static str,
    of: fn(&HostSeen) -> u64,
}

/// The gauges of each host's CPU and memory, booked and free.
const PER_HOST: [PerHost; 4] = [
    PerHost {
        name: "js_host_cpu_booked_millicores",
        help: "CPU booked on each host, in thousandths of a core.",
        of: |host| host.cpu.booked,
    },
    PerHost {
        name: "js_host_cpu_free_millicores",
        help: "CPU free on each host, in thousandths of a core.",
        of: |host| host.cpu.free,
    },
    PerHost {
        name: "js_host_memory_booked_bytes",
        help: "Memory booked on each host.",
        of: |host| host.memory.booked,
    },
    PerHost {
        name: "js_host_memory_free_bytes",
        help: "Memory free on each host, of what it may book.",
        of: |host| host.memory.free,
    },
];

fn hosts(out: &mut String, seen: &Snapshot) {
    for PerHost { name, help, of } in PER_HOST {
        family(out, name, "gauge", help);
        for host in &seen.hosts {
            let host_label = label(host.host.as_str());
            let _ = writeln!(out, "{name}{{host=\"{host_label}\"}} {}", of(host));
        }
    }
    let name = "js_host_whole_mark";
    let help =
        "1 for each host set aside whole, by its mark: reserved, held, draining or releasing.";
    family(out, name, "gauge", help);
    let marked = seen
        .hosts
        .iter()
        .filter_map(|host| host.mark.map(|mark| (host, mark)));
    for (host, mark) in marked {
        let (host, state) = (label(host.host.as_str()), mark_text(mark));
        let _ = writeln!(out, "{name}{{host=\"{host}\",state=\"{state}\"}} 1");
    }
}
