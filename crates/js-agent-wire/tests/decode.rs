//! Decoding: older (minimal) bodies, unknown fields and values, and the two values that
//! fail closed.

mod common;

use common::{heartbeat, hello, job, round_trip, welcome, with_unknown};
use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldState, Hello, HelloReply, Isolation, JobSpec,
    Outcome, Phase, ProtocolRange, ResultAck, Welcome,
};
use serde_json::{Value, json};

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

/// Asserts that an isolation value this version does not know fails to decode with an
/// error that names it, that the same start with a known value decodes under the pinned
/// `start` tag, and that inside a reply the unknown one reads as an unknown command
/// beside intact commands. It catches an older agent that would run a job it cannot
/// classify, and a wrong tag (which would turn every start into an unknown command).
#[test]
fn unknown_isolation_is_typed_error_not_contained() {
    let mut body = serde_json::to_value(job()).unwrap();
    body["isolation"] = json!("vm");
    let err = serde_json::from_value::<JobSpec>(body.clone()).unwrap_err();
    assert!(err.is_data());
    assert!(err.to_string().contains("unknown variant `vm`"), "{err}");
    let tagged = |isolation: &str| {
        let mut command = json!({"type": "start"});
        let mut fields = body.as_object().unwrap().clone();
        fields.insert("isolation".into(), json!(isolation));
        command.as_object_mut().unwrap().extend(fields);
        command
    };
    let known: Command = serde_json::from_value(tagged("host")).unwrap();
    assert!(matches!(&known, Command::Start(spec) if spec.isolation == Isolation::Host));
    let stop = json!({"type": "stop", "run_id": "r"});
    let reply = json!({"commands": [tagged("vm"), stop]});
    let reply: HeartbeatReply = serde_json::from_value(reply).unwrap();
    assert_eq!(
        reply.commands,
        vec![Command::Unknown, Command::Stop { run_id: "r".into() }]
    );
}

/// Asserts that a start with only its required fields decodes with every optional field
/// at its default. It catches a dropped `#[serde(default)]` on args, gpu_indices, mounts,
/// labels, heartbeat_seq or valid_for_ms, which would reject a start from an older server.
#[test]
fn minimal_start_takes_defaults() {
    let body = json!({
        "type": "start",
        "run_id": "r", "lease_id": "1-1", "image": "alpine",
        "limits": {"cpu_millis": 1, "memory_bytes": 1, "wall_ms": 1}
    });
    let Command::Start(spec) = serde_json::from_value(body.clone()).unwrap() else {
        panic!("not a start");
    };
    assert!(spec.args.is_empty() && spec.gpu_indices.is_empty() && spec.mounts.is_empty());
    assert!(spec.labels.is_empty());
    assert_eq!((spec.heartbeat_seq, spec.valid_for_ms), (0, 0));
    assert_eq!(spec.limits.pids, None);
    assert_eq!(spec.isolation, Isolation::Contained);
    assert_eq!(spec.validate(), Ok(()));
}

/// Asserts that a hello with only its required fields decodes with no capabilities,
/// devices, labels, held runs, instance or lease cap, and with the protocol range of
/// major 1 alone. It catches a dropped default, and a hello from before the protocol
/// field being read as a version it does not speak.
#[test]
fn minimal_hello_takes_defaults() {
    let hello: Hello = serde_json::from_value(json!({
        "host_id": "a", "pool": "p", "agent_version": "1",
        "shape": {"cpu_millis": 1, "memory_bytes": 2, "gpus": 0},
        "phase": "serving"
    }))
    .unwrap();
    assert!(hello.capabilities.is_empty());
    assert!(hello.labels.is_empty() && hello.held.is_empty() && hello.gpu_devices.is_empty());
    assert_eq!(hello.shape.gpu_class, None);
    assert_eq!(hello.shape.max_leases, None);
    assert_eq!(hello.instance, None);
    assert_eq!(hello.protocol, ProtocolRange { min: 1, max: 1 });
    assert_eq!(hello.validate(), Ok(()));
}

/// Asserts that a welcome with no `protocol` field names major 1, and that a heartbeat,
/// a reply and an empty reply decode from their required fields alone, with an empty
/// `session` on the welcome and the heartbeat (which the agent's welcome check and the
/// registry then refuse). It catches a server of the first version being read as
/// speaking no version, and an old body failing to decode instead of being refused by
/// name.
#[test]
fn minimal_welcome_heartbeat_and_reply_take_defaults() {
    let welcome: Welcome = serde_json::from_value(json!({
        "epoch": 1, "heartbeat_interval_ms": 5000, "grace_ms": 15000, "pool": "p"
    }))
    .unwrap();
    assert_eq!(welcome.protocol, 1);
    assert_eq!(welcome.session, "");
    let beat: Heartbeat =
        serde_json::from_value(json!({"host_id": "a", "epoch": 1, "phase": "serving"})).unwrap();
    assert!(beat.held.is_empty() && beat.results.is_empty() && beat.taint.is_none());
    assert_eq!(beat.session, "");
    let reply: HeartbeatReply = serde_json::from_value(json!({"commands": []})).unwrap();
    assert_eq!(reply, HeartbeatReply::default());
    let reply: HeartbeatReply = serde_json::from_value(json!({})).unwrap();
    assert!(reply.commands.is_empty() && reply.result_acks.is_empty() && !reply.taint_ack);
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

/// Asserts that the phase text `memory-pressure` reads as memory pressure and the
/// underscore spelling does not. It catches a spelling that differs from the one the
/// design gives.
#[test]
fn memory_pressure_phase_uses_the_design_spelling() {
    let phase = serde_json::from_value::<Phase>(json!("memory-pressure")).unwrap();
    assert_eq!(phase, Phase::MemoryPressure);
    assert_eq!(
        serde_json::to_value(Phase::MemoryPressure).unwrap(),
        json!("memory-pressure")
    );
    let old = serde_json::from_value::<Phase>(json!("memory_pressure")).unwrap();
    assert_eq!(old, Phase::Quarantined);
}

/// Asserts that an unknown field, at the top and nested, is ignored when decoding a
/// hello, a welcome, a hello reply, a heartbeat, a reply (with a start inside) and a
/// result ack. It catches a `deny_unknown_fields` that would break a mixed-version fleet.
#[test]
fn unknown_fields_ignored_everywhere() {
    let back: Hello = serde_json::from_value(with_unknown(&hello(), &[])).unwrap();
    assert_eq!(back, hello());
    let nested = with_unknown(&hello(), &["shape"]);
    assert_eq!(serde_json::from_value::<Hello>(nested).unwrap(), hello());
    let nested = with_unknown(&hello(), &["protocol"]);
    assert_eq!(serde_json::from_value::<Hello>(nested).unwrap(), hello());
    let back: Welcome = serde_json::from_value(with_unknown(&welcome(), &[])).unwrap();
    assert_eq!(back, welcome());
    let reply = HelloReply::Welcome(welcome());
    let back: HelloReply = serde_json::from_value(with_unknown(&reply, &[])).unwrap();
    assert_eq!(back, reply);
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

/// Asserts that an unknown command kind, an unknown ack verdict, an unknown outcome and
/// an unknown held state each read as `Unknown` while the known entries beside them
/// decode, and that the unknown values encode to text that decodes as unknown again. It
/// catches a newer peer's new kind failing a whole reply or heartbeat, which would drop
/// every stop and acknowledgement in it.
#[test]
fn unknown_enum_values_leave_known_entries_intact() {
    let reply: HeartbeatReply = serde_json::from_value(json!({
        "commands": [
            {"type": "pause", "run_id": "a"},
            {"type": "stop", "run_id": "b"},
            {"type": "cancel", "run_id": "c"}
        ],
        "result_acks": [
            {"run_id": "a", "ack": "REQUEUED"},
            {"run_id": "b", "ack": "ACCEPTED"}
        ]
    }))
    .unwrap();
    assert_eq!(
        reply.commands,
        vec![
            Command::Unknown,
            Command::Stop { run_id: "b".into() },
            Command::Cancel { run_id: "c".into() },
        ]
    );
    assert_eq!(reply.result_acks[0].ack, Ack::Unknown);
    assert_eq!(reply.result_acks[1].ack, Ack::Accepted);

    let mut body: Value = serde_json::to_value(heartbeat()).unwrap();
    body["results"][0]["outcome"] = json!("preempted");
    body["held"][0]["state"] = json!("paused");
    let beat: Heartbeat = serde_json::from_value(body).unwrap();
    assert_eq!(beat.results[0].outcome, Outcome::Unknown);
    assert_eq!(beat.held[0].state, HeldState::Unknown);
    assert_eq!(beat.results[0].run_id, heartbeat().results[0].run_id);

    round_trip(&Command::Unknown);
    round_trip(&Ack::Unknown);
    round_trip(&Outcome::Unknown);
    round_trip(&HeldState::Unknown);
}

/// Asserts that a hello reply of an unknown `type` fails to decode, as does one with no
/// `type`. It catches a refusal being read as a welcome by an agent that guesses.
#[test]
fn hello_reply_of_unknown_type_is_an_error() {
    assert!(serde_json::from_value::<HelloReply>(json!({"type": "maybe"})).is_err());
    assert!(serde_json::from_value::<HelloReply>(json!({"epoch": 1})).is_err());
}
