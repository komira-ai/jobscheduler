//! The start fence on the wire: a hello capability and nothing else. The fence of a job
//! start is the pair it already carries (its job id and its allocation id), so the bodies
//! a deployed agent or server sends are what they were.

use js_agent_wire::{BookedResources, Hello, JobStartSpec, START_FENCE_CAPABILITY};
use serde_json::{Value, json};

const HELLO: &str = include_str!("fixtures/v0.1.135/hello.json");

const IMAGE: &str =
    "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with its required fields only.
fn bare_start() -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "alloc-1".into(),
        image: IMAGE.into(),
        command: Vec::new(),
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
    }
}

/// Asserts that the encodings the wire already sends are unchanged: a job start with its
/// required fields encodes to exactly the keys it had (no fence key: the fence is the job
/// id with the allocation id), the previous release's hello (the file under
/// `fixtures/v0.1.135`, which this change does not touch) decodes, validates and encodes
/// back to the same JSON with no `start-fence`, and the capability is named `start-fence`.
/// It catches a fence added as a field of the start (an older agent would ignore it, so it
/// could not carry a guarantee), a default that makes an old body encode differently, and
/// a renamed capability that a server would never find.
#[test]
fn old_encodings_are_unchanged() {
    assert_eq!(
        serde_json::to_value(bare_start()).unwrap(),
        json!({
            "job_id": "job-1",
            "allocation_id": "alloc-1",
            "image": IMAGE,
            "booked": { "cpu_millis": 1000, "memory_bytes": 1_073_741_824u64 },
            "wall_ms": 60_000
        })
    );
    let fixture: Value = serde_json::from_str(HELLO).unwrap();
    let hello: Hello = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(hello.validate(), Ok(()));
    assert!(!hello.capabilities.contains_key(START_FENCE_CAPABILITY));
    assert_eq!(serde_json::to_value(&hello).unwrap(), fixture);
    assert_eq!(START_FENCE_CAPABILITY, "start-fence");
}

/// Asserts that the capability is one more entry of a hello's capabilities and nothing
/// else: the previous release's hello with `start-fence` set to `true` validates, and its
/// encoding differs from the file only by that entry. It catches a capability value the
/// hello's own check refuses (every agent that reports it would be refused), and a change
/// that moves anything else in the hello.
#[test]
fn the_capability_is_only_a_hello_entry() {
    let fixture: Value = serde_json::from_str(HELLO).unwrap();
    let mut hello: Hello = serde_json::from_value(fixture.clone()).unwrap();
    hello
        .capabilities
        .insert(START_FENCE_CAPABILITY.to_owned(), "true".to_owned());
    assert_eq!(hello.validate(), Ok(()));
    let mut expected = fixture;
    expected["capabilities"][START_FENCE_CAPABILITY] = json!("true");
    assert_eq!(serde_json::to_value(&hello).unwrap(), expected);
}
