//! The `usage` of a job's result: what the job's control group counted. Its wire form, that a
//! value that does not decode reads as absent without failing the result around it, and that
//! a result with it and one without it are read by the release before it and by this one.

mod common;

use common::heartbeat;
use js_agent_wire::{CpuThrottle, Heartbeat, HeldJob, HeldState, JobResult, JobUsage, Outcome};
use serde::Deserialize;
use serde_json::{Value, json};

/// The `JobResult` of the release before `usage`, kept as it was: it stands for servers
/// already deployed and must not follow later changes to [`JobResult`]. Fields it does not
/// know are skipped, as they were.
#[derive(Debug, Deserialize, PartialEq)]
struct PreviousJobResult {
    job_id: String,
    allocation_id: String,
    outcome: Outcome,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    detail: Option<String>,
}

/// The `JobUsage` of the release before the CPU throttle and pressure counters, kept as it was:
/// it stands for servers already deployed. Fields it does not know are skipped.
#[derive(Debug, Deserialize, PartialEq)]
struct PreviousJobUsage {
    peak_memory_bytes: u64,
    cpu_usage_usec: u64,
}

/// A result as the release before the counters read it, with its `usage`.
#[derive(Debug, Deserialize, PartialEq)]
struct PreviousResultWithUsage {
    job_id: String,
    usage: Option<PreviousJobUsage>,
}

/// The bytes the two optional counters add to a usage at their widest, 20 digits per number:
/// `,"cpu_throttle":{"periods":N,"throttled_periods":N}` 89 and `,"cpu_pressure_some_usec":N` 46.
const WIDEST_CPU_COUNTERS: usize = 89 + 46;

/// A usage as an agent from before the counters sent it: the two measured numbers.
fn counted() -> JobUsage {
    JobUsage {
        peak_memory_bytes: 3 << 30,
        cpu_usage_usec: 4_500_000,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
    }
}

/// A usage with every counter.
fn counted_with_cpu() -> JobUsage {
    JobUsage {
        cpu_throttle: Some(CpuThrottle {
            periods: 600,
            throttled_periods: 45,
        }),
        cpu_pressure_some_usec: Some(1_250_000),
        ..counted()
    }
}

fn result(usage: Option<JobUsage>) -> JobResult {
    JobResult {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: None,
        logs: None,
        input_failure: None,
        usage,
    }
}

/// The text of a result as the release before `usage` sent it.
fn previous_text() -> Value {
    json!({
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "outcome": "succeeded",
        "exit_code": 0
    })
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

/// Asserts the exact JSON of a result that carries usage, that it round-trips with both
/// counters, that a result without usage has no `usage` key, and that a measured zero stays a
/// zero on the wire. Catches a renamed counter, a `usage` written as `null` when absent, and a
/// zero that reads as absent.
#[test]
fn a_result_with_usage_has_the_exact_wire_form() {
    let with = result(Some(counted()));
    let wire = json!({
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "outcome": "succeeded",
        "exit_code": 0,
        "usage": { "peak_memory_bytes": 3_221_225_472_u64, "cpu_usage_usec": 4_500_000 }
    });
    assert_eq!(serde_json::to_value(&with).unwrap(), wire);
    assert_eq!(serde_json::from_value::<JobResult>(wire).unwrap(), with);
    assert_eq!(serde_json::to_value(result(None)).unwrap(), previous_text());
    let zero = result(Some(JobUsage {
        peak_memory_bytes: 0,
        cpu_usage_usec: 0,
        ..counted()
    }));
    let text = serde_json::to_value(&zero).unwrap();
    assert_eq!(
        text["usage"],
        json!({ "peak_memory_bytes": 0, "cpu_usage_usec": 0 })
    );
    assert_eq!(serde_json::from_value::<JobResult>(text).unwrap(), zero);
}

/// Asserts the bytes `usage` adds to a result at its widest: 89 with the two measured counters
/// at 20 digits, and 224 with the two optional ones at 20 digits as well, the figures of
/// `docs/design/agent-wire.md`. Catches a field whose size the heartbeat budget no longer
/// allows for.
#[test]
fn usage_at_its_widest_adds_its_documented_bytes_to_a_result() {
    let widest = JobUsage {
        peak_memory_bytes: u64::MAX,
        cpu_usage_usec: u64::MAX,
        ..counted()
    };
    let len = |result: &JobResult| serde_json::to_string(result).unwrap().len();
    assert_eq!(len(&result(Some(widest))) - len(&result(None)), 89);
    let all = JobUsage {
        cpu_throttle: Some(CpuThrottle {
            periods: u64::MAX,
            throttled_periods: u64::MAX,
        }),
        cpu_pressure_some_usec: Some(u64::MAX),
        ..widest
    };
    assert_eq!(
        len(&result(Some(all))) - len(&result(None)),
        89 + WIDEST_CPU_COUNTERS
    );
    assert_eq!(89 + WIDEST_CPU_COUNTERS, 224);
}

/// Asserts a usage with the CPU counters has the exact wire form, round-trips, and that a
/// counter alone is written without the others. Catches a renamed counter and a counter
/// written as `null` when absent.
#[test]
fn a_usage_with_cpu_counters_has_the_exact_wire_form() {
    let with = result(Some(counted_with_cpu()));
    let wire = json!({
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "outcome": "succeeded",
        "exit_code": 0,
        "usage": {
            "peak_memory_bytes": 3_221_225_472_u64,
            "cpu_usage_usec": 4_500_000,
            "cpu_throttle": { "periods": 600, "throttled_periods": 45 },
            "cpu_pressure_some_usec": 1_250_000
        }
    });
    assert_eq!(serde_json::to_value(&with).unwrap(), wire);
    assert_eq!(serde_json::from_value::<JobResult>(wire).unwrap(), with);
    let only_periods = JobUsage {
        cpu_throttle: Some(CpuThrottle {
            periods: 0,
            throttled_periods: 0,
        }),
        ..counted()
    };
    assert_eq!(
        serde_json::to_value(only_periods).unwrap(),
        json!({
            "peak_memory_bytes": 3_221_225_472_u64,
            "cpu_usage_usec": 4_500_000,
            "cpu_throttle": { "periods": 0, "throttled_periods": 0 }
        })
    );
}

/// Asserts a usage in the previous release's form, the two measured numbers only, still reads
/// as a usage, with the two counters as `None`. Catches a counter made required, which would
/// turn every result of an agent from before it into an absent usage.
#[test]
fn a_usage_without_the_cpu_counters_reads_with_none() {
    let mut text = previous_text();
    text["usage"] = json!({ "peak_memory_bytes": 3_221_225_472_u64, "cpu_usage_usec": 4_500_000 });
    let read: JobResult = serde_json::from_value(text).unwrap();
    assert_eq!(read, result(Some(counted())));
    let usage = read.usage.unwrap();
    assert_eq!(
        (usage.cpu_throttle, usage.cpu_pressure_some_usec),
        (None, None)
    );
}

/// Asserts a reader of the previous release, with its own copy of `JobUsage`, reads a usage
/// that carries the counters as the two numbers it knows. Catches a counter that an older
/// server cannot skip.
#[test]
fn the_previous_release_reads_a_usage_with_cpu_counters() {
    let new = serde_json::to_value(result(Some(counted_with_cpu()))).unwrap();
    let old: PreviousResultWithUsage = serde_json::from_value(new).unwrap();
    assert_eq!(old.job_id, "job-1");
    assert_eq!(
        old.usage,
        Some(PreviousJobUsage {
            peak_memory_bytes: 3 << 30,
            cpu_usage_usec: 4_500_000,
        })
    );
}

/// Asserts a server from before `usage` reads a result that carries it as it reads one without
/// (same ids, outcome and exit code), inside a heartbeat that this release validates, and that
/// this release reads a result from before the field with no usage. Catches a field an older
/// server cannot skip, and one this release demands of an older agent.
#[test]
fn the_previous_release_and_this_one_read_each_others_results() {
    let new = serde_json::to_value(result(Some(counted()))).unwrap();
    let old: PreviousJobResult = serde_json::from_value(new).unwrap();
    let plain: PreviousJobResult = serde_json::from_value(previous_text()).unwrap();
    assert_eq!(old, plain);
    assert_eq!(old.outcome, Outcome::Succeeded);
    assert_eq!((old.exit_code, old.detail), (Some(0), None));

    let read: JobResult = serde_json::from_value(previous_text()).unwrap();
    assert_eq!(read, result(None));
    assert_eq!(read.usage, None);

    let beat = beat_with(result(Some(counted())));
    assert_eq!(beat.validate(), Ok(()));
    let text = serde_json::to_string(&beat).unwrap();
    let back: Heartbeat = serde_json::from_str(&text).unwrap();
    assert_eq!(back.jobs[0].result.as_ref().unwrap().usage, Some(counted()));
}

/// Asserts a `usage` that does not decode (a counter negative, text, missing or of the wrong
/// type, or the whole value not an object) reads as absent, leaves the result's other fields
/// as sent, and does not fail the heartbeat's validation. Catches a malformed usage that
/// refuses a result, which an agent would then resend for ever.
#[test]
fn a_malformed_usage_reads_as_absent() {
    for bad in [
        json!({ "peak_memory_bytes": -1, "cpu_usage_usec": 1 }),
        json!({ "peak_memory_bytes": "big", "cpu_usage_usec": 1 }),
        json!({ "peak_memory_bytes": 1 }),
        json!({ "cpu_usage_usec": 1.5, "peak_memory_bytes": 1 }),
        json!("lots"),
        json!([1]),
        Value::Null,
    ] {
        let mut text = previous_text();
        text["usage"] = bad.clone();
        let read: JobResult = serde_json::from_value(text).unwrap();
        assert_eq!(read, result(None), "{bad}");
        assert_eq!(beat_with(read).validate(), Ok(()), "{bad}");
    }
}

/// Asserts a `cpu_throttle` that does not decode (a number negative, text, missing a field, or
/// not an object) reads as `None` while the usage around it keeps its other numbers. Catches a
/// malformed throttle that drops the whole usage.
#[test]
fn a_malformed_cpu_throttle_reads_as_none_and_keeps_the_usage() {
    for bad in [
        json!({ "periods": -1, "throttled_periods": 1 }),
        json!({ "periods": "many", "throttled_periods": 1 }),
        json!({ "periods": 1 }),
        json!({ "periods": 1.5, "throttled_periods": 1 }),
        json!(7),
        json!("1,2"),
        json!([1]),
    ] {
        let mut text = previous_text();
        text["usage"] = json!({
            "peak_memory_bytes": 3_221_225_472_u64,
            "cpu_usage_usec": 4_500_000,
            "cpu_throttle": bad.clone(),
            "cpu_pressure_some_usec": 9
        });
        let read: JobResult = serde_json::from_value(text).unwrap();
        let expected = JobUsage {
            cpu_pressure_some_usec: Some(9),
            ..counted()
        };
        assert_eq!(read, result(Some(expected)), "{bad}");
    }
}

/// Behaviour: a CPU pressure that is not a whole number of microseconds (negative, a string,
/// a float) reads as absent and leaves the rest of the usage, the throttle counters included,
/// as it was sent.
#[test]
fn a_malformed_cpu_pressure_reads_as_none_and_keeps_the_usage() {
    for bad in [json!(-1), json!("9"), json!(1.5), json!([9])] {
        let mut text = previous_text();
        text["usage"] = json!({
            "peak_memory_bytes": 3_221_225_472_u64,
            "cpu_usage_usec": 4_500_000,
            "cpu_throttle": { "periods": 10, "throttled_periods": 2 },
            "cpu_pressure_some_usec": bad.clone()
        });
        let read: JobResult = serde_json::from_value(text).unwrap();
        let expected = JobUsage {
            cpu_throttle: Some(CpuThrottle {
                periods: 10,
                throttled_periods: 2,
            }),
            ..counted()
        };
        assert_eq!(read, result(Some(expected)), "{bad}");
    }
}
