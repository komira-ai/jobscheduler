//! The `kind` of a job start on the wire ([`StartKind`]): absent for the agent's own
//! runtime, `vm` for a fresh VM. The field is additive: a start without it reads as it did,
//! an action start encodes as it did, and an unknown kind costs only its own start.

use js_agent_wire::{BookedResources, Command, HeartbeatReply, JobStartSpec, StartKind};
use js_types::sched::LeaseKind;
use serde_json::{Value, json};

const IMAGE: &str =
    "macos:15@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with its required fields and `kind`.
fn start(kind: StartKind) -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "alloc-1".into(),
        image: IMAGE.into(),
        command: vec!["xcodebuild".into()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind,
        isolation: Default::default(),
    }
}

/// The encoding of a start of a server that predates the field.
fn old_body() -> Value {
    json!({
        "job_id": "job-1",
        "allocation_id": "alloc-1",
        "image": IMAGE,
        "command": ["xcodebuild"],
        "booked": { "cpu_millis": 4000, "memory_bytes": 8u64 << 30 },
        "wall_ms": 60_000
    })
}

/// Asserts that a start of a server that predates the field decodes as an action and
/// validates, and that an action start encodes to exactly the old body (no `kind` key).
/// Catches a field an old server's start fails on, and a default that changes the bytes
/// every action start sends.
#[test]
fn a_start_without_kind_is_an_action_and_encodes_as_before() {
    let old: JobStartSpec = serde_json::from_value(old_body()).unwrap();
    assert_eq!(old.kind, StartKind::Action);
    assert_eq!(old.validate(), Ok(()));
    assert_eq!(old, start(StartKind::Action));
    assert_eq!(
        serde_json::to_value(start(StartKind::Action)).unwrap(),
        old_body()
    );
}

/// Asserts that a VM start encodes `"kind": "vm"` beside the old body, decodes back to
/// itself and validates, and that the spelling is the lease kind's. Catches a VM start that
/// reads as an action on the agent (the job would run on the host, not in a VM) and a
/// spelling the scheduler's lease kind does not share.
#[test]
fn a_vm_start_says_kind_vm_and_reads_back() {
    let mut body = old_body();
    body["kind"] = json!("vm");
    assert_eq!(serde_json::to_value(start(StartKind::Vm)).unwrap(), body);
    let read: JobStartSpec = serde_json::from_value(body).unwrap();
    assert_eq!(read, start(StartKind::Vm));
    assert_eq!(read.validate(), Ok(()));
    assert_eq!(LeaseKind::Vm.name(), "vm");
    assert_eq!(LeaseKind::Action.name(), "action");
    let action: StartKind = serde_json::from_value(json!("action")).unwrap();
    assert!(action.is_action());
    assert!(!StartKind::Vm.is_action());
}

/// Asserts that a start of a kind this version cannot name fails to decode, and inside a
/// reply reads as `Unknown` while the stop beside it arrives intact. Catches an unknown kind
/// read as an action (an agent would run on the host what was meant for something else)
/// and one bad start that costs the whole reply.
#[test]
fn an_unknown_kind_costs_only_its_own_start() {
    let mut body = old_body();
    body["kind"] = json!("pod");
    assert!(serde_json::from_value::<JobStartSpec>(body.clone()).is_err());
    body["type"] = json!("job_start");
    let reply: HeartbeatReply = serde_json::from_value(json!({
        "commands": [body, { "type": "job_stop", "job_id": "job-0" }]
    }))
    .unwrap();
    assert_eq!(
        reply.commands,
        [
            Command::Unknown,
            Command::JobStop {
                job_id: "job-0".into()
            }
        ]
    );
}
