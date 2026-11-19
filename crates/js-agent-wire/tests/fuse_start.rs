//! The `fuse` of a job start on the wire: absent for false, `true` for a job that is given
//! the FUSE device. The field is additive: a start without it reads as it did, a start with
//! false encodes as it did, and a host start cannot ask for it.

mod common;

use common::expect;
use js_agent_wire::{BookedResources, Isolation, JobStartSpec, Rule, refusal};
use serde_json::{Value, json};

const IMAGE: &str =
    "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with its required fields, `isolation` and `fuse`.
fn start(isolation: Isolation, fuse: bool) -> JobStartSpec {
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
        fuse,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
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

/// Asserts that a start with `fuse` encodes `"fuse": true` beside the old body, decodes back
/// to itself and validates.
#[test]
fn a_fuse_start_encodes_the_key_and_reads_back() {
    let mut body = old_body();
    body["fuse"] = json!(true);
    let spec = start(Isolation::Contained, true);
    assert_eq!(serde_json::to_value(&spec).unwrap(), body);
    let back: JobStartSpec = serde_json::from_value(body).unwrap();
    assert_eq!(back, spec);
    assert!(back.fuse);
    assert_eq!(back.validate(), Ok(()));
}

/// Asserts that a start without `fuse` encodes with no `fuse` key at all, exactly the old
/// body, so every start of a server that does not ask for the device is byte for byte what
/// it was.
#[test]
fn a_start_without_fuse_leaves_the_key_out() {
    let spec = start(Isolation::Contained, false);
    let body = serde_json::to_value(&spec).unwrap();
    assert!(body.as_object().unwrap().get("fuse").is_none());
    assert_eq!(body, old_body());
    assert!(!serde_json::to_string(&spec).unwrap().contains("fuse"));
}

/// Asserts that a start of a server that predates the field decodes with `fuse` false and
/// validates, and that an explicit `false` reads the same.
#[test]
fn a_start_of_the_previous_release_has_no_fuse() {
    let old: JobStartSpec = serde_json::from_value(old_body()).unwrap();
    assert!(!old.fuse);
    assert_eq!(old.validate(), Ok(()));
    assert_eq!(old, start(Isolation::Contained, false));
    let mut body = old_body();
    body["fuse"] = json!(false);
    let explicit: JobStartSpec = serde_json::from_value(body).unwrap();
    assert_eq!(explicit, old);
}

/// Asserts that a host start with `fuse` is refused with field `fuse`, rule `Inconsistent`
/// and the `invalid-job` code, that a host start without it and a contained start with it
/// validate. Catches a host job given a device it already has the host's own of.
#[test]
fn fuse_is_not_allowed_with_host_isolation() {
    let err = start(Isolation::Host, true).validate().unwrap_err();
    expect(err, "fuse", Rule::Inconsistent, refusal::INVALID_JOB);
    assert_eq!(start(Isolation::Host, false).validate(), Ok(()));
    assert_eq!(start(Isolation::Contained, true).validate(), Ok(()));
}
