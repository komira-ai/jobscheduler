//! Tests of the agent wire: decoding rules, validators, codes and round trips.

use std::collections::BTreeMap;
use std::fmt::Debug;

use js_agent_wire::refusal;
use js_agent_wire::{
    Ack, Command, DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS, GpuDevice, Heartbeat,
    HeartbeatReply, HeldRun, HeldState, Hello, HelloError, Isolation, JobSpec, JobSpecError,
    Limits, Mount, MountError, Outcome, Phase, ResultAck, RunResult, Taint, Welcome, WireShape,
    grace_covers_heartbeats,
};
use js_types::{LabelError, Resources, Shape, TypeError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn hello() -> Hello {
    Hello {
        host_id: "mac-01".into(),
        pool: "builders".into(),
        agent_version: "0.1.0".into(),
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 1 << 34,
            gpus: 1,
            gpu_class: Some("nvidia-a100".into()),
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

fn job() -> JobSpec {
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

fn heartbeat() -> Heartbeat {
    Heartbeat {
        host_id: "mac-01".into(),
        epoch: 3,
        phase: Phase::Draining,
        held: vec![HeldRun {
            run_id: "run-1".into(),
            state: HeldState::ExitedUnacked,
        }],
        results: vec![RunResult {
            run_id: "run-1".into(),
            lease_id: "1-1".into(),
            outcome: Outcome::TimedOut,
            exit_code: Some(137),
            detail: Some("wall limit".into()),
        }],
        taint: Some(Taint {
            reason: "cleanup-failed".into(),
            run_id: Some("run-1".into()),
            check: Some("new_mount".into()),
        }),
    }
}

/// Encodes `value` and decodes it again.
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let text = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&back, value);
}

/// `value` as JSON with an unknown field added to the object at `path` (empty: the root).
fn with_unknown<T: Serialize>(value: &T, path: &[&str]) -> Value {
    let mut json = serde_json::to_value(value).unwrap();
    let mut at = &mut json;
    for key in path {
        at = &mut at[*key];
    }
    at["field_from_the_future"] = json!({"nested": [1, 2]});
    json
}

/// Asserts that a start with no `isolation` field decodes as contained. It catches a
/// Host default, or a decode error, on a start from an older server that never sends the
/// field.
#[test]
fn isolation_absent_is_contained() {
    let mut body = serde_json::to_value(job()).unwrap();
    body.as_object_mut().unwrap().remove("isolation");
    let spec: JobSpec = serde_json::from_value(body).unwrap();
    assert_eq!(spec.isolation, Isolation::Contained);
    assert_eq!(Isolation::default(), Isolation::Contained);
}

/// Asserts that an isolation value this version does not know fails to decode, alone
/// and inside a reply, and does not read as contained. It catches an older agent that
/// would run a job it cannot classify.
#[test]
fn unknown_isolation_is_typed_error_not_contained() {
    let mut body = serde_json::to_value(job()).unwrap();
    body["isolation"] = json!("vm");
    let err = serde_json::from_value::<JobSpec>(body.clone()).unwrap_err();
    assert!(err.is_data());
    assert!(err.to_string().contains("unknown variant `vm`"), "{err}");
    let mut command = json!({"type": "start"});
    command
        .as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    let reply = json!({"commands": [command]});
    assert!(serde_json::from_value::<HeartbeatReply>(reply).is_err());
}

/// Asserts that a phase text this version does not know reads as quarantined, an empty
/// text too, and that a known phase keeps its meaning. It catches a skip that could
/// place work on a host that is not serving.
#[test]
fn unknown_phase_reads_quarantined() {
    let phase = |text: &str| serde_json::from_value::<Phase>(json!(text)).unwrap();
    assert_eq!(phase("hibernating"), Phase::Quarantined);
    assert_eq!(phase(""), Phase::Quarantined);
    assert_eq!(phase("serving"), Phase::Serving);
    let mut body = serde_json::to_value(heartbeat()).unwrap();
    body["phase"] = json!("something-new");
    let beat: Heartbeat = serde_json::from_value(body).unwrap();
    assert_eq!(beat.phase, Phase::Quarantined);
}

/// Asserts that a hello with a label under `jobscheduler.dev/` is refused with the
/// reserved-label code, while another label fault keeps the generic code. It catches a
/// host that could forge a scheduler-written label.
#[test]
fn hello_reserved_label_refused() {
    let mut bad = hello();
    bad.labels = map(&[("jobscheduler.dev/tenant", "x")]);
    let err = bad.validate().unwrap_err();
    assert_eq!(
        err,
        HelloError::Label(LabelError::Reserved("jobscheduler.dev/tenant".into()))
    );
    assert_eq!(err.code(), refusal::RESERVED_LABEL);
    assert!(err.to_string().starts_with("labels:"));
    bad.labels = map(&[("", "x")]);
    let err = bad.validate().unwrap_err();
    assert_eq!(err, HelloError::Label(LabelError::EmptyKey));
    assert_eq!(err.code(), refusal::INVALID_HELLO);
}

/// Asserts that a hello counting GPUs without a class, or naming a class for no GPUs,
/// is refused. It catches a host booked as GPU capacity of no kind.
#[test]
fn gpus_without_class_refused() {
    let mut bad = hello();
    bad.shape.gpu_class = None;
    let err = bad.validate().unwrap_err();
    assert_eq!(err, HelloError::Shape(TypeError::GpuClassMismatch));
    assert_eq!(err.code(), refusal::INVALID_HELLO);
    assert_eq!(bad.to_shape(), Err(TypeError::GpuClassMismatch));
    bad.shape.gpus = 0;
    bad.shape.gpu_class = Some("nvidia-a100".into());
    assert_eq!(
        bad.validate(),
        Err(HelloError::Shape(TypeError::GpuClassMismatch))
    );
}

/// Asserts that a hello is refused for a malformed host id, an empty pool and an empty
/// held run id, and accepted when whole. It catches an empty id reaching the registry.
#[test]
fn hello_empty_ids_refused() {
    assert_eq!(hello().validate(), Ok(()));
    let mut bad = hello();
    bad.host_id = String::new();
    let err = bad.validate().unwrap_err();
    assert!(matches!(err, HelloError::HostId(_)));
    assert!(err.to_string().starts_with("host_id:"));
    assert_eq!(err.code(), refusal::INVALID_HELLO);
    bad.host_id = "Upper".into();
    assert!(matches!(bad.validate(), Err(HelloError::HostId(_))));
    let mut bad = hello();
    bad.pool = String::new();
    assert_eq!(bad.validate(), Err(HelloError::EmptyId("pool")));
    let mut bad = hello();
    bad.held[0].run_id = String::new();
    let err = bad.validate().unwrap_err();
    assert_eq!(err, HelloError::EmptyId("held run_id"));
    assert_eq!(err.to_string(), "held run_id must not be empty");
}

/// Asserts that the shape converts to and from `js_types::Shape` without loss.
#[test]
fn shape_converts_both_ways() {
    let shape = hello().to_shape().unwrap();
    let expected = Shape::new(
        Resources::new(8000, 1 << 34, 1, Some("nvidia-a100".into())).unwrap(),
        map(&[("os", "macos")]),
    );
    assert_eq!(shape, expected);
    assert_eq!(WireShape::from_resources(shape.resources()), hello().shape);
    let cpu_only = WireShape::from_resources(&Resources::new(1000, 2, 0, None).unwrap());
    assert_eq!(cpu_only.gpu_class, None);
    assert_eq!(cpu_only.to_resources().unwrap().cpu_millis(), 1000);
}

/// Asserts that a start whose wall-time limit is zero is refused with its own code,
/// while a limit of one passes. It closes the hole in the start contract (design 5.2): a
/// job with no time bound would hold its slot forever.
#[test]
fn jobspec_zero_wall_refused() {
    let mut bad = job();
    bad.limits.wall_ms = 0;
    let err = bad.validate().unwrap_err();
    assert_eq!(err, JobSpecError::WallTimeNotPositive);
    assert_eq!(err.code(), refusal::WALL_TIME_NOT_POSITIVE);
    assert_eq!(err.to_string(), "limits.wall_ms must be above zero");
    bad.limits.wall_ms = 1;
    assert_eq!(bad.validate(), Ok(()));
}

/// Asserts that a job with an empty run id, lease id or image is refused with the code
/// of its kind. It catches a start that could not be keyed or run.
#[test]
fn jobspec_empty_ids_and_image_refused() {
    let mut bad = job();
    bad.run_id = String::new();
    let err = bad.validate().unwrap_err();
    assert_eq!(err, JobSpecError::EmptyId("run_id"));
    assert_eq!(err.code(), refusal::INVALID_JOB);
    let mut bad = job();
    bad.lease_id = String::new();
    assert_eq!(bad.validate(), Err(JobSpecError::EmptyId("lease_id")));
    let mut bad = job();
    bad.image = String::new();
    let err = bad.validate().unwrap_err();
    assert_eq!(err, JobSpecError::EmptyImage);
    assert_eq!(err.code(), refusal::INVALID_IMAGE);
}

/// Asserts that a mount that is absolute, climbs with `..` (first, middle or alone),
/// holds a NUL byte, or names no root is refused, and that a plain, a dotted and an
/// empty relative path pass. It catches a message reaching outside the agent's roots.
#[test]
fn mount_dotdot_or_absolute_refused() {
    let mount = |root: &str, rel: &str| Mount {
        root_name: root.into(),
        rel: rel.into(),
        ro: false,
    };
    for rel in ["/etc", "../x", "a/../../b", "..", "a/..", "a\0b"] {
        assert_eq!(
            mount("cache", rel).validate(),
            Err(MountError::Escapes(rel.into())),
            "{rel:?}"
        );
    }
    for root in ["", "a/b"] {
        assert_eq!(
            mount(root, "x").validate(),
            Err(MountError::BadRoot(root.into()))
        );
    }
    for rel in ["", "a", "./a/b", "a..b", "..a"] {
        assert_eq!(mount("cache", rel).validate(), Ok(()), "{rel:?}");
    }
    let mut bad = job();
    bad.mounts.push(mount("cache", "../x"));
    let err = bad.validate().unwrap_err();
    assert_eq!(
        err,
        JobSpecError::Mount {
            index: 1,
            source: MountError::Escapes("../x".into())
        }
    );
    assert_eq!(err.code(), refusal::INVALID_MOUNT);
    assert!(err.to_string().starts_with("mounts[1]:"));
    assert_eq!(job().validate(), Ok(()));
}

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
    ];
    assert_eq!(
        codes.join(" "),
        "invalid-hello reserved-label wall-time-not-positive invalid-job invalid-image \
         invalid-mount host-live host-tainted stale-epoch unknown-host not-held host-lost \
         no-capacity host-not-registered limits-exceed-shape host-jobs-not-allowed"
    );
}

/// Asserts that an unknown field, at the top and nested, is ignored when decoding a
/// hello, a welcome, a heartbeat, a reply (with a start inside) and a result ack. It
/// catches a `deny_unknown_fields` that would break a mixed-version fleet.
#[test]
fn unknown_fields_ignored_on_hello_heartbeat_and_reply() {
    let back: Hello = serde_json::from_value(with_unknown(&hello(), &[])).unwrap();
    assert_eq!(back, hello());
    let nested = with_unknown(&hello(), &["shape"]);
    assert_eq!(serde_json::from_value::<Hello>(nested).unwrap(), hello());
    let welcome = Welcome {
        epoch: 1,
        heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
        grace_ms: DEFAULT_GRACE_MS,
        pool: "builders".into(),
    };
    let back: Welcome = serde_json::from_value(with_unknown(&welcome, &[])).unwrap();
    assert_eq!(back, welcome);
    let back: Heartbeat = serde_json::from_value(with_unknown(&heartbeat(), &[])).unwrap();
    assert_eq!(back, heartbeat());
    let nested = with_unknown(&heartbeat(), &["taint"]);
    assert_eq!(
        serde_json::from_value::<Heartbeat>(nested).unwrap(),
        heartbeat()
    );
    let reply = HeartbeatReply {
        commands: vec![Command::Start(Box::new(job()))],
        result_acks: vec![ResultAck {
            run_id: "run-1".into(),
            ack: Ack::Accepted,
        }],
        taint_ack: true,
    };
    let mut body = with_unknown(&reply, &[]);
    body["commands"][0]["field_from_the_future"] = json!(true);
    body["result_acks"][0]["field_from_the_future"] = json!(true);
    assert_eq!(
        serde_json::from_value::<HeartbeatReply>(body).unwrap(),
        reply
    );
}

/// Asserts that a reply from an older server that sends only `commands`, and an empty
/// object, decode with the other fields at their defaults, and that a hello and a
/// heartbeat without their optional lists decode. It catches a newer agent failing on an
/// older server's reply.
#[test]
fn missing_optional_fields_take_defaults() {
    let reply: HeartbeatReply = serde_json::from_value(json!({"commands": []})).unwrap();
    assert_eq!(reply, HeartbeatReply::default());
    let reply: HeartbeatReply = serde_json::from_value(json!({})).unwrap();
    assert!(reply.commands.is_empty() && reply.result_acks.is_empty() && !reply.taint_ack);
    let hello: Hello = serde_json::from_value(json!({
        "host_id": "a", "pool": "p", "agent_version": "1",
        "shape": {"cpu_millis": 1, "memory_bytes": 2, "gpus": 0},
        "phase": "serving"
    }))
    .unwrap();
    assert!(hello.labels.is_empty() && hello.held.is_empty() && hello.gpu_devices.is_empty());
    assert_eq!(hello.shape.gpu_class, None);
    let beat: Heartbeat =
        serde_json::from_value(json!({"host_id": "a", "epoch": 1, "phase": "serving"})).unwrap();
    assert!(beat.held.is_empty() && beat.results.is_empty() && beat.taint.is_none());
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

/// Asserts that every variant of every enum survives encode and decode, and that the
/// wire text of the names is the one peers match on. It catches a lost field, a renamed
/// variant, and (as every arm runs) untested branches.
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
    assert_eq!(
        serde_json::to_string(&[Phase::MemoryPressure, Phase::Tainted]).unwrap(),
        r#"["memory_pressure","tainted"]"#
    );
    for state in [
        HeldState::Starting,
        HeldState::Running,
        HeldState::ExitedUnacked,
    ] {
        round_trip(&state);
    }
    assert_eq!(
        serde_json::to_string(&HeldState::ExitedUnacked).unwrap(),
        r#""exited_unacked""#
    );
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
    let acks = [
        Ack::Accepted,
        Ack::Duplicate,
        Ack::Stale,
        Ack::UnknownLease,
        Ack::Mismatch,
    ];
    for ack in acks {
        round_trip(&ack);
    }
    assert_eq!(
        serde_json::to_string(&acks).unwrap(),
        r#"["ACCEPTED","DUPLICATE","STALE","UNKNOWN_LEASE","MISMATCH"]"#
    );
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
    let kinds: Vec<Value> = [
        Command::Stop { run_id: "r".into() },
        Command::Cancel { run_id: "r".into() },
    ]
    .iter()
    .map(|c| serde_json::to_value(c).unwrap())
    .collect();
    assert_eq!(kinds[0], json!({"type": "stop", "run_id": "r"}));
    assert_eq!(kinds[1], json!({"type": "cancel", "run_id": "r"}));
}

/// Asserts that every message survives encode and decode whole, with its optional
/// fields both set and left out, and that a left-out option is absent from the text.
#[test]
fn round_trip_of_every_message() {
    round_trip(&hello());
    round_trip(&job());
    round_trip(&heartbeat());
    round_trip(&Welcome {
        epoch: 9,
        heartbeat_interval_ms: 5000,
        grace_ms: 15_000,
        pool: "builders".into(),
    });
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
    };
    round_trip(&cpu_only);
    assert!(
        !serde_json::to_string(&cpu_only)
            .unwrap()
            .contains("gpu_class")
    );
    let mut plain = job();
    plain.limits.pids = None;
    plain.isolation = Isolation::Contained;
    round_trip(&plain);
    assert!(!serde_json::to_string(&plain).unwrap().contains("pids"));
    let taint = Taint {
        reason: "x".into(),
        run_id: None,
        check: None,
    };
    round_trip(&taint);
    assert_eq!(
        serde_json::to_value(&taint).unwrap(),
        json!({"reason": "x"})
    );
    round_trip(&HeartbeatReply {
        commands: vec![Command::Start(Box::new(job()))],
        result_acks: vec![ResultAck {
            run_id: "r".into(),
            ack: Ack::Stale,
        }],
        taint_ack: true,
    });
}
