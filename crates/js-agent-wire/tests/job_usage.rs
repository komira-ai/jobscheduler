//! The `usage` of a job's result: what the job's control group counted. Its wire form, that a
//! value that does not decode reads as absent without failing the result around it, and that
//! a result with it and one without it are read by the release before it and by this one.

mod common;

use common::heartbeat;
use js_agent_wire::{Heartbeat, HeldJob, HeldState, JobResult, JobUsage, Outcome};
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

fn counted() -> JobUsage {
    JobUsage {
        peak_memory_bytes: 3 << 30,
        cpu_usage_usec: 4_500_000,
        cpu_throttle: None,
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
        cpu_throttle: None,
    }));
    let text = serde_json::to_value(&zero).unwrap();
    assert_eq!(
        text["usage"],
        json!({ "peak_memory_bytes": 0, "cpu_usage_usec": 0 })
    );
    assert_eq!(serde_json::from_value::<JobResult>(text).unwrap(), zero);
}

/// Asserts the bytes `usage` adds to a result at its widest, both counters at 20 digits: 89,
/// the figure of `docs/design/agent-wire.md`. Catches a field whose size the heartbeat budget
/// no longer allows for.
#[test]
fn usage_at_its_widest_adds_89_bytes_to_a_result() {
    let widest = JobUsage {
        peak_memory_bytes: u64::MAX,
        cpu_usage_usec: u64::MAX,
        cpu_throttle: None,
    };
    let len = |result: &JobResult| serde_json::to_string(result).unwrap().len();
    assert_eq!(len(&result(Some(widest))) - len(&result(None)), 89);
}

/// Asserts the bytes `cpu_throttle` adds to a usage at its widest, both counts at 20 digits:
/// 89 more, 178 for the whole usage, the figure of `docs/design/agent-wire.md`; and that a
/// usage with no throttle counts writes none of it. Catches a field whose size the heartbeat
/// budget no longer allows for, and an absent count written as a zero.
#[test]
fn throttle_counts_at_their_widest_add_89_bytes_to_a_usage() {
    let widest = JobUsage {
        peak_memory_bytes: u64::MAX,
        cpu_usage_usec: u64::MAX,
        cpu_throttle: Some(js_agent_wire::CpuThrottle {
            periods: u64::MAX,
            throttled_periods: u64::MAX,
        }),
    };
    let len = |result: &JobResult| serde_json::to_string(result).unwrap().len();
    assert_eq!(len(&result(Some(widest))) - len(&result(None)), 178);
    let none = JobUsage {
        cpu_throttle: None,
        ..widest
    };
    let text = serde_json::to_string(&none).unwrap();
    assert!(!text.contains("cpu_throttle"), "{text}");
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
