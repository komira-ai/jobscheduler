//! The `xcode` of a job start on the wire: absent for a job that names no Xcode, the build id
//! (`16C5032a`) for one that does. The field is additive: a start without it reads and
//! encodes as it did, and a build that is empty, holds a `,` (which the capability joins
//! builds with), starts with a dot or is longer than 32 bytes is refused.

mod common;

use common::expect;
use js_agent_wire::limits::{MAX_XCODE_BUILD_LEN, XCODE_CAPABILITY};
use js_agent_wire::{BookedResources, JobStartSpec, Rule, is_xcode_build, refusal};
use serde_json::{Value, json};

const IMAGE: &str =
    "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with its required fields and the Xcode `xcode`.
fn start(xcode: Option<&str>) -> JobStartSpec {
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
        isolation: Default::default(),
        fuse: false,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        placed_at_ms: None,
        xcode: xcode.map(str::to_owned),
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

/// Asserts that a start with a build encodes `"xcode":"16C5032a"` beside the old body, decodes
/// back to itself and validates, and that the capability a Mac reports the builds under is
/// `xcode`. Catches a field renamed on the wire.
#[test]
fn an_xcode_start_encodes_the_build_and_reads_back() {
    let mut body = old_body();
    body["xcode"] = json!("16C5032a");
    let spec = start(Some("16C5032a"));
    assert_eq!(serde_json::to_value(&spec).unwrap(), body);
    assert!(
        serde_json::to_string(&spec)
            .unwrap()
            .contains(r#""xcode":"16C5032a""#)
    );
    let back: JobStartSpec = serde_json::from_value(body).unwrap();
    assert_eq!(back, spec);
    assert_eq!(back.xcode.as_deref(), Some("16C5032a"));
    assert_eq!(back.validate(), Ok(()));
    assert_eq!(XCODE_CAPABILITY, "xcode");
}

/// Asserts that a start without a build encodes with no `xcode` key at all, exactly the old
/// body, and that a start of a server that predates the field decodes with none and validates.
#[test]
fn a_start_without_xcode_leaves_the_key_out_and_the_old_body_reads_as_none() {
    let spec = start(None);
    let body = serde_json::to_value(&spec).unwrap();
    assert!(body.as_object().unwrap().get("xcode").is_none());
    assert_eq!(body, old_body());
    assert!(!serde_json::to_string(&spec).unwrap().contains("xcode"));
    let old: JobStartSpec = serde_json::from_value(old_body()).unwrap();
    assert_eq!(old.xcode, None);
    assert_eq!(old.validate(), Ok(()));
    assert_eq!(old, spec);
}

/// Asserts that a build that is empty, holds a comma, starts with a dot or a dash, holds a
/// space or is 33 bytes is refused with code `invalid-job` naming field `xcode`, and that
/// builds of one byte, of exactly 32 bytes, and with dots, dashes and underscores inside are
/// accepted. Catches a build that would split into two in the host's comma-joined set.
#[test]
fn a_malformed_xcode_build_is_refused_naming_the_field() {
    let long = "a".repeat(MAX_XCODE_BUILD_LEN + 1);
    let cases = [
        ("", Rule::Empty),
        ("16C5032a,16E140", Rule::BadChars),
        (".16C", Rule::BadChars),
        ("-16C", Rule::BadChars),
        ("16C 5032a", Rule::BadChars),
        (long.as_str(), Rule::TooLong),
    ];
    for (build, rule) in cases {
        let err = start(Some(build)).validate().unwrap_err();
        expect(err, "xcode", rule, refusal::INVALID_JOB);
    }
    let longest = "a".repeat(MAX_XCODE_BUILD_LEN);
    for build in ["1", "16C5032a", "1.2_3-4", longest.as_str()] {
        assert_eq!(start(Some(build)).validate(), Ok(()), "{build}");
    }
}

/// Asserts that `is_xcode_build` answers as the start's check does on the same builds, so a
/// server that checks a submitted build with it never builds a start the agent refuses.
#[test]
fn is_xcode_build_agrees_with_the_start_check() {
    let long = "a".repeat(MAX_XCODE_BUILD_LEN + 1);
    let longest = "a".repeat(MAX_XCODE_BUILD_LEN);
    for build in [
        "",
        "16C5032a",
        "16C5032a,16E140",
        ".16C",
        "-16C",
        "16C 5032a",
        "1",
        "1.2_3-4",
        long.as_str(),
        longest.as_str(),
    ] {
        let accepted = start(Some(build)).validate().is_ok();
        assert_eq!(is_xcode_build(build), accepted, "{build:?}");
    }
    assert!(is_xcode_build("16C5032a"));
    assert!(!is_xcode_build("16C5032a,16E140"));
}
