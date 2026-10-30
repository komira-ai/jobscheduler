//! Scratch disk on the wire: the `disk_bytes` of a job's limits and of a host's shape.
//! Both are additive, so a peer from before the field still parses and is read as a job
//! with no disk bound and a host with no disk.

mod common;

use common::{broken, expect, hello, job, round_trip};
use js_agent_wire::limits::MAX_DISK_BYTES;
use js_agent_wire::refusal;
use js_agent_wire::{Hello, JobSpec, Limits, Rule, WireShape};
use js_types::Resources;
use serde_json::{Value, json};

fn check_job(edit: impl FnOnce(&mut JobSpec)) -> js_agent_wire::ValidationError {
    broken(&job(), edit, JobSpec::validate)
}

fn check_hello(edit: impl FnOnce(&mut Hello)) -> js_agent_wire::ValidationError {
    broken(&hello(), edit, Hello::validate)
}

fn shape_with_disk(disk_bytes: u64) -> WireShape {
    WireShape {
        disk_bytes,
        ..hello().shape
    }
}

/// Asserts the bounds of a job's `disk_bytes`: absent is valid, zero is refused as out of
/// range (a bound of nothing is not a bound), the maximum is valid, one past it is
/// refused, all under `invalid-job`. It catches a limit that accepts zero (an agent
/// would enforce a quota no write can fit) or one that lets the sum of a host's runs
/// overflow.
#[test]
fn a_job_disk_limit_is_above_zero_and_bounded() {
    let mut spec = job();
    spec.limits.disk_bytes = Some(MAX_DISK_BYTES);
    assert_eq!(spec.validate(), Ok(()));
    spec.limits.disk_bytes = Some(1);
    assert_eq!(spec.validate(), Ok(()));
    let err = check_job(|j| j.limits.disk_bytes = Some(0));
    expect(
        err,
        "limits.disk_bytes",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );
    let err = check_job(|j| j.limits.disk_bytes = Some(MAX_DISK_BYTES + 1));
    expect(
        err,
        "limits.disk_bytes",
        Rule::OutOfRange,
        refusal::INVALID_JOB,
    );
}

/// Asserts that the disk maximum keeps a sum over 2^18 jobs or hosts in a `u64`, as the
/// documented bound of the other two caps does, and is no smaller than the 2^46 - 1 the
/// design names. It catches a maximum raised to 2^46, whose 2^18 copies sum to 2^64.
#[test]
fn the_disk_maximum_keeps_a_sum_over_2_pow_18_in_u64() {
    assert!(MAX_DISK_BYTES.checked_mul(1 << 18).is_some());
    assert_eq!(MAX_DISK_BYTES, (1 << 46) - 1);
    assert_eq!((1u64 << 46).checked_mul(1 << 18), None);
}

/// Asserts that a job that needs more disk than the shape has is refused
/// `limits-exceed-shape` (the code that is not retried), that equal disk passes, and that
/// a job with no `disk_bytes` passes a shape of any disk, including none. It catches a
/// disk term missing from `fits_shape`, which would leave an oversize job to wait for
/// room that cannot come.
#[test]
fn fits_shape_disk_refusal_is_limits_exceed_shape() {
    let mut spec = job();
    spec.limits.disk_bytes = Some(1000);
    assert_eq!(spec.fits_shape(&shape_with_disk(1000)), Ok(()));
    assert_eq!(spec.fits_shape(&shape_with_disk(1001)), Ok(()));
    for short in [999, 0] {
        let err = spec.fits_shape(&shape_with_disk(short)).unwrap_err();
        expect(
            err,
            "limits",
            Rule::Inconsistent,
            refusal::LIMITS_EXCEED_SHAPE,
        );
    }
    spec.limits.disk_bytes = None;
    assert_eq!(spec.fits_shape(&shape_with_disk(0)), Ok(()));
    assert_eq!(spec.fits_shape(&shape_with_disk(5)), Ok(()));
}

/// Asserts that a job from a peer that predates the field decodes with no disk limit,
/// that a job without one encodes without the key (so an older agent receives what it
/// always did), and that a limit round-trips. It catches a field that is required on the
/// wire, which would make every older server's job unreadable to this agent.
#[test]
fn a_job_from_an_old_peer_has_no_disk_limit() {
    let spec = job();
    assert_eq!(spec.limits.disk_bytes, None);
    let old = serde_json::to_value(&spec).unwrap();
    assert!(old["limits"].get("disk_bytes").is_none());
    let back: JobSpec = serde_json::from_value(old).unwrap();
    assert_eq!(back, spec);
    let limits: Limits =
        serde_json::from_value(json!({"cpu_millis": 1, "memory_bytes": 2, "wall_ms": 3})).unwrap();
    assert_eq!(limits.disk_bytes, None);
    let mut bounded = job();
    bounded.limits.disk_bytes = Some(4096);
    round_trip(&bounded);
    let text = serde_json::to_value(&bounded).unwrap();
    assert_eq!(text["limits"]["disk_bytes"], json!(4096));
}

/// Asserts that a hello from a host that predates the field decodes with zero disk, that a
/// shape with no disk encodes without the key (so a hello from this version reads the same
/// to an older server), that a shape with disk round-trips, and that the resource vector
/// carries disk both ways. It catches a shape field without `serde(default)`, which would
/// refuse every hello from an older agent, and a conversion that drops the disk.
#[test]
fn a_shape_without_disk_reads_as_zero_and_round_trips() {
    let plain = hello();
    assert_eq!(plain.shape.disk_bytes, 0);
    let mut old = serde_json::to_value(&plain).unwrap();
    assert!(old["shape"].get("disk_bytes").is_none());
    let back: Hello = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(back, plain);
    old["shape"]["disk_bytes"] = Value::from(7u64);
    let sized: Hello = serde_json::from_value(old).unwrap();
    assert_eq!(sized.shape.disk_bytes, 7);
    round_trip(&sized);
    let resources = Resources::new(1, 2, 0, None).unwrap().with_disk_bytes(9);
    let wire = WireShape::from_resources(&resources);
    assert_eq!(wire.disk_bytes, 9);
    assert_eq!(wire.to_resources().unwrap(), resources);
    assert_eq!(sized.to_shape().unwrap().resources().disk_bytes(), 7);
}

/// Asserts that a hello whose shape names more disk than the maximum is refused as out of
/// range under `invalid-hello`, and that the maximum and zero pass. It catches a host
/// shape that makes the sums over hosts overflow.
#[test]
fn a_shape_disk_above_the_maximum_is_refused() {
    let err = check_hello(|h| h.shape.disk_bytes = MAX_DISK_BYTES + 1);
    expect(
        err,
        "shape.disk_bytes",
        Rule::OutOfRange,
        refusal::INVALID_HELLO,
    );
    for ok in [0, MAX_DISK_BYTES] {
        let mut h = hello();
        h.shape.disk_bytes = ok;
        assert_eq!(h.validate(), Ok(()));
    }
}
