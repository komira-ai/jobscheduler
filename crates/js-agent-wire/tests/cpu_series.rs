//! The `cpu_series` of a job's result: the job's CPU second by second, coarsened as it ran. Its
//! wire form, that a value that does not decode or is not a well-formed series reads as absent
//! without failing the result around it, and that a result with it and one without it are read
//! by the release now deployed and by this one.

mod common;

use common::heartbeat;
use js_agent_wire::{
    CPU_SERIES_MAX_POINTS, CPU_SERIES_POINTS_PER_CORE, CpuPeak, CpuSeries, CpuThrottle, Heartbeat,
    HeldJob, HeldState, JobResult, JobUsage, LogReport, LogTail, Outcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The `JobResult` of the release before `cpu_series`, kept as it was: it stands for the servers
/// and agents of the release now deployed, and must not follow later changes to [`JobResult`].
/// Fields it does not know are skipped, as they were.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedJobResult {
    job_id: String,
    allocation_id: String,
    outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_tail: Option<LogTail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logs: Option<LogReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<ReleasedJobUsage>,
}

/// The `JobUsage` of the release now deployed, kept as it was.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct ReleasedJobUsage {
    peak_memory_bytes: u64,
    cpu_usage_usec: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_throttle: Option<CpuThrottle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_pressure_some_usec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_peak: Option<CpuPeak>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peak_swap_bytes: Option<u64>,
}

/// The bytes a series adds to a result at its widest: `,"cpu_series":` 14, then
/// `{"granularity_secs":N` 1 + 19 + 10, `,"points":[` 11, 128 points of 5 digits and the 127
/// commas between them 767, `]` 1, `,"run_usec":N` 1 + 11 + 20, `,"partial":true` 15 and `}` 1.
const WIDEST_SERIES: usize = 14 + 30 + 11 + 767 + 1 + 32 + 15 + 1;

fn usage() -> JobUsage {
    JobUsage {
        peak_memory_bytes: 3 << 30,
        cpu_usage_usec: 4_500_000,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: Some(CpuPeak {
            millicores: 3_500,
            window_usec: 2_000_000,
        }),
        peak_swap_bytes: None,
    }
}

/// A series of a run of 2.5 seconds: 2 cores, then 1.5, then 1 over the last half second.
fn series() -> CpuSeries {
    CpuSeries {
        granularity_secs: 1,
        points: vec![32, 24, 16],
        run_usec: 2_500_000,
        partial: false,
    }
}

fn result(cpu_series: Option<CpuSeries>) -> JobResult {
    JobResult {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: None,
        logs: None,
        input_failure: None,
        usage: Some(usage()),
        cpu_series,
    }
}

/// The text of the result with `series` in place of its `cpu_series`.
fn text_with(series: Value) -> Value {
    let mut text = serde_json::to_value(result(None)).unwrap();
    text["cpu_series"] = series;
    text
}

fn beat_with(result: JobResult) -> Heartbeat {
    Heartbeat {
        jobs: vec![HeldJob {
            job_id: result.job_id.clone(),
            state: HeldState::ExitedUnacked,
            result: Some(result),
        }],
        ..heartbeat()
    }
}

/// Asserts the exact text of a series: its granularity, its points in order, the run length,
/// and `partial` only when true; and that it reads back as the same series. Catches a renamed
/// key, points sent out of order, or a `partial: false` sent on every result.
#[test]
fn a_series_has_the_exact_wire_form() {
    let wire = serde_json::to_value(result(Some(series()))).unwrap();
    assert_eq!(
        wire["cpu_series"],
        json!({ "granularity_secs": 1, "points": [32, 24, 16], "run_usec": 2_500_000 })
    );
    assert_eq!(
        serde_json::from_value::<JobResult>(wire).unwrap(),
        result(Some(series()))
    );
    let partial = CpuSeries {
        partial: true,
        ..series()
    };
    assert_eq!(
        serde_json::to_value(&partial).unwrap(),
        json!({ "granularity_secs": 1, "points": [32, 24, 16], "run_usec": 2_500_000, "partial": true })
    );
    let none = serde_json::to_value(result(None)).unwrap();
    assert!(none.get("cpu_series").is_none(), "{none}");
}

/// Asserts the bytes a series of 128 points adds to a result at its widest, the figure
/// `docs/design/agent-wire.md` gives; and that a point is a sixteenth of a core. Catches a
/// series that outgrows the heartbeat budget the document counts.
#[test]
fn a_series_at_its_widest_adds_its_documented_bytes_to_a_result() {
    let widest = CpuSeries {
        granularity_secs: 1 << 31,
        points: vec![u16::MAX; CPU_SERIES_MAX_POINTS],
        run_usec: u64::MAX,
        partial: true,
    };
    let without = serde_json::to_string(&result(None)).unwrap().len();
    let with = serde_json::to_string(&result(Some(widest))).unwrap().len();
    // 2^31 has 10 digits, as u32::MAX does.
    assert_eq!(with - without, WIDEST_SERIES);
    assert_eq!(WIDEST_SERIES, 871);
    assert_eq!(
        (CPU_SERIES_MAX_POINTS, CPU_SERIES_POINTS_PER_CORE),
        (128, 16)
    );
}

/// Asserts the release now deployed, with its own copy of `JobResult`, reads a result that
/// carries a series as the same result without it, and sends back text that this release reads
/// with no series; and that a heartbeat carrying it validates. Catches a field an older server
/// cannot skip, and one this release demands of an older agent.
#[test]
fn the_released_server_and_this_one_read_each_others_results() {
    let new = serde_json::to_value(result(Some(series()))).unwrap();
    let old: ReleasedJobResult = serde_json::from_value(new).unwrap();
    let plain: ReleasedJobResult =
        serde_json::from_value(serde_json::to_value(result(None)).unwrap()).unwrap();
    assert_eq!(old, plain);
    let back: JobResult = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(back, result(None));
    let beat = beat_with(result(Some(series())));
    assert_eq!(beat.validate(), Ok(()));
    let read: Heartbeat = serde_json::from_value(serde_json::to_value(&beat).unwrap()).unwrap();
    assert_eq!(read, beat);
}

/// Asserts a series that does not decode, or is not well formed (a granularity that is not a
/// power of two, more than 128 points, a point past 16 bits), reads as absent and leaves the
/// rest of the result as it was, and that such a result is still accepted. Catches a bad series
/// failing the result, and so the heartbeat, that carries it.
#[test]
fn a_malformed_series_reads_as_absent_and_keeps_the_result() {
    let too_many = vec![1; CPU_SERIES_MAX_POINTS + 1];
    for bad in [
        json!("1,2,3"),
        json!({ "granularity_secs": 3, "points": [1], "run_usec": 1 }),
        json!({ "granularity_secs": 0, "points": [1], "run_usec": 1 }),
        json!({ "granularity_secs": 2, "points": too_many, "run_usec": 1 }),
        json!({ "granularity_secs": 1, "points": [65_536], "run_usec": 1 }),
        json!({ "granularity_secs": 1, "points": [1] }),
    ] {
        let read: JobResult = serde_json::from_value(text_with(bad.clone())).unwrap();
        assert_eq!(read, result(None), "{bad}");
        assert_eq!(read.validate(), Ok(()), "{bad}");
    }
}

/// Asserts the boundaries a series may sit at decode: no points, exactly 128, a granularity
/// of one second and one of 2^31 seconds. Catches an off-by-one at the point cap.
#[test]
fn a_series_at_its_bounds_decodes() {
    for good in [
        CpuSeries {
            points: Vec::new(),
            ..series()
        },
        CpuSeries {
            points: vec![u16::MAX; CPU_SERIES_MAX_POINTS],
            granularity_secs: 1 << 31,
            ..series()
        },
    ] {
        let text = text_with(serde_json::to_value(&good).unwrap());
        let read: JobResult = serde_json::from_value(text).unwrap();
        assert_eq!(read.cpu_series, Some(good));
    }
}
