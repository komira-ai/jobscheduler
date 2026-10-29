//! The literal wire form of each message. A renamed field, a changed tag or a lost
//! `rename_all` changes these texts, so each assertion goes red on the rename.

mod common;

use common::{heartbeat, hello, job, welcome};
use js_agent_wire::refusal;
use js_agent_wire::{
    Ack, Command, ExclusiveRefusal, Heartbeat, HeartbeatReply, HelloReply, Isolation, Outcome,
    ProtocolRange, RefusalReason, Refused, ResultAck, Taint,
};
use serde_json::{Value, json};

/// Asserts the exact JSON of a hello, with an instance identity and every optional
/// field present. It catches a renamed hello field (including `protocol`, `instance`,
/// `max_leases` and `gpu_class`), which an encode-decode round trip cannot see.
#[test]
fn hello_wire_form() {
    let mut value = hello();
    value.instance = Some(js_agent_wire::InstanceIdentity {
        provider: "aws".into(),
        instance_id: "i-0abc".into(),
    });
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({
            "protocol": {"min": 1, "max": 1},
            "host_id": "mac-01",
            "pool": "builders",
            "agent_version": "0.1.0",
            "instance": {"provider": "aws", "instance_id": "i-0abc"},
            "shape": {
                "cpu_millis": 8000,
                "memory_bytes": 17179869184u64,
                "gpus": 1,
                "gpu_class": "nvidia-a100",
                "max_leases": 4
            },
            "capabilities": {"os": "macos"},
            "gpu_devices": [{
                "index": 0,
                "uuid": "GPU-1",
                "model": "A100",
                "memory_bytes": 68719476736u64
            }],
            "labels": {"rack": "r1"},
            "phase": "serving",
            "held": [{"run_id": "run-1", "state": "running"}]
        })
    );
}

/// Asserts the exact JSON of a welcome, and of a hello reply that carries a welcome and
/// one that carries a refusal with the supported range. It catches a renamed welcome
/// field and a changed `type` tag, which would make a deployed agent read a refusal as a
/// welcome or fail to read either.
#[test]
fn welcome_and_hello_reply_wire_forms() {
    assert_eq!(
        serde_json::to_value(welcome()).unwrap(),
        json!({
            "protocol": 1,
            "epoch": 9,
            "heartbeat_interval_ms": 5000,
            "grace_ms": 15000,
            "pool": "builders",
            "session": "s3cret-0"
        })
    );
    assert_eq!(
        serde_json::to_value(HelloReply::Welcome(welcome())).unwrap(),
        json!({
            "type": "welcome",
            "protocol": 1,
            "epoch": 9,
            "heartbeat_interval_ms": 5000,
            "grace_ms": 15000,
            "pool": "builders",
            "session": "s3cret-0"
        })
    );
    let refused = HelloReply::Refused(Refused {
        code: "unsupported-protocol".into(),
        supported: Some(ProtocolRange { min: 2, max: 3 }),
        field: None,
    });
    assert_eq!(
        serde_json::to_value(&refused).unwrap(),
        json!({
            "type": "refused",
            "code": "unsupported-protocol",
            "supported": {"min": 2, "max": 3}
        })
    );
    let bare = HelloReply::Refused(Refused {
        code: "host-live".into(),
        supported: None,
        field: None,
    });
    assert_eq!(
        serde_json::to_value(&bare).unwrap(),
        json!({"type": "refused", "code": "host-live"})
    );
    let named = HelloReply::Refused(Refused {
        code: refusal::HOST_CONTRACT.into(),
        supported: None,
        field: Some("runtime".into()),
    });
    assert_eq!(
        serde_json::to_value(&named).unwrap(),
        json!({"type": "refused", "code": "host-contract", "field": "runtime"})
    );
    // A refusal an older server sent has no `field`; it reads as none.
    let old: Refused = serde_json::from_value(json!({"code": "host-live"})).unwrap();
    assert_eq!(old.field, None);
}

/// Asserts the exact JSON of a start command, tag included. It catches a changed
/// `start` tag, a renamed job or limits field, and a lost `snake_case` on isolation.
#[test]
fn start_command_wire_form() {
    let expected = json!({
        "type": "start",
        "run_id": "run-1",
        "lease_id": "1-1",
        "image": "ubuntu:24.04",
        "args": ["make", "all"],
        "limits": {
            "cpu_millis": 2000,
            "memory_bytes": 1073741824u64,
            "pids": 512,
            "wall_ms": 60000
        },
        "gpu_indices": [0],
        "mounts": [{"root_name": "cache", "rel": "inputs/a", "ro": true}],
        "isolation": "host",
        "labels": {"team": "infra"},
        "heartbeat_seq": 7,
        "valid_for_ms": 10000
    });
    let command = Command::Start(Box::new(job()));
    assert_eq!(serde_json::to_value(&command).unwrap(), expected);
    let decoded: Command = serde_json::from_value(expected).unwrap();
    assert_eq!(decoded, command);
    let contained = serde_json::to_value(Isolation::Contained).unwrap();
    assert_eq!(contained, json!("contained"));
}

/// Asserts the exact JSON of a heartbeat that carries a held run, a run result and a
/// taint, and the spelling of every outcome. It catches a renamed heartbeat, result or
/// taint field and a changed outcome spelling.
#[test]
fn heartbeat_with_result_wire_form() {
    assert_eq!(
        serde_json::to_value(heartbeat()).unwrap(),
        json!({
            "host_id": "mac-01",
            "epoch": 3,
            "session": "s3cret-0",
            "phase": "draining",
            "held": [{"run_id": "run-1", "state": "exited_unacked"}],
            "results": [{
                "run_id": "run-1",
                "lease_id": "1-1",
                "outcome": "timed_out",
                "exit_code": 137,
                "detail": "wall limit"
            }],
            "taint": {"reason": "cleanup-failed", "run_id": "run-1", "check": "new_mount"}
        })
    );
    let outcomes = [
        Outcome::Succeeded,
        Outcome::Failed,
        Outcome::Killed,
        Outcome::TimedOut,
        Outcome::OutOfMemory,
        Outcome::FailedToStart,
    ];
    assert_eq!(
        serde_json::to_string(&outcomes).unwrap(),
        r#"["succeeded","failed","killed","timed_out","out_of_memory","failed_to_start"]"#
    );
    let taint = Taint {
        reason: "x".into(),
        run_id: None,
        check: None,
    };
    assert_eq!(
        serde_json::to_value(&taint).unwrap(),
        json!({"reason": "x"})
    );
}

/// Asserts the exact JSON of a reply with a start, a stop, a cancel, an acknowledgement
/// and a taint acknowledgement, and the spelling of every verdict. It catches a renamed
/// reply field, command tag or verdict.
#[test]
fn heartbeat_reply_wire_form() {
    let reply = HeartbeatReply {
        commands: vec![
            Command::Stop {
                run_id: "r1".into(),
            },
            Command::Cancel {
                run_id: "r2".into(),
            },
        ],
        result_acks: vec![ResultAck {
            run_id: "r1".into(),
            ack: Ack::UnknownLease,
        }],
        taint_ack: true,
        exclusive: None,
    };
    assert_eq!(
        serde_json::to_value(&reply).unwrap(),
        json!({
            "commands": [
                {"type": "stop", "run_id": "r1"},
                {"type": "cancel", "run_id": "r2"}
            ],
            "result_acks": [{"run_id": "r1", "ack": "UNKNOWN_LEASE"}],
            "taint_ack": true
        })
    );
    let verdicts = [
        Ack::Accepted,
        Ack::Duplicate,
        Ack::Stale,
        Ack::UnknownLease,
        Ack::Mismatch,
    ];
    assert_eq!(
        serde_json::to_string(&verdicts).unwrap(),
        r#"["ACCEPTED","DUPLICATE","STALE","UNKNOWN_LEASE","MISMATCH"]"#
    );
}

/// Asserts that a golden heartbeat decodes from literal text with the phase spelled as
/// the design spells it. It catches a heartbeat that serializes but no longer reads the
/// text a deployed agent sends.
#[test]
fn heartbeat_decodes_from_literal_text() {
    let text = r#"{"host_id":"mac-01","epoch":3,"phase":"memory-pressure",
        "held":[{"run_id":"run-1","state":"starting"}]}"#;
    let beat: Heartbeat = serde_json::from_str(text).unwrap();
    assert_eq!(beat.phase, js_agent_wire::Phase::MemoryPressure);
    assert_eq!(beat.held[0].state, js_agent_wire::HeldState::Starting);
    let _: Value = serde_json::from_str(text).unwrap();
}

/// Asserts the exact JSON of a heartbeat that reports an exclusive lease ready, and of
/// one that refuses it, and that a reply with `exclusive` encodes to fixed bytes. It
/// catches a renamed field or code spelling, and a field sent when it is None.
#[test]
fn exclusive_wire_form() {
    let ready = Heartbeat {
        exclusive_ready: Some("7-1".into()),
        taint: None,
        results: Vec::new(),
        held: Vec::new(),
        ..heartbeat()
    };
    assert_eq!(
        serde_json::to_string(&ready).unwrap(),
        concat!(
            r#"{"host_id":"mac-01","epoch":3,"session":"s3cret-0","phase":"draining","#,
            r#""held":[],"results":[],"exclusive_ready":"7-1"}"#
        )
    );
    let refused = Heartbeat {
        exclusive_refused: Some(ExclusiveRefusal {
            lease_id: "7-1".into(),
            code: RefusalReason::NoiseOverBound,
            detail: "p99 4ms".into(),
        }),
        taint: None,
        results: Vec::new(),
        held: Vec::new(),
        ..heartbeat()
    };
    assert_eq!(
        serde_json::to_string(&refused).unwrap(),
        concat!(
            r#"{"host_id":"mac-01","epoch":3,"session":"s3cret-0","phase":"draining","#,
            r#""held":[],"results":[],"exclusive_refused":"#,
            r#"{"lease_id":"7-1","code":"noise-over-bound","detail":"p99 4ms"}}"#
        )
    );
    let reply = HeartbeatReply {
        exclusive: Some("7-1".into()),
        ..HeartbeatReply::default()
    };
    assert_eq!(
        serde_json::to_string(&reply).unwrap(),
        r#"{"commands":[],"result_acks":[],"taint_ack":false,"exclusive":"7-1"}"#
    );
}

/// A heartbeat and a reply from before the exclusive fields (the existing fixtures carry
/// none) decode with them None and encode without them: the change is additive. It
/// catches a required new field, which would refuse every deployed agent.
#[test]
fn a_body_from_before_the_exclusive_fields_still_decodes() {
    let old_beat = serde_json::to_value(heartbeat()).unwrap();
    assert!(old_beat.get("exclusive_ready").is_none());
    assert!(old_beat.get("exclusive_refused").is_none());
    let beat: Heartbeat = serde_json::from_value(old_beat.clone()).unwrap();
    assert_eq!((beat.exclusive_ready, beat.exclusive_refused), (None, None));
    let old_reply = json!({"commands": [], "result_acks": [], "taint_ack": true});
    let reply: HeartbeatReply = serde_json::from_value(old_reply.clone()).unwrap();
    assert_eq!(reply.exclusive, None);
    assert_eq!(serde_json::to_value(&reply).unwrap(), old_reply);
}
