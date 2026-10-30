//! A result's `log_tail`: the round trip, the old payload, and the bound.

mod common;

use common::{expect, result, round_trip};
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_agent_wire::{LogTail, Rule, RunResult, refusal};
use serde_json::json;

fn tail(stdout: &str, stderr: &str, truncated: bool) -> LogTail {
    LogTail {
        stdout: stdout.into(),
        stderr: stderr.into(),
        truncated,
    }
}

/// A result with a tail survives encode and decode, the two streams stay apart, and a
/// result without one writes no `log_tail` key. It catches a swapped or merged stream and
/// a `null` that an older server (with no such field) would have to read.
#[test]
fn log_tail_round_trips_and_is_omitted_when_absent() {
    let with = RunResult {
        log_tail: Some(tail("out\n", "err\n", true)),
        ..result()
    };
    round_trip(&with);
    let value = serde_json::to_value(&with).unwrap();
    assert_eq!(
        value["log_tail"],
        json!({"stdout": "out\n", "stderr": "err\n", "truncated": true})
    );
    let without = serde_json::to_value(result()).unwrap();
    assert!(without.as_object().unwrap().get("log_tail").is_none());
}

/// A result from an agent that has no `log_tail` decodes with none, and a tail with only
/// some of its keys and an unknown key decodes. It catches a required field, which would
/// make every result of an older agent undecodable, and a `deny_unknown_fields`.
#[test]
fn an_old_payload_without_the_field_decodes() {
    let mut value = serde_json::to_value(result()).unwrap();
    assert!(value.as_object_mut().unwrap().remove("log_tail").is_none());
    let old: RunResult = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(old.log_tail, None);
    value["log_tail"] = json!({"stdout": "x", "later": 1});
    let newer: RunResult = serde_json::from_value(value).unwrap();
    assert_eq!(newer.log_tail, Some(tail("x", "", false)));
}

/// A stream at the cap passes `validate` and one byte more is refused as a too long
/// field of the heartbeat. It catches a tail that could push a heartbeat past its body
/// budget unchecked.
#[test]
fn a_stream_over_the_cap_is_refused() {
    let at_cap = "a".repeat(MAX_LOG_STREAM_LEN);
    let ok = RunResult {
        log_tail: Some(tail(&at_cap, &at_cap, false)),
        ..result()
    };
    assert_eq!(ok.validate(), Ok(()));
    let over = "a".repeat(MAX_LOG_STREAM_LEN + 1);
    let cases = [
        (tail(&over, "", true), "results.log_tail.stdout"),
        (tail("", &over, true), "results.log_tail.stderr"),
    ];
    for (bad, field) in cases {
        let run = RunResult {
            log_tail: Some(bad),
            ..result()
        };
        expect(
            run.validate().unwrap_err(),
            field,
            Rule::TooLong,
            refusal::INVALID_HEARTBEAT,
        );
    }
}
