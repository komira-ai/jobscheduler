//! The families of the placement by load: what admission has done, how each host is charged,
//! what each host's agent reported besides its load, how the runs freed while serving did
//! against what they booked, and how their checks against the classes' profiles came out. The
//! figures of the placer come from one [`Telemetry`] read at the scrape; with none (the server
//! has no placer) each of those families is written as its `HELP` and `TYPE` lines alone. The
//! families of the registry's hosts are written from the hosts read.

use std::fmt::{Display, Write};
use std::time::Duration;

use js_sched_placer::telemetry::{
    CheckCounts, FIT_BOUNDS_PERMILLE, HostLoad, RatioHistogram, Telemetry,
};

use super::render::{HostSeen, Snapshot, family, label, seconds};

/// The statuses of a check, in the order they are written, as the metric spells them.
const STATUSES: [&str; 6] = [
    "learned",
    "not-learned",
    "no-series",
    "partial",
    "disagrees",
    "not-a-success",
];

/// A gauge of each host the placer charges: its name, its help, and how it is read from the
/// host's load (`None` writes no series for the host).
struct PerLoad {
    name: &'static str,
    help: &'static str,
    of: fn(&HostLoad) -> Option<u64>,
}

/// The gauges of how each host is charged, in the order they are written.
const PER_LOAD: [PerLoad; 9] = [
    PerLoad {
        name: "js_host_cpu_capacity_millicores",
        help: "The CPU the scheduler books on each host it has a worker for, in thousandths of a core.",
        of: |host| Some(host.cpu_capacity_millis),
    },
    PerLoad {
        name: "js_host_memory_capacity_bytes",
        help: "The memory the scheduler books on each host it has a worker for: the host's memory cut to the share it may book (memory_schedulable_percent).",
        of: |host| Some(host.memory_capacity_bytes),
    },
    PerLoad {
        name: "js_host_cpu_measured_millicores",
        help: "The cores demanded on each host over the last two windows (the larger), in thousandths of a core: its busy cores plus those times the share of the window in which a task waited for a CPU. No series before the host's first window.",
        of: |host| host.load.measured_cpu_millis.map(u64::from),
    },
    PerLoad {
        name: "js_host_memory_working_set_bytes",
        help: "The working set of each host's newest reading, as the agent reported it. No series for a host with no reading.",
        of: |host| host.load.working_set_bytes,
    },
    PerLoad {
        name: "js_host_cpu_predicted_millicores",
        help: "The CPU predicted for the action leases of each host, in thousandths of a core.",
        of: |host| Some(host.load.predicted_cpu_millis),
    },
    PerLoad {
        name: "js_host_cpu_uncovered_millicores",
        help: "Of the CPU predicted on each host, what no reading has covered yet, in thousandths of a core.",
        of: |host| Some(host.load.uncovered_cpu_millis),
    },
    PerLoad {
        name: "js_host_memory_predicted_bytes",
        help: "The memory predicted for the action leases of each host.",
        of: |host| Some(host.load.predicted_memory_bytes),
    },
    PerLoad {
        name: "js_host_memory_uncovered_bytes",
        help: "Of the memory predicted on each host, what no reading has covered yet.",
        of: |host| Some(host.load.uncovered_memory_bytes),
    },
    PerLoad {
        name: "js_host_load_fallback",
        help: "1 for a host charged by prediction alone, with no usable measurement, and 0 for a host charged by its measurement.",
        of: |host| Some(u64::from(host.load.fallback)),
    },
];

/// The families of the placement and of the hosts' reports, after the hosts' own gauges.
pub(super) fn write(out: &mut String, seen: &Snapshot) {
    let telemetry = seen.telemetry.as_ref();
    admission(out, telemetry);
    loads(out, telemetry);
    reports(out, &seen.hosts);
    fit(out, telemetry);
    checks(out, telemetry);
}

/// `value` written as `name`'s one series, when there is one, under its family lines.
fn scalar(out: &mut String, name: &str, kind: &str, help: &str, value: Option<u64>) {
    family(out, name, kind, help);
    if let Some(value) = value {
        let _ = writeln!(out, "{name} {value}");
    }
}

/// The series of `name`, each its label text (what stands between the braces) and its value,
/// under the family lines.
fn series<V: Display>(
    out: &mut String,
    (name, kind, help): (&str, &str, &str),
    rows: impl IntoIterator<Item = (String, V)>,
) {
    family(out, name, kind, help);
    for (labels, value) in rows {
        let _ = writeln!(out, "{name}{{{labels}}} {value}");
    }
}

/// The label text of one label.
fn pair(name: &str, value: &str) -> String {
    format!("{name}=\"{}\"", label(value))
}

/// The counters of admission, which count since the scheduler was built.
fn admission(out: &mut String, telemetry: Option<&Telemetry>) {
    let counters = telemetry.map(|telemetry| telemetry.counters);
    let counter = "counter";
    let help = "Leases the load predicate admitted.";
    let value = counters.map(|counters| counters.admitted);
    scalar(out, "js_placement_admissions_total", counter, help, value);
    let help = "Of the admissions, those on a host charged by prediction alone.";
    let value = counters.map(|counters| counters.admitted_fallback);
    scalar(
        out,
        "js_placement_admissions_fallback_total",
        counter,
        help,
        value,
    );
    let help = "Of the admissions, the cold ones.";
    let value = counters.map(|counters| counters.admitted_cold);
    scalar(
        out,
        "js_placement_admissions_cold_total",
        counter,
        help,
        value,
    );
    let help = "Placements chosen by the score among two or more hosts.";
    let value = counters.map(|counters| counters.scored);
    scalar(out, "js_placement_scored_total", counter, help, value);
    let help = "Of the scored placements, by the key that chose them: slack (key 1: a host predicted above its capacity, so the slack zone was used), distrusted (key 3: a distrusted host), concentrated (key 4: a host concentrated on the lease's key) or tie-break (the best two were equal in keys 1 to 5 and a tie-break decided). One placement can count under more than one key.";
    let keys = counters.into_iter().flat_map(|counters| {
        [
            ("slack", counters.chose_slack),
            ("distrusted", counters.chose_distrusted),
            ("concentrated", counters.chose_concentrated),
            ("tie-break", counters.chose_by_tie_break),
        ]
    });
    let rows = keys.map(|(key, count)| (pair("key", key), count));
    series(out, ("js_placement_decided_total", counter, help), rows);
    let help = "Slot-resource evaluations of the score: 64 for each host scored for each request. None for a host the predicate refused or that was excluded before it, and for a request with one host left.";
    let value = counters.map(|counters| counters.score_evaluations);
    scalar(
        out,
        "js_placement_score_evaluations_total",
        counter,
        help,
        value,
    );
    let help = "Load readings of the hosts, by result: accepted, or dropped (out of order, a counter or the clock going back, or implausible).";
    let results = counters.into_iter().flat_map(|counters| {
        [
            ("accepted", counters.readings_accepted),
            ("dropped", counters.readings_dropped),
        ]
    });
    let rows = results.map(|(result, count)| (pair("result", result), count));
    series(out, ("js_load_readings_total", counter, help), rows);
    let help = "Accepted readings that gave a window: the windows in which the floor may bind.";
    let value = counters.map(|counters| counters.floor_windows);
    scalar(out, "js_load_floor_windows_total", counter, help, value);
    let help =
        "Of those windows, those in which the floor on the demand was above the measurement.";
    let value = counters.map(|counters| counters.floor_binds);
    scalar(out, "js_load_floor_binds_total", counter, help, value);
}

/// The gauges of how each host that is a worker of the scheduler is charged.
fn loads(out: &mut String, telemetry: Option<&Telemetry>) {
    for PerLoad { name, help, of } in PER_LOAD {
        let hosts = telemetry.into_iter().flat_map(|telemetry| &telemetry.hosts);
        let rows = hosts
            .filter_map(|(host, load)| of(load).map(|value| (pair("host", host.as_str()), value)));
        series(out, (name, "gauge", help), rows);
    }
}

/// What the hosts' agents reported besides what the scheduler charges, and how many hosts
/// stand empty.
fn reports(out: &mut String, hosts: &[HostSeen]) {
    let name = "js_host_cpu_pressure_some_seconds_total";
    let help = "The time some task of each host waited for a CPU, from the host's /proc/pressure/cpu, in seconds, counted since the host booted. 0 when the agent does not measure it. No series for a host with no load reported.";
    let rows = hosts.iter().filter_map(|host| {
        let usec = host.cpu_pressure_some_usec?;
        let seconds = seconds(Duration::from_micros(usec));
        Some((pair("host", host.host.as_str()), seconds))
    });
    series(out, (name, "counter", help), rows);
    let name = "js_host_actions_oom_events_total";
    let help = "How many times the OOM killer ran for the limit of each host's actions control group, as of its newest reading (a kill at a lease's own limit is not counted). No series for a host that reported none.";
    let rows = hosts.iter().filter_map(|host| {
        let events = host.actions_oom_events?;
        Some((pair("host", host.host.as_str()), events))
    });
    series(out, (name, "counter", help), rows);
    let name = "js_hosts_kept_empty";
    let help = "Live hosts with no mark that have nothing booked (no CPU and no memory) at the scrape: the hosts the packing keeps empty. A lost host and one waiting to say hello again are not counted.";
    let empty = hosts.iter().filter(|host| {
        host.live && host.mark.is_none() && host.cpu.booked == 0 && host.memory.booked == 0
    });
    scalar(out, name, "gauge", help, u64::try_from(empty.count()).ok());
}

/// `permille` thousandths written as a decimal number, exactly: its whole part, then its
/// fraction without trailing zeros.
fn decimal(permille: u64) -> String {
    let text = format!("{}.{:03}", permille / 1000, permille % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// One histogram of a ratio, its buckets cumulative; with none, its family lines alone.
fn ratio_histogram(out: &mut String, name: &str, help: &str, held: Option<&RatioHistogram>) {
    family(out, name, "histogram", help);
    if let Some(held) = held {
        let mut below = 0;
        for (bound, count) in FIT_BOUNDS_PERMILLE.iter().zip(&held.counts) {
            below += count;
            let le = decimal(*bound);
            let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {below}");
        }
        let count = held.count();
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_sum {}", decimal(held.sum_permille));
        let _ = writeln!(out, "{name}_count {count}");
    }
}

/// How the runs freed while serving did against what they booked.
fn fit(out: &mut String, telemetry: Option<&Telemetry>) {
    let fit = telemetry.map(|telemetry| &telemetry.fit);
    let help = "A successful run's mean CPU over its booked CPU: its CPU time over its wall time, over the cores it booked, for runs freed while the server was serving. Every such run counts by what it booked, cold and explicit bookings and runs held at a ceiling included. A run under a second, one that used no CPU time and one that booked none have none.";
    let held = fit.map(|fit| &fit.cpu_mean_over_booked);
    ratio_histogram(out, "js_run_cpu_mean_over_booked", help, held);
    let help = "A successful run's CPU peak over its mean CPU: the most cores its group used over the agent's peak window (five seconds), over its mean. A run shorter than the window reports its mean as its peak, so it reads 1. A run with no reported peak or no mean has none.";
    let held = fit.map(|fit| &fit.cpu_peak_over_mean);
    ratio_histogram(out, "js_run_cpu_peak_over_mean", help, held);
    let help = "A successful run's peak memory over its booked memory: the peak of its non-file memory series when its host recorded a whole one, else its group's memory.peak (which counts page cache). A run with no measured peak or no booked memory has none.";
    let held = fit.map(|fit| &fit.memory_peak_over_booked);
    ratio_histogram(out, "js_run_memory_peak_over_booked", help, held);
    let help =
        "Runs freed while the server was serving that ended killed at their own memory limit.";
    let kills = fit.map(|fit| fit.memory_kills);
    scalar(out, "js_run_memory_kills_total", "counter", help, kills);
}

/// The counts of `checks` in the order of [`STATUSES`].
fn by_status(checks: CheckCounts) -> [u64; 6] {
    [
        checks.learned,
        checks.not_learned,
        checks.no_series,
        checks.partial,
        checks.disagrees,
        checks.not_a_success,
    ]
}

/// The checks of the runs freed while serving against their classes' profiles.
fn checks(out: &mut String, telemetry: Option<&Telemetry>) {
    let both = telemetry.into_iter().flat_map(|telemetry| {
        [
            ("cpu", telemetry.checks.cpu),
            ("memory", telemetry.checks.memory),
        ]
    });
    let help = "Runs freed while the server was serving, by the resource of the profile that checked them and the status of the check: learned, not-learned (the profile chose not to learn the run), no-series (the host recorded no series of the resource), partial (the series misses the start of the run), disagrees (the CPU series disagrees with the run's CPU time) or not-a-success (the run was no success, which the profile never checks).";
    let statuses = both.clone().flat_map(|(resource, counts)| {
        STATUSES
            .iter()
            .zip(by_status(counts))
            .map(move |(status, count)| {
                let labels = format!("{},{}", pair("resource", resource), pair("status", status));
                (labels, count)
            })
    });
    series(out, ("js_curve_checks_total", "counter", help), statuses);
    let help = "Of the checked runs, those that overran the peak their class's profile predicted: the profile held enough runs to score a prediction and the run's peak was more than a quarter above it. Counted besides the run's status.";
    let overruns = both.map(|(resource, counts)| (pair("resource", resource), counts.overran));
    series(out, ("js_curve_overruns_total", "counter", help), overruns);
}
