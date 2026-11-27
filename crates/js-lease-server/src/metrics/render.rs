//! The text a scrape answers: the snapshot it read and the events the recorder counted, in
//! the Prometheus text format, every family with its `HELP` and `TYPE` lines and in a fixed
//! order, so the same state renders the same text.

use std::fmt::Write;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_jobs::JobState;
use js_registry::HostMark;
use js_sched_placer::curves::{CurveView, HostMemoryShadow, MemoryShadowView};
use js_types::HostId;

use super::{
    CALLBACK_RESULTS, FAILURE_CODES, Histogram, RETRY_OUTCOMES, Recorder, SUBMIT_PHASES,
    TABLES_FROM,
};
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

/// What a scrape read of the job tables: how many jobs are in each state, how long the
/// oldest queued job has waited (zero with none queued), and how many completion callbacks are
/// pending.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct JobsSeen {
    by_state: [u64; 9],
    oldest_queued: Duration,
    callbacks_pending: u64,
}

impl JobsSeen {
    /// Counts a job in `state`, submitted `waited` ago.
    pub(crate) fn count(&mut self, state: JobState, waited: Duration) {
        self.by_state[state_slot(state)] += 1;
        if state == JobState::Queued {
            self.oldest_queued = self.oldest_queued.max(waited);
        }
    }

    /// Records that `pending` completion callbacks are pending.
    pub(crate) fn callbacks_pending(&mut self, pending: u64) {
        self.callbacks_pending = pending;
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

/// What a scrape read: the job tables (`None` without the job service), the hosts, the curve
/// view of the placer (`None` unless it books by curve) and its memory shadow (`None` unless
/// it runs, under `shadow` and `on`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) jobs: Option<JobsSeen>,
    pub(crate) hosts: Vec<HostSeen>,
    pub(crate) curves: Option<CurveView>,
    pub(crate) memory: Option<MemoryShadowView>,
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

/// The series of `histogram` under the label `phase="{phase}"`, with no family lines.
fn phase_series(out: &mut String, name: &str, phase: &str, histogram: &Histogram) {
    let (below, count, sum_micros) = histogram.read();
    for (le, seen) in below {
        let _ = writeln!(out, "{name}_bucket{{phase=\"{phase}\",le=\"{le}\"}} {seen}");
    }
    let sum = Duration::from_micros(sum_micros);
    let _ = writeln!(
        out,
        "{name}_bucket{{phase=\"{phase}\",le=\"+Inf\"}} {count}"
    );
    let _ = writeln!(out, "{name}_sum{{phase=\"{phase}\"}} {}", seconds(sum));
    let _ = writeln!(out, "{name}_count{{phase=\"{phase}\"}} {count}");
}

fn submit_phases(out: &mut String, recorder: &Recorder) {
    let name = "js_submit_phase_seconds";
    let help = "How long each phase of each SubmitJob call took, by phase: validation (the checks of the job), get (the read of its id, for a job with one), admission (for a job not yet held), insert-checkout (of the insert, getting connections of the store's pool), insert-transaction (the rest of the insert) or note-inserted (the placer told of the job, never waiting for a pass). A call is timed in each phase it reaches, whatever the answer.";
    family(out, name, "histogram", help);
    for (phase, histogram) in SUBMIT_PHASES.iter().zip(&recorder.submit_phases) {
        phase_series(out, name, phase, histogram);
    }
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
        let pending = "js_callback_pending";
        let help = "Completion callbacks of ended jobs not yet delivered, abandoned or expired; one past its deadline counts until a try finds it expired.";
        family(&mut out, pending, "gauge", help);
        let _ = writeln!(out, "{pending} {}", jobs.callbacks_pending);
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
    let name = "js_job_retries_total";
    let help = "Attempts of jobs that ended in a way that retries and were followed by another, by how they ended: start-failed (a start failure of the host's) or host-lost.";
    family(&mut out, name, "counter", help);
    for (outcome, count) in RETRY_OUTCOMES.iter().zip(&recorder.retries) {
        let count = count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}{{outcome=\"{outcome}\"}} {count}");
    }
    let exhausted = recorder.attempts_exhausted.load(Ordering::Relaxed);
    let help =
        "Jobs that ended in a way that retries with no attempt left under their retry policy.";
    counter(&mut out, "js_job_attempts_exhausted_total", help, exhausted);
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
    let name = "js_job_tables_total";
    let help = "Passes over the queued jobs, by where each took the job tables from: read from the store, kept from the last read with no write since, kept with the jobs this server's submits recorded since added to them, or brought up to date from the store's feed with the rows that changed since the last read.";
    family(&mut out, name, "counter", help);
    for (from, count) in TABLES_FROM.iter().zip(&recorder.job_tables) {
        let count = count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}{{from=\"{from}\"}} {count}");
    }
    submit_phases(&mut out, recorder);
    callbacks(&mut out, recorder);
    refusals(&mut out, recorder);
    let expired = recorder.curve_wait_expiries.load(Ordering::Relaxed);
    let help = "Queued jobs of a curve class that waited past the wait bound (server.curve_max_wait_seconds) with no host fitting their curve, and were tested with their flat number from then on. A rise means curves book more than the hosts can give.";
    counter(&mut out, "js_curve_wait_expiries_total", help, expired);
    start_refusals(&mut out, recorder);
    acquires(&mut out, recorder);
    hosts(&mut out, seen);
    if let Some(view) = &seen.curves {
        curves(&mut out, view);
    }
    if let Some(view) = &seen.memory {
        memory_shadow(&mut out, view);
    }
    out
}

fn callbacks(out: &mut String, recorder: &Recorder) {
    let name = "js_callback_deliveries_total";
    let help = "Tries of jobs' completion callbacks whose answer was recorded, by result: delivered (a 2xx), abandoned (a 410), failed (any other status, a timeout, a refused connection, a redirect or a name that did not resolve) or refused-address (the address rule refused an address of the receiver, and nothing was sent).";
    family(out, name, "counter", help);
    for (result, count) in CALLBACK_RESULTS.iter().zip(&recorder.callbacks) {
        let count = count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}{{result=\"{result}\"}} {count}");
    }
}

fn refusals(out: &mut String, recorder: &Recorder) {
    let name = "js_placement_refusals_total";
    let help = "Refusals of a host to a queued job by the placement, by host and by the predicate that refused: cpu (CPU slack), cpu-curve (the curve hosts' slot-by-slot CPU check), predicted-memory (the load predicate's memory side), booked-memory (the flat memory guard) or cold-cap. A job is counted once for each host that refused it, in each pass.";
    family(out, name, "counter", help);
    for ((host, reason), count) in recorder.refusals() {
        let host = label(&host);
        let _ = writeln!(out, "{name}{{host=\"{host}\",reason=\"{reason}\"}} {count}");
    }
}

fn start_refusals(out: &mut String, recorder: &Recorder) {
    let name = "js_agent_start_refusals_total";
    let help = "Job starts a host's agent refused, by host and refusal code (the agent's code, or other), counted when the refusal is recorded, whether the job then goes back to the queue or fails.";
    family(out, name, "counter", help);
    for ((host, code), count) in recorder.start_refusals() {
        let host = label(&host);
        let _ = writeln!(out, "{name}{{host=\"{host}\",code=\"{code}\"}} {count}");
    }
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

/// The families of the memory shadow, written only while it runs (`--placement-curves shadow`
/// and `on`): how each placement compared with a booking that follows the classes' memory
/// curves, and each host's projected memory against its readings. Nothing in them is booked.
fn memory_shadow(out: &mut String, view: &MemoryShadowView) {
    let name = "js_memory_curve_placements_total";
    let help = "Placements of queued jobs by how a booking that follows the classes' memory curves would have placed them (shadow only, nothing is booked by it): same, sooner (an earlier walk fitted a host the flat memory booking refused) or later (the curve would not have fitted the host the flat booking chose).";
    family(out, name, "counter", help);
    let verdicts = [
        ("same", view.same),
        ("sooner", view.sooner),
        ("later", view.later),
    ];
    for (verdict, count) in verdicts {
        let _ = writeln!(out, "{name}{{verdict=\"{verdict}\"}} {count}");
    }
    let help = "The time, summed over the placements counted sooner, from the first walk at which the memory curve fitted a host the flat memory booking refused to the placement, in milliseconds.";
    counter(
        out,
        "js_memory_curve_sooner_milliseconds_total",
        help,
        view.sooner_ms,
    );
    type Of = fn(&HostMemoryShadow) -> Option<u64>;
    let per_host: [(&str, &str, &str, Of); 5] = [
        (
            "js_host_memory_projected_bytes",
            "gauge",
            "What each host's live jobs are projected to hold now by their classes' memory curves (their flat memory booking where a class has none), as the last placement walk found it, in bytes.",
            |seen| Some(seen.projected_bytes),
        ),
        (
            "js_host_memory_measured_bytes",
            "gauge",
            "The memory each host's last reading showed in use (the whole host's non-file memory when its agent reports it), in bytes.",
            |seen| seen.measured_bytes,
        ),
        (
            "js_host_memory_projection_checks_total",
            "counter",
            "Readings of each host compared with the memory projection the walk before them made of their moment.",
            |seen| Some(seen.checks),
        ),
        (
            "js_host_memory_projection_low_total",
            "counter",
            "Readings of each host that showed more memory in use than the projection the walk before them made of their moment.",
            |seen| Some(seen.too_low),
        ),
        (
            "js_host_memory_projection_short_bytes_total",
            "counter",
            "What each host's readings showed beyond the projection, summed over the readings counted in js_host_memory_projection_low_total, in bytes.",
            |seen| Some(seen.short_bytes),
        ),
    ];
    for (name, kind, help, of) in per_host {
        family(out, name, kind, help);
        for (host, seen) in &view.hosts {
            if let Some(value) = of(seen) {
                let _ = writeln!(out, "{name}{{host=\"{}\"}} {value}", label(host.as_str()));
            }
        }
    }
}

/// The families of the curve mode, written only when the placer books by curve: the classes
/// booking their curve and those booking flat, and the fullest booked slot of each curve host.
fn curves(out: &mut String, view: &CurveView) {
    let name = "js_curve_classes";
    let help = "Job classes with a profile, by how they are booked: curve (passing all four checks, booked by their CPU curve) or flat (failing a check, booked their flat number).";
    family(out, name, "gauge", help);
    let _ = writeln!(out, "{name}{{state=\"curve\"}} {}", view.curve_classes);
    let _ = writeln!(out, "{name}{{state=\"flat\"}} {}", view.flat_classes);
    let name = "js_host_cpu_booked_peak_millicores";
    let help = "The fullest slot of each curve host's booked CPU curve at the last placement pass, in thousandths of a core; js_host_cpu_booked_millicores stays the sum of flat bookings.";
    family(out, name, "gauge", help);
    for (host, peak) in &view.host_peaks {
        let _ = writeln!(out, "{name}{{host=\"{}\"}} {peak}", label(host.as_str()));
    }
}
