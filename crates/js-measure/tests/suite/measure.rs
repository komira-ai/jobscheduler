//! The arithmetic of the numbers, on inputs whose answers are worked out by hand.

use std::collections::BTreeMap;

use js_measure::measure::{
    HostReading, Identity, JobFacts, Prediction, assemble, share, window_usage,
};
use js_measure::metric::name;
use js_measure::{Curve, HostShape, JobTiming, Observation, UsageRow};

use crate::support::{GIB, T0, job};

fn row(minute: u64, cpu_millis: u64, ws_bytes: u64, leases: u64) -> UsageRow {
    UsageRow {
        minute,
        cpu_millis,
        ws_bytes,
        leases,
    }
}

fn timing(name: &str, host: &str) -> JobTiming {
    JobTiming {
        name: name.to_owned(),
        submit_at_ms: T0,
        requested_ms: Some(T0),
        granted_ms: Some(T0),
        started_ms: Some(T0),
        finished_ms: Some(T0 + 60_000),
        host: Some(host.to_owned()),
        outcome: Some("succeeded".to_owned()),
        failure: None,
        log_tail: None,
    }
}

/// Asserts a share is a part over its whole and that a whole of zero has no share (not
/// infinity, not zero).
#[test]
fn a_share_is_part_over_whole() {
    assert_eq!(share(1, 4), Some(0.25));
    assert_eq!(share(1, 0), None);
}

/// Asserts the prediction a curve holds: its length in minutes as milliseconds and its
/// largest working set; a curve with no minute predicts nothing.
#[test]
fn a_curve_predicts_its_length_and_its_peak() {
    let curve = Curve {
        runs: 5,
        ws_bytes: vec![GIB, 5 * GIB, 3 * GIB],
        cores_milli: Vec::new(),
    };
    let prediction = Prediction::of(&curve).unwrap();
    assert_eq!(prediction.life_ms, 180_000);
    assert_eq!(prediction.peak_ws_bytes, 5 * GIB);
    let empty = Curve {
        ws_bytes: Vec::new(),
        ..curve
    };
    assert_eq!(Prediction::of(&empty), None);
}

/// Asserts the window arithmetic: CPU is each minute's mean times the part of the minute
/// the window covers, the peak is the largest working set of the minutes it touches, and
/// the window is refused (not guessed) when a minute has no row, a row shared by two
/// leases, or when it is empty. It catches usage attributed to a lease that shared the host.
#[test]
fn a_window_counts_only_minutes_the_lease_had_alone() {
    let rows = [
        row(10, 1_000, 3 * GIB, 1),
        row(11, 3_000, 7 * GIB, 1),
        row(12, 1, 1, 2),
    ];
    let half = window_usage(&rows, T0, T0 + 30_000).unwrap();
    assert_eq!(
        (half.cpu_milli_ms, half.peak_ws_bytes),
        (30_000_000, 3 * GIB)
    );
    let two = window_usage(&rows, T0 + 30_000, T0 + 90_000).unwrap();
    assert_eq!(two.cpu_milli_ms, 1_000 * 30_000 + 3_000 * 30_000);
    assert_eq!(two.peak_ws_bytes, 7 * GIB);
    let whole = window_usage(&rows, T0, T0 + 60_000).unwrap();
    assert_eq!(whole.cpu_milli_ms, 60_000_000);
    // A window that ends on a minute boundary does not touch the next minute: its peak is
    // minute 10's, not minute 11's.
    assert_eq!(whole.peak_ws_bytes, 3 * GIB);
    let alone = [row(10, 1_000, GIB, 1)];
    assert_eq!(
        window_usage(&alone, T0, T0 + 60_000).map(|u| u.cpu_milli_ms),
        Some(60_000_000)
    );
    assert_eq!(
        window_usage(&rows, T0 + 60_000, T0 + 130_000),
        None,
        "minute 12 is shared"
    );
    assert_eq!(
        window_usage(&rows, T0 + 120_000 + 60_000, T0 + 300_000),
        None,
        "no row"
    );
    assert_eq!(window_usage(&rows, T0, T0), None, "empty window");
    assert_eq!(window_usage(&rows, T0 + 5, T0), None, "backwards window");
}

fn known_timing() -> JobTiming {
    JobTiming {
        name: "j".to_owned(),
        submit_at_ms: 1_000,
        requested_ms: Some(1_000),
        granted_ms: Some(1_500),
        started_ms: Some(2_500),
        finished_ms: Some(62_500),
        host: Some("h".to_owned()),
        outcome: Some("succeeded".to_owned()),
        failure: None,
        log_tail: None,
    }
}

/// Asserts every fact of one job on inputs worked out by hand: queue wait 2500-1000,
/// request to token 1500-1000, run time 62500-2500, actual over predicted 60000/120000
/// and 6/4 GiB, booked CPU 4000 millicores over the 61000 ms the lease was held.
#[test]
fn the_facts_of_one_job() {
    let prediction = Prediction {
        life_ms: 120_000,
        peak_ws_bytes: 4 * GIB,
    };
    let usage = js_measure::measure::WindowUsage {
        cpu_milli_ms: 60_000_000,
        peak_ws_bytes: 6 * GIB,
    };
    let facts = JobFacts::of(
        &known_timing(),
        &job("j", 1, None),
        Some(&prediction),
        Some(&usage),
    );
    assert_eq!(facts.queue_wait_ms, Some(1_500.0));
    assert_eq!(facts.request_to_token_ms, Some(500.0));
    assert_eq!(facts.runtime_ms, Some(60_000.0));
    assert_eq!(facts.predicted_runtime_ms, Some(120_000.0));
    assert_eq!(facts.runtime_actual_over_predicted, Some(0.5));
    assert_eq!(facts.peak_ws_bytes, Some(6.0 * GIB as f64));
    assert_eq!(facts.predicted_peak_ws_bytes, Some(4.0 * GIB as f64));
    assert_eq!(facts.memory_actual_over_predicted, Some(1.5));
    assert_eq!(facts.booked_cpu_milli_ms, Some(4_000.0 * 61_000.0));
    assert_eq!(facts.used_cpu_milli_ms, Some(60_000_000.0));
    assert_eq!(facts.booked_memory_bytes, 8.0 * GIB as f64);
}

/// Asserts that a fact with a missing input is missing, in the facts and in the metrics,
/// never zero: no prediction, no usage, a job that never started, and times in the wrong
/// order. Booked memory is the request and is always there.
#[test]
fn a_fact_without_its_inputs_is_not_available() {
    let mut never_ran = known_timing();
    never_ran.started_ms = None;
    never_ran.finished_ms = None;
    never_ran.granted_ms = None;
    let facts = JobFacts::of(&never_ran, &job("j", 1, None), None, None);
    let metrics = facts.metrics();
    for key in [
        "queue_wait_ms",
        "request_to_token_ms",
        "runtime_ms",
        "predicted_runtime_ms",
        "runtime_actual_over_predicted",
        "peak_ws_bytes",
        "predicted_peak_ws_bytes",
        "memory_actual_over_predicted",
        "booked_cpu_milli_ms",
        "used_cpu_milli_ms",
    ] {
        assert_eq!(metrics[key], None, "{key}");
    }
    assert_eq!(metrics["booked_memory_bytes"], Some(8.0 * GIB as f64));
    let mut backwards = known_timing();
    backwards.finished_ms = Some(2_000);
    let facts = JobFacts::of(&backwards, &job("j", 1, None), None, None);
    assert_eq!(facts.runtime_ms, None);
    let zero = Prediction {
        life_ms: 0,
        peak_ws_bytes: 0,
    };
    let facts = JobFacts::of(&known_timing(), &job("j", 1, None), Some(&zero), None);
    assert_eq!(
        facts.runtime_actual_over_predicted, None,
        "a zero prediction divides nothing"
    );
}

fn who() -> Identity {
    Identity {
        setting: "fill".to_owned(),
        arm: "0.8".to_owned(),
        run_index: 2,
        seed: 9,
        commit: "c0".to_owned(),
        label: String::new(),
    }
}

fn two_jobs() -> (Observation, Vec<js_measure::Job>) {
    let a = timing("a", "h1");
    let mut b = timing("b", "h1");
    b.granted_ms = Some(T0 + 60_000);
    b.started_ms = Some(T0 + 60_000);
    b.finished_ms = Some(T0 + 120_000);
    b.outcome = Some("out_of_memory".to_owned());
    let obs = Observation {
        t0_ms: T0,
        end_ms: T0 + 180_000,
        jobs: vec![a, b],
    };
    let mut ja = job("a", 1, Some("p"));
    ja.lease.cpu_millis = 32_000;
    ja.lease.memory_bytes = 128 * GIB;
    let mut jb = job("b", 1, None);
    jb.lease.cpu_millis = 32_000;
    jb.lease.memory_bytes = 128 * GIB;
    (obs, vec![ja, jb])
}

fn host_h1() -> BTreeMap<String, HostReading> {
    BTreeMap::from([(
        "h1".to_owned(),
        HostReading {
            shape: Some(HostShape {
                cpu_millis: 32_000,
                memory_bytes: 128 * GIB,
            }),
            rows: Some(vec![
                row(10, 8_000, 32 * GIB, 1),
                row(11, 16_000, 128 * GIB, 1),
            ]),
        },
    )])
}

fn value(metrics: &BTreeMap<String, Option<f64>>, key: &str) -> f64 {
    metrics[key].unwrap_or_else(|| panic!("{key} is not available"))
}

/// Asserts every number of a run on two jobs run back to back on one host, worked out by
/// hand: makespan 120000, queue wait mean 30000 and max 60000, request to token the same,
/// the host's shares by minute (CPU 0.25 and 0.5, RAM 0.25 and 1.0), actual over predicted
/// for the one job with a prediction, CPU used 1.44e9 of 3.84e9 booked, memory peak 160 of
/// 256 GiB booked, one failure that is a memory kill. It catches a wrong denominator, a
/// job counted twice, and a number averaged over jobs that had no reading.
#[test]
fn a_run_on_two_jobs() {
    let (obs, jobs) = two_jobs();
    let predictions = BTreeMap::from([(
        "p".to_owned(),
        Prediction {
            life_ms: 120_000,
            peak_ws_bytes: 64 * GIB,
        },
    )]);
    let out = assemble(&who(), &obs, &jobs, &predictions, &host_h1());
    let m = &out.record.metrics;
    assert_eq!(value(m, name::MAKESPAN_MS), 120_000.0);
    assert_eq!(value(m, name::JOBS), 2.0);
    assert_eq!(value(m, name::JOBS_FAILED), 1.0);
    assert_eq!(value(m, name::MEMORY_KILLS), 1.0);
    assert_eq!(value(m, name::QUEUE_WAIT_MS_MEAN), 30_000.0);
    assert_eq!(value(m, name::QUEUE_WAIT_MS_MAX), 60_000.0);
    assert_eq!(value(m, name::REQUEST_TO_TOKEN_MS_MEAN), 30_000.0);
    assert_eq!(value(m, name::REQUEST_TO_TOKEN_MS_MAX), 60_000.0);
    assert_eq!(value(m, name::HOST_CPU_SHARE_MEAN), 0.375);
    assert_eq!(value(m, name::HOST_CPU_SHARE_MAX), 0.5);
    assert_eq!(value(m, name::HOST_RAM_SHARE_MEAN), 0.625);
    assert_eq!(value(m, name::HOST_RAM_SHARE_MAX), 1.0);
    assert_eq!(value(m, name::RUNTIME_ACTUAL_OVER_PREDICTED_MEAN), 0.5);
    assert_eq!(value(m, name::MEMORY_ACTUAL_OVER_PREDICTED_MEAN), 0.5);
    assert_eq!(value(m, name::CPU_USED_OVER_BOOKED), 0.375);
    assert_eq!(value(m, name::MEMORY_USED_OVER_BOOKED), 0.625);
    let record = &out.record;
    assert_eq!(
        (record.setting.as_str(), record.arm.as_str()),
        ("fill", "0.8")
    );
    assert_eq!((record.run_index, record.seed), (2, 9));
    assert_eq!((record.commit.as_str(), record.label.as_str()), ("c0", ""));
}

/// Asserts the rows under the record: one per job with the job's name, host and outcome
/// and its own facts, one per host-minute with the shares, in minute order.
#[test]
fn the_rows_under_a_run() {
    let (obs, jobs) = two_jobs();
    let out = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &host_h1());
    assert_eq!(out.jobs.len(), 2);
    let b = &out.jobs[1];
    assert_eq!((b.job.as_str(), b.host.as_deref()), ("b", Some("h1")));
    assert_eq!(b.outcome.as_deref(), Some("out_of_memory"));
    assert_eq!(
        (b.setting.as_str(), b.arm.as_str(), b.run_index),
        ("fill", "0.8", 2)
    );
    assert_eq!(b.metrics["queue_wait_ms"], Some(60_000.0));
    assert_eq!(b.metrics["predicted_runtime_ms"], None);
    let minutes: Vec<(u64, Option<f64>, Option<f64>, u64)> = out
        .minutes
        .iter()
        .map(|r| (r.minute, r.cpu_share, r.ram_share, r.leases))
        .collect();
    assert_eq!(
        minutes,
        vec![
            (10, Some(0.25), Some(0.25), 1),
            (11, Some(0.5), Some(1.0), 1),
        ]
    );
    assert_eq!(out.minutes[0].host, "h1");
    assert_eq!(out.minutes[0].run_index, 2);
}

/// Asserts what a run with no readable server data records: the numbers of the drive
/// (makespan, waits, counts) stand, every number that needs the usage rows or a
/// prediction is not available, there are no minute rows, and a host whose shape could
/// not be read keeps its rows with the shares not available.
#[test]
fn a_run_without_readings_has_the_drive_and_nothing_invented() {
    let (obs, jobs) = two_jobs();
    let out = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &BTreeMap::new());
    let m = &out.record.metrics;
    assert_eq!(value(m, name::MAKESPAN_MS), 120_000.0);
    for key in [
        name::HOST_CPU_SHARE_MEAN,
        name::HOST_CPU_SHARE_MAX,
        name::HOST_RAM_SHARE_MEAN,
        name::HOST_RAM_SHARE_MAX,
        name::RUNTIME_ACTUAL_OVER_PREDICTED_MEAN,
        name::MEMORY_ACTUAL_OVER_PREDICTED_MEAN,
        name::CPU_USED_OVER_BOOKED,
        name::MEMORY_USED_OVER_BOOKED,
    ] {
        assert_eq!(m[key], None, "{key}");
    }
    assert!(out.minutes.is_empty());
    let mut hosts = host_h1();
    hosts.get_mut("h1").unwrap().shape = None;
    let out = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &hosts);
    assert_eq!(out.minutes.len(), 2);
    assert!(out.minutes.iter().all(|r| r.cpu_share.is_none()));
    assert_eq!(out.record.metrics[name::HOST_CPU_SHARE_MEAN], None);
    assert_eq!(
        value(&out.record.metrics, name::CPU_USED_OVER_BOOKED),
        0.375
    );
    let mut unreadable = host_h1();
    unreadable.get_mut("h1").unwrap().rows = None;
    let out = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &unreadable);
    assert!(out.minutes.is_empty());
}

/// Asserts that a run in which no job finished has no makespan, and counts its failures:
/// a job that never got a lease is a failure and not a memory kill.
#[test]
fn a_run_in_which_nothing_finished_has_no_makespan() {
    let mut timing = timing("a", "h1");
    timing.finished_ms = None;
    timing.outcome = None;
    timing.failure = Some("deadline".to_owned());
    let obs = Observation {
        t0_ms: T0,
        end_ms: T0 + 1,
        jobs: vec![timing],
    };
    let out = assemble(
        &who(),
        &obs,
        &[job("a", 1, None)],
        &BTreeMap::new(),
        &host_h1(),
    );
    assert_eq!(out.record.metrics[name::MAKESPAN_MS], None);
    assert_eq!(value(&out.record.metrics, name::JOBS_FAILED), 1.0);
    assert_eq!(value(&out.record.metrics, name::MEMORY_KILLS), 0.0);
    assert_eq!(out.jobs[0].failure.as_deref(), Some("deadline"));
}

/// Asserts that the makespan is not available when any job has no finish: a job that failed
/// or was given up would otherwise shorten it to the jobs that happened to finish.
#[test]
fn a_makespan_needs_every_job_to_have_finished() {
    let (mut obs, jobs) = two_jobs();
    let whole = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &host_h1());
    assert_eq!(value(&whole.record.metrics, name::MAKESPAN_MS), 120_000.0);
    obs.jobs[1].finished_ms = None;
    obs.jobs[1].failure = Some("deadline".to_owned());
    let cut = assemble(&who(), &obs, &jobs, &BTreeMap::new(), &host_h1());
    assert_eq!(cut.record.metrics[name::MAKESPAN_MS], None);
    assert_eq!(value(&cut.record.metrics, name::JOBS_FAILED), 1.0);
}
