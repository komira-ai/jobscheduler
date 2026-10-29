//! Round trips of every message and variant, the refusal-code snapshot and the grace
//! relation.

mod common;

use common::{heartbeat, heartbeat_with_load, hello, job, round_trip, welcome};
use js_agent_wire::refusal;
use js_agent_wire::{
    Ack, Command, DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, HeartbeatReply, HeldState,
    HelloReply, Isolation, Outcome, Phase, ProtocolRange, Refused, ResultAck, Taint, WireShape,
    grace_covers_heartbeats,
};
use js_types::{Resources, Shape, TypeError};

/// Asserts the exact text of every refusal code. A rename would stop a deployed agent
/// or server from recognising the code, so changing this table must be a decision.
#[test]
fn refusal_codes_snapshot() {
    let codes = [
        refusal::INVALID_HELLO,
        refusal::RESERVED_LABEL,
        refusal::WALL_TIME_NOT_POSITIVE,
        refusal::INVALID_JOB,
        refusal::INVALID_IMAGE,
        refusal::IMAGE_DIGEST_REQUIRED,
        refusal::INVALID_MOUNT,
        refusal::HOST_LIVE,
        refusal::HOST_TAINTED,
        refusal::STALE_EPOCH,
        refusal::UNKNOWN_HOST,
        refusal::NOT_HELD,
        refusal::HOST_LOST,
        refusal::NO_CAPACITY,
        refusal::HOST_NOT_REGISTERED,
        refusal::LIMITS_EXCEED_SHAPE,
        refusal::HOST_JOBS_NOT_ALLOWED,
        refusal::UNSUPPORTED_PROTOCOL,
        refusal::INVALID_HEARTBEAT,
        refusal::INVALID_WELCOME,
        refusal::INVALID_REPLY,
        refusal::UNKNOWN_POOL,
        refusal::SHAPE_NOT_ACCEPTED,
        refusal::HOST_CONTRACT,
        refusal::EXECUTION_MISMATCH,
        refusal::POOL_FULL,
        refusal::HOST_BOUND,
        refusal::SESSION_MISMATCH,
        refusal::SESSION_UNAVAILABLE,
        refusal::HOST_LISTED,
        refusal::RATE_LIMITED,
        refusal::BODY_TOO_LARGE,
        refusal::EXCLUSIVE_NOT_READY,
        refusal::NO_CAPABLE_HOST,
        refusal::NO_MATCHING_POOL,
        refusal::NOT_READY,
    ];
    assert_eq!(
        codes.join(" "),
        "invalid-hello reserved-label wall-time-not-positive invalid-job invalid-image \
         image-digest-required invalid-mount host-live host-tainted stale-epoch unknown-host not-held host-lost \
         no-capacity host-not-registered limits-exceed-shape host-jobs-not-allowed \
         unsupported-protocol invalid-heartbeat invalid-welcome invalid-reply unknown-pool \
         shape-not-accepted host-contract execution-mismatch pool-full host-bound session-mismatch session-unavailable host-listed rate-limited \
         body-too-large exclusive-not-ready no-capable-host no-matching-pool not-ready"
    );
}

/// Asserts that the grace relation holds for the defaults and is judged correctly for
/// other values: strictly more than two intervals, and no overflow at the extreme.
#[test]
fn grace_must_exceed_two_heartbeats() {
    assert!(grace_covers_heartbeats(
        DEFAULT_HEARTBEAT_INTERVAL_MS,
        DEFAULT_GRACE_MS
    ));
    assert!(grace_covers_heartbeats(10, 21));
    assert!(!grace_covers_heartbeats(10, 20));
    assert!(!grace_covers_heartbeats(u64::MAX, u64::MAX));
}

/// Asserts that the shape converts to and from `js_types::Shape` without loss, that the
/// lease cap is not part of the resource vector, and that a GPU class that does not match
/// the count is the conversion's error.
#[test]
fn shape_converts_both_ways() {
    let shape = hello().to_shape().unwrap();
    let expected = Shape::new(
        Resources::new(8000, 1 << 34, 1, Some("nvidia-a100".into())).unwrap(),
        common::map(&[("os", "macos")]),
    );
    assert_eq!(shape, expected);
    let mut wire = hello().shape;
    wire.max_leases = None;
    assert_eq!(WireShape::from_resources(shape.resources()), wire);
    let cpu_only = WireShape::from_resources(&Resources::new(1000, 2, 0, None).unwrap());
    assert_eq!(cpu_only.gpu_class, None);
    assert_eq!(cpu_only.to_resources().unwrap().cpu_millis(), 1000);
    let mut bad = hello();
    bad.shape.gpu_class = None;
    assert_eq!(bad.to_shape(), Err(TypeError::GpuClassMismatch));
}

/// Asserts that every variant of every enum survives encode and decode. It catches a
/// lost field or a renamed variant (and, as every arm runs, untested branches).
#[test]
fn round_trip_of_every_variant() {
    for phase in [
        Phase::Serving,
        Phase::Preparing,
        Phase::Restoring,
        Phase::MemoryPressure,
        Phase::Draining,
        Phase::Quarantined,
        Phase::Tainted,
    ] {
        round_trip(&phase);
    }
    for state in [
        HeldState::Starting,
        HeldState::Running,
        HeldState::ExitedUnacked,
    ] {
        round_trip(&state);
    }
    for outcome in [
        Outcome::Succeeded,
        Outcome::Failed,
        Outcome::Killed,
        Outcome::TimedOut,
        Outcome::OutOfMemory,
        Outcome::FailedToStart,
    ] {
        round_trip(&outcome);
    }
    for ack in [
        Ack::Accepted,
        Ack::Duplicate,
        Ack::Stale,
        Ack::UnknownLease,
        Ack::Mismatch,
    ] {
        round_trip(&ack);
    }
    for isolation in [Isolation::Contained, Isolation::Host] {
        round_trip(&isolation);
    }
    for command in [
        Command::Start(Box::new(job())),
        Command::Stop { run_id: "r".into() },
        Command::Cancel { run_id: "r".into() },
    ] {
        round_trip(&command);
    }
}

/// Asserts that every message survives encode and decode whole, with its optional
/// fields both set and left out, and that a left-out option is absent from the text.
#[test]
fn round_trip_of_every_message() {
    round_trip(&hello());
    round_trip(&job());
    round_trip(&heartbeat());
    round_trip(&heartbeat_with_load());
    round_trip(&welcome());
    round_trip(&HelloReply::Welcome(welcome()));
    round_trip(&HelloReply::Refused(Refused {
        code: "host-live".into(),
        supported: Some(ProtocolRange { min: 1, max: 2 }),
        field: Some("gpu-toolkit".into()),
    }));
    let mut bare = heartbeat();
    bare.taint = None;
    bare.results[0].exit_code = None;
    bare.results[0].detail = None;
    round_trip(&bare);
    let text = serde_json::to_string(&bare).unwrap();
    assert!(!text.contains("taint") && !text.contains("exit_code") && !text.contains("detail"));
    let mut cpu_only = hello();
    cpu_only.shape = WireShape {
        cpu_millis: 1,
        memory_bytes: 1,
        gpus: 0,
        gpu_class: None,
        max_leases: None,
    };
    round_trip(&cpu_only);
    let text = serde_json::to_string(&cpu_only).unwrap();
    assert!(!text.contains("gpu_class") && !text.contains("max_leases"));
    assert!(!text.contains("instance"));
    let mut plain = job();
    plain.limits.pids = None;
    plain.isolation = Isolation::Contained;
    round_trip(&plain);
    assert!(!serde_json::to_string(&plain).unwrap().contains("pids"));
    round_trip(&Taint {
        reason: "x".into(),
        run_id: None,
        check: None,
    });
    round_trip(&HeartbeatReply {
        commands: vec![Command::Start(Box::new(job()))],
        result_acks: vec![ResultAck {
            run_id: "r".into(),
            ack: Ack::Stale,
        }],
        taint_ack: true,
        exclusive: None,
    });
}
