//! Fixtures and helpers the test files share.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Debug;

use js_agent_wire::{
    GpuDevice, Heartbeat, HeldRun, HeldState, Hello, Isolation, JobSpec, Limits, Mount, Outcome,
    Phase, ProtocolRange, RunResult, Taint, ValidationError, Welcome, WireShape,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn hello() -> Hello {
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: "mac-01".into(),
        pool: "builders".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 1 << 34,
            gpus: 1,
            gpu_class: Some("nvidia-a100".into()),
            max_leases: Some(4),
        },
        capabilities: map(&[("os", "macos")]),
        gpu_devices: vec![GpuDevice {
            index: 0,
            uuid: "GPU-1".into(),
            model: "A100".into(),
            memory_bytes: 1 << 36,
        }],
        labels: map(&[("rack", "r1")]),
        phase: Phase::Serving,
        held: vec![HeldRun {
            run_id: "run-1".into(),
            state: HeldState::Running,
        }],
    }
}

pub fn job() -> JobSpec {
    JobSpec {
        run_id: "run-1".into(),
        lease_id: "1-1".into(),
        image: "ubuntu:24.04".into(),
        args: vec!["make".into(), "all".into()],
        limits: Limits {
            cpu_millis: 2000,
            memory_bytes: 1 << 30,
            pids: Some(512),
            wall_ms: 60_000,
        },
        gpu_indices: vec![0],
        mounts: vec![Mount {
            root_name: "cache".into(),
            rel: "inputs/a".into(),
            ro: true,
        }],
        isolation: Isolation::Host,
        labels: map(&[("team", "infra")]),
        heartbeat_seq: 7,
        valid_for_ms: 10_000,
    }
}

pub fn result() -> RunResult {
    RunResult {
        run_id: "run-1".into(),
        lease_id: "1-1".into(),
        outcome: Outcome::TimedOut,
        exit_code: Some(137),
        detail: Some("wall limit".into()),
    }
}

pub fn heartbeat() -> Heartbeat {
    Heartbeat {
        host_id: "mac-01".into(),
        epoch: 3,
        session: "s3cret-0".into(),
        phase: Phase::Draining,
        held: vec![HeldRun {
            run_id: "run-1".into(),
            state: HeldState::ExitedUnacked,
        }],
        results: vec![result()],
        taint: Some(Taint {
            reason: "cleanup-failed".into(),
            run_id: Some("run-1".into()),
            check: Some("new_mount".into()),
        }),
    }
}

pub fn welcome() -> Welcome {
    Welcome {
        protocol: 1,
        epoch: 9,
        heartbeat_interval_ms: 5000,
        grace_ms: 15_000,
        pool: "builders".into(),
        session: "s3cret-0".into(),
    }
}

/// Encodes `value` and decodes it again.
pub fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let text = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&back, value);
}

/// `value` as JSON with an unknown field added to the object at `path` (empty: the root).
pub fn with_unknown<T: Serialize>(value: &T, path: &[&str]) -> Value {
    let mut json = serde_json::to_value(value).unwrap();
    let mut at = &mut json;
    for key in path {
        at = &mut at[*key];
    }
    at["field_from_the_future"] = json!({"nested": [1, 2]});
    json
}

/// A string of `len` bytes of the letter `a`.
pub fn long(len: usize) -> String {
    "a".repeat(len)
}

/// Asserts that `result` is an error with `code` and returns it.
pub fn refused(result: Result<(), ValidationError>, code: &str) -> ValidationError {
    let err = result.unwrap_err();
    assert_eq!(err.code(), code, "{err}");
    err
}

/// Applies `edit` to a copy of `good` and returns the error `validate` gives for it.
pub fn broken<T: Clone>(
    good: &T,
    edit: impl FnOnce(&mut T),
    validate: impl Fn(&T) -> Result<(), ValidationError>,
) -> ValidationError {
    assert_eq!(validate(good), Ok(()), "the fixture must be valid");
    let mut copy = good.clone();
    edit(&mut copy);
    validate(&copy).unwrap_err()
}

/// Asserts the field, rule and code of `err`.
pub fn expect(err: ValidationError, field: &str, rule: js_agent_wire::Rule, code: &str) {
    assert_eq!(
        (err.field(), err.rule(), err.code()),
        (field, rule, code),
        "{err}"
    );
}
