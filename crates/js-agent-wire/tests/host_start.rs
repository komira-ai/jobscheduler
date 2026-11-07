//! The `isolation` of a job start on the wire ([`Isolation`]): absent for contained, `host`
//! for a job that runs on the machine itself. The field is additive: a start without it reads
//! as it did, a contained start encodes as it did, and an unknown value costs only its own
//! start. The `host-jobs` hello capability is one more hello entry and nothing else.

use js_agent_wire::{
    BookedResources, Command, HOST_JOBS_CAPABILITY, HeartbeatReply, Hello, Isolation, JobStartSpec,
};
use serde_json::{Value, json};

const HELLO: &str = include_str!("fixtures/v0.1.135/hello.json");

const IMAGE: &str =
    "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with its required fields and `isolation`.
fn start(isolation: Isolation) -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "alloc-1".into(),
        image: IMAGE.into(),
        command: vec!["make".into()],
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
        kind: Default::default(),
        isolation,
    }
}

/// The encoding of a start of a server that predates the field.
fn old_body() -> Value {
    json!({
        "job_id": "job-1",
        "allocation_id": "alloc-1",
        "image": IMAGE,
        "command": ["make"],
        "booked": { "cpu_millis": 4000, "memory_bytes": 8u64 << 30 },
        "wall_ms": 60_000
    })
}

/// Asserts that a start of a server that predates the field decodes as contained, validates
/// and encodes back to exactly the old body (no `isolation` key); that a host start encodes
/// `"isolation": "host"` beside the old body, reads back to itself and validates; and that a
/// value this version cannot name fails to decode, and inside a reply reads as `Unknown`
/// while the stop beside it arrives intact. Catches a field an old server's start fails on,
/// a default that changes the bytes every contained start sends, a host start that reads as
/// contained, and one bad start that costs the whole reply.
#[test]
fn isolation_is_additive_on_the_wire_and_in_the_proto() {
    let old: JobStartSpec = serde_json::from_value(old_body()).unwrap();
    assert_eq!(old.isolation, Isolation::Contained);
    assert_eq!(old.validate(), Ok(()));
    assert_eq!(old, start(Isolation::Contained));
    assert_eq!(serde_json::to_value(&old).unwrap(), old_body());

    let mut host_body = old_body();
    host_body["isolation"] = json!("host");
    assert_eq!(
        serde_json::to_value(start(Isolation::Host)).unwrap(),
        host_body
    );
    let host: JobStartSpec = serde_json::from_value(host_body).unwrap();
    assert_eq!(host, start(Isolation::Host));
    assert_eq!(host.validate(), Ok(()));

    let mut body = old_body();
    body["isolation"] = json!("privileged");
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

/// Asserts that only `contained` is the value a start leaves out. Catches a host start whose
/// field is skipped on encode (the agent would run it contained).
#[test]
fn only_contained_is_left_out_of_a_start() {
    assert!(Isolation::Contained.is_contained());
    assert!(!Isolation::Host.is_contained());
    assert_eq!(Isolation::default(), Isolation::Contained);
}

/// Asserts that the capability is named `host-jobs`, that the previous release's hello has no
/// such entry, and that the hello with it set to `true` validates and encodes to the file plus
/// that one entry. Catches a capability value the hello's own check refuses (every agent that
/// reports it would be refused), a renamed capability that a server would never find, and a
/// change that moves anything else in the hello.
#[test]
fn host_jobs_is_only_a_hello_entry() {
    assert_eq!(HOST_JOBS_CAPABILITY, "host-jobs");
    let fixture: Value = serde_json::from_str(HELLO).unwrap();
    let mut hello: Hello = serde_json::from_value(fixture.clone()).unwrap();
    assert!(!hello.capabilities.contains_key(HOST_JOBS_CAPABILITY));
    hello
        .capabilities
        .insert(HOST_JOBS_CAPABILITY.to_owned(), "true".to_owned());
    assert_eq!(hello.validate(), Ok(()));
    let mut expected = fixture;
    expected["capabilities"][HOST_JOBS_CAPABILITY] = json!("true");
    assert_eq!(serde_json::to_value(&hello).unwrap(), expected);
}
