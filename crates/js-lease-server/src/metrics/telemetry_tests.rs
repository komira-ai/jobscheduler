//! The families of the placement by load in a scrape: each written from a telemetry built by
//! hand, and the read of the placer's telemetry by a scrape.

use std::num::NonZeroUsize;

use js_registry::HostMark;
use js_sched::{LoadCounters, LoadView};
use js_sched_placer::telemetry::{
    CheckCounts, CurveChecks, HostLoad, RatioHistogram, RunFit, Telemetry,
};
use js_types::HostId;

use super::render::{HostSeen, Snapshot, Split, render};
use super::scrape::snapshot;
use super::tests::shared_over;
use super::*;
use crate::agent_api::job_rig::clocked_rig;
use crate::job_expiry::JobLimits;

/// The first line of the placement families in a scrape.
const FIRST: &str = "# HELP js_placement_admissions_total ";

/// A host as a scrape reads it: `booked` of CPU and memory of 8000 m and 1 GiB.
fn host(
    name: &str,
    (cpu, memory): (u64, u64),
    mark: Option<HostMark>,
    (cpu_pressure_some_usec, actions_oom_events): (Option<u64>, Option<u64>),
) -> HostSeen {
    HostSeen {
        host: HostId::new(name).unwrap(),
        cpu: Split::of(8000, cpu),
        memory: Split::of(1 << 30, memory),
        mark,
        live: true,
        cpu_pressure_some_usec,
        actions_oom_events,
    }
}

/// How a host is charged: by prediction alone (`fallback`, nothing predicted) or by a
/// measurement, with 3 cores and 6 GiB predicted of which half a core and 1 GiB are uncovered.
fn load_view(fallback: bool, measured: Option<u32>, working_set: Option<u64>) -> LoadView {
    LoadView {
        fallback,
        measured_cpu_millis: measured,
        working_set_bytes: working_set,
        left_cpu_millis: 5000,
        left_memory_bytes: 1 << 30,
        predicted_cpu_millis: if fallback { 0 } else { 3000 },
        uncovered_cpu_millis: if fallback { 0 } else { 500 },
        predicted_memory_bytes: if fallback { 0 } else { 6_442_450_944 },
        uncovered_memory_bytes: if fallback { 0 } else { 1_073_741_824 },
        vm_cpu_millis: 0,
        vm_memory_bytes: 0,
        cold_this_window: 0,
    }
}

/// The checks of a resource: the six statuses in order, then the overruns.
fn checks_of(by_status: [u64; 6], overran: u64) -> CheckCounts {
    let [
        learned,
        not_learned,
        no_series,
        partial,
        disagrees,
        not_a_success,
    ] = by_status;
    CheckCounts {
        learned,
        not_learned,
        no_series,
        partial,
        disagrees,
        not_a_success,
        overran,
    }
}

/// A telemetry with a figure of each kind: `host-a` charged by its measurement, `host-b` by
/// prediction alone with no measurement.
pub(super) fn known() -> Telemetry {
    let counters = LoadCounters {
        admitted: 7,
        admitted_fallback: 3,
        admitted_cold: 2,
        scored: 5,
        chose_slack: 1,
        chose_distrusted: 2,
        chose_concentrated: 3,
        chose_by_tie_break: 4,
        score_evaluations: 640,
        readings_accepted: 20,
        readings_dropped: 2,
        floor_windows: 18,
        floor_binds: 6,
        ..LoadCounters::default()
    };
    let charged = |load| HostLoad {
        cpu_capacity_millis: 8000,
        memory_capacity_bytes: 17_179_869_184,
        load,
    };
    let hosts = [
        (
            HostId::new("host-a").unwrap(),
            charged(load_view(false, Some(2500), Some(4_294_967_296))),
        ),
        (
            HostId::new("host-b").unwrap(),
            charged(load_view(true, None, None)),
        ),
    ];
    let fit = RunFit {
        cpu_mean_over_booked: RatioHistogram {
            counts: [0, 1, 0, 2, 0, 0, 0, 0, 0, 1],
            sum_permille: 8400,
        },
        cpu_peak_over_mean: RatioHistogram {
            counts: [0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            sum_permille: 1750,
        },
        memory_peak_over_booked: RatioHistogram::default(),
        memory_kills: 2,
    };
    Telemetry {
        counters,
        hosts: hosts.into_iter().collect(),
        fit,
        checks: CurveChecks {
            cpu: checks_of([5, 1, 2, 0, 3, 4], 2),
            memory: checks_of([6, 0, 0, 1, 0, 4], 1),
        },
    }
}

/// The text of a scrape that read `hosts` and `telemetry`, from its first placement family.
fn section(hosts: Vec<HostSeen>, telemetry: Option<Telemetry>) -> String {
    let seen = Snapshot {
        jobs: None,
        hosts,
        curves: None,
        telemetry,
        memory: None,
    };
    let text = render(&seen, &Recorder::default());
    let at = text.find(FIRST).unwrap();
    text[at..].to_owned()
}

/// Asserts the whole text of the placement families, by exact equality, for a telemetry built
/// by hand and five hosts: every family's HELP and TYPE, the counters of admission, the
/// gauges of the two hosts charged (the measured CPU and the working set only for the one
/// that has them), the pressure and the OOM count of the hosts that reported them (a
/// pressure of 0 written as 0), the two ratio histograms with figures and the empty one, the
/// kills, and every status of both resources. It catches a family missing, renamed or out of
/// order, a figure read from another field, a bucket not cumulative and a label left out.
#[test]
fn a_telemetry_is_written_as_every_placement_family() {
    let hosts = vec![
        host("host-a", (0, 0), None, (Some(4_500_000), Some(3))),
        host("host-b", (100, 0), None, (Some(0), Some(0))),
        host("host-c", (0, 1), None, (None, None)),
        host("host-d", (0, 0), Some(HostMark::Draining), (None, None)),
        host("host-e", (0, 0), None, (None, None)),
    ];
    let text = section(hosts, Some(known()));
    assert_eq!(text, include_str!("placement.prom"), "now:\n{text}");
}

/// Asserts that a scrape with no placer writes each of the 27 placement families as its HELP
/// and TYPE lines alone, except the count of hosts kept empty, which reads the hosts and is
/// 0 for none. It catches a family left out or a series written from nothing.
#[test]
fn with_no_telemetry_each_placement_family_is_its_header_alone() {
    let text = section(Vec::new(), None);
    let types = text.lines().filter(|line| line.starts_with("# TYPE "));
    assert_eq!(types.count(), 27, "{text}");
    let series: Vec<&str> = text.lines().filter(|line| !line.starts_with('#')).collect();
    assert_eq!(series, ["js_hosts_kept_empty 0"]);
}

/// Asserts the bucket of each observation of a ratio and the cumulative text: 0 and 250 in
/// the first, 251 in the second, 5000 in the ninth, 5001 only in `+Inf`; the buckets add up,
/// the sum is 10.502 and the count 5. It catches a bucket that is not cumulative and a bound
/// that is exclusive.
#[test]
fn a_ratio_histogram_is_written_cumulative_with_its_sum_and_count() {
    let mut held = RatioHistogram::default();
    [0, 250, 251, 5000, 5001]
        .into_iter()
        .for_each(|value| held.observe(value));
    let telemetry = Telemetry {
        fit: RunFit {
            memory_peak_over_booked: held,
            ..RunFit::default()
        },
        ..Telemetry::default()
    };
    let text = section(Vec::new(), Some(telemetry));
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("js_run_memory_peak_over_booked"))
        .collect();
    let name = "js_run_memory_peak_over_booked";
    let buckets = [
        ("0.25", 2),
        ("0.5", 3),
        ("0.75", 3),
        ("1", 3),
        ("1.25", 3),
        ("1.5", 3),
        ("2", 3),
        ("3", 3),
        ("5", 4),
        ("+Inf", 5),
    ];
    let mut expected: Vec<String> = buckets
        .iter()
        .map(|(le, count)| format!("{name}_bucket{{le=\"{le}\"}} {count}"))
        .collect();
    expected.push(format!("{name}_sum 10.502"));
    expected.push(format!("{name}_count 5"));
    assert_eq!(lines, expected);
}

/// Asserts how the sum of a histogram, kept in thousandths, is written: 0 as `0`, 1000 as
/// `1`, 2500 as `2.5`, 1 as `0.001`, 1250 as `1.25` and 12000 as `12`. It catches a float
/// that rounds, and a fraction written with trailing zeros.
#[test]
fn the_sum_of_a_ratio_histogram_is_written_as_an_exact_decimal() {
    let cases = [
        (0, "0"),
        (1000, "1"),
        (2500, "2.5"),
        (1, "0.001"),
        (1250, "1.25"),
        (12_000, "12"),
    ];
    for (permille, text) in cases {
        let telemetry = Telemetry {
            fit: RunFit {
                cpu_mean_over_booked: RatioHistogram {
                    counts: [0; 10],
                    sum_permille: permille,
                },
                ..RunFit::default()
            },
            ..Telemetry::default()
        };
        let written = section(Vec::new(), Some(telemetry));
        let line = format!("\njs_run_cpu_mean_over_booked_sum {text}\n");
        assert!(written.contains(&line), "{permille}: {written}");
    }
}

/// Asserts that a host with no measurement has no `js_host_cpu_measured_millicores` or
/// `js_host_memory_working_set_bytes` series, that the other gauges of the host are still
/// written, and that `js_host_load_fallback` reads 1 for it and 0 for a host charged by its
/// measurement. It catches a missing measurement written as 0.
#[test]
fn a_host_with_no_measurement_has_no_measured_series() {
    let text = section(Vec::new(), Some(known()));
    let lines = |family: &str| -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with(family))
            .collect()
    };
    assert_eq!(
        lines("js_host_cpu_measured_millicores{"),
        ["js_host_cpu_measured_millicores{host=\"host-a\"} 2500"]
    );
    assert_eq!(
        lines("js_host_memory_working_set_bytes{"),
        ["js_host_memory_working_set_bytes{host=\"host-a\"} 4294967296"]
    );
    assert_eq!(
        lines("js_host_cpu_predicted_millicores{"),
        [
            "js_host_cpu_predicted_millicores{host=\"host-a\"} 3000",
            "js_host_cpu_predicted_millicores{host=\"host-b\"} 0"
        ]
    );
    assert_eq!(
        lines("js_host_load_fallback{"),
        [
            "js_host_load_fallback{host=\"host-a\"} 0",
            "js_host_load_fallback{host=\"host-b\"} 1"
        ]
    );
}

/// Asserts that `js_hosts_kept_empty` counts the live hosts with no mark and nothing booked,
/// and no other: of six hosts, one has CPU booked, one memory booked, one a mark and one is
/// not live, so two are counted. It catches a count of marked or lost hosts, and one that
/// looks at CPU alone.
#[test]
fn the_hosts_kept_empty_are_the_live_ones_with_no_mark_and_nothing_booked() {
    let lost = HostSeen {
        live: false,
        ..host("host-f", (0, 0), None, (None, None))
    };
    let hosts = vec![
        host("host-a", (0, 0), None, (None, None)),
        host("host-b", (100, 0), None, (None, None)),
        host("host-c", (0, 1), None, (None, None)),
        host("host-d", (0, 0), Some(HostMark::Releasing), (None, None)),
        host("host-e", (0, 0), None, (None, None)),
        lost,
    ];
    let text = section(hosts, None);
    assert!(text.contains("\njs_hosts_kept_empty 2\n"), "{text}");
}

/// Asserts that a scrape reads the telemetry of the placer of the server: none for a server
/// with no placer, and for one that places, the telemetry the placer gives (nothing has been
/// admitted, so it holds no figure yet). It catches a scrape that never reads the placer's
/// telemetry.
#[tokio::test]
async fn a_scrape_reads_the_telemetry_of_the_placer() {
    let mut bare = crate::agent_api::job_rig::rig("metrics_telemetry_none", false);
    bare.register("host-a", true);
    let shared = shared_over(&bare);
    let seen = snapshot(&shared, NonZeroUsize::MIN).await.unwrap();
    assert_eq!(seen.telemetry, None);
    let mut rig = clocked_rig("metrics_telemetry_read", true, JobLimits::default());
    rig.register("host-a", true);
    let placer = rig.agents.placer.clone().unwrap();
    let shared = shared_over(&rig);
    let seen = snapshot(&shared, NonZeroUsize::MIN).await.unwrap();
    assert_eq!(seen.telemetry, Some(placer.telemetry()));
    assert_eq!(seen.telemetry.map(|seen| seen.counters.admitted), Some(0));
}
