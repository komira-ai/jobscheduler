//! Fixtures and helpers the test files share.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Debug;

use js_agent_wire::{
    GpuDevice, Heartbeat, HeldRun, HeldState, Hello, Isolation, JobSpec, LeaseClock, LeaseMinute,
    Limits, Mount, NodeLoad, Outcome, Phase, ProtocolRange, RunResult, Taint, ValidationError,
    Welcome, WireShape,
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
            disk_bytes: 0,
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
        accepts_host_credential: false,
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
        image:
            "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        args: vec!["make".into(), "all".into()],
        limits: Limits {
            cpu_millis: 2000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
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
        files: Vec::new(),
        valid_for_ms: 10_000,
        bundle: None,
        native: None,
    }
}

pub fn result() -> RunResult {
    RunResult {
        run_id: "run-1".into(),
        lease_id: "1-1".into(),
        outcome: Outcome::TimedOut,
        exit_code: Some(137),
        detail: Some("wall limit".into()),
        log_tail: None,
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
        exclusive_ready: None,
        exclusive_refused: None,
        load: None,
    }
}

/// A load with a distinct value in every field, so a swapped or renamed field shows.
/// `heartbeat().load` stays `None`, so the older goldens do not move.
pub fn load() -> NodeLoad {
    NodeLoad {
        seq: 7,
        mono_usec: 9_000_000,
        cpu_usage_usec: 4_500_000,
        working_set_bytes: 3 << 30,
        active_file_bytes: 1 << 20,
        actions_cpu_limited: true,
        covered: vec!["3-4".into()],
        clocks: vec![LeaseClock {
            lease_id: "3-4".into(),
            started_mono_usec: 1_000_000,
        }],
        minutes: vec![LeaseMinute {
            lease_id: "3-4".into(),
            minute: 2,
            cpu_mean_millis: 1500,
            cpu_peak_window_millis: 2500,
            ws_max_bytes: 2 << 30,
        }],
        dropped: js_agent_wire::LoadDrops::default(),
    }
}

/// A heartbeat that carries [`load`].
pub fn heartbeat_with_load() -> Heartbeat {
    Heartbeat {
        load: Some(load()),
        ..heartbeat()
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
        host_credential: None,
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
