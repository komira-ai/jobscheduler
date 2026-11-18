//! Native bodies and image runs through the lifecycle, over a fake runtime that says it runs
//! them. The agent's runtime-agnostic rules for such a start are the lifecycle's own: which
//! body the runtime is asked to create, the runtime's network check for a native body (the
//! trait's default, here), and the runtime's refusal of a body. They run on every platform;
//! the runtime that gives them meaning on a host is the Mac's.

use std::sync::Arc;

use js_agent::bundle::{Admission, Needs};
use js_agent::lifecycle::{Body, Event};
use js_agent_wire::{InputRef, JobSpec, NativeSpec, NetworkLevel, Placement, refusal};
use js_bundle::DottedVersion;
use js_oci::Allow;
use serde_json::json;

use super::{Harness, Options, job, names};
use crate::bundle::fake::{FakeHost, digest};
use crate::bundle::fixture::settings;
use crate::support::Scratch;

const IMAGE_NAME: &str = "ghcr.io/komira-ai/js/toolchain:1";

/// A native body that runs `program` with `network`.
fn native(network: NetworkLevel) -> NativeSpec {
    serde_json::from_value(json!({
        "program": "/opt/tools/bin/build",
        "args": ["--fast"],
        "network": network,
    }))
    .unwrap()
}

/// A start of run `run_id` with a native body, and no image.
fn native_job(run_id: &str, network: NetworkLevel) -> JobSpec {
    let mut spec = job(run_id);
    spec.image = String::new();
    spec.args = Vec::new();
    spec.native = Some(native(network));
    spec
}

/// A start of run `run_id` named by its image with a `run`.
fn image_run_job(run_id: &str) -> JobSpec {
    let mut spec = job(run_id);
    spec.image = format!("{IMAGE_NAME}@{}", digest('a'));
    spec.args = Vec::new();
    spec.run = Some(
        serde_json::from_value(json!({
            "program": "@bundle/clang",
            "args": ["-c", "main.c"],
            "network": "loopback",
        }))
        .unwrap(),
    );
    spec
}

/// The one event of a start refused with `code` and `why`.
fn refused(run_id: &str, code: &'static str, why: &str) -> Vec<Event> {
    vec![Event::Refused {
        run_id: run_id.to_owned(),
        code,
        why: why.to_owned(),
    }]
}

/// Asserts that a start with a `native` body, on a runtime that runs native bodies, is
/// admitted and the runtime is asked to create exactly that body (`Body::Native`), not the
/// start's container image. It catches a lifecycle that builds a container body for a native
/// start, or drops the body's program and arguments.
#[test]
fn a_native_start_is_created_with_its_native_body() {
    let mut h = Harness::new("nb-native");
    h.fake.run_native();
    let events = h.start(native_job("r1", NetworkLevel::Off));
    assert_eq!(names(&events), ["started r1"]);
    let created = h.fake.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].body, Body::Native(native(NetworkLevel::Off)));
}

/// The Harness of a runtime that runs image runs, with a bundle store whose host holds the
/// bundle `a` (entrypoint `clang`), and image names under `ghcr.io/komira-ai/` allowed.
fn image_run_harness(name: &str) -> Harness {
    let dir = Scratch::new(&format!("nb-store-{name}"));
    let host = FakeHost::new();
    host.set(|state| {
        state.held = Ok(vec![digest('a')]);
        state.needs.insert(
            digest('a'),
            Needs {
                os_version: DottedVersion::parse("14.0").unwrap(),
                xcode: None,
                entrypoints: ["clang".to_owned()].into(),
            },
        );
    });
    let mut rules = settings(&dir);
    rules.image_allow = vec![Allow::parse("ghcr.io/komira-ai/").unwrap()];
    let admission = Admission::new(rules, Arc::clone(&host) as Arc<_>);
    let h = Harness::with(
        name,
        Options {
            bundle: Some(admission),
            ..Options::default()
        },
    );
    h.fake.run_image_runs();
    h
}

/// Asserts that a start named by its image with a `run`, on a runtime that runs image runs
/// and a bundle the store holds, is admitted and created with the `run` as its native body:
/// the run's program, arguments and network, with no ports. It catches a lifecycle that
/// creates a container of the image for such a start, or that drops the run.
#[test]
fn an_image_run_is_created_as_a_native_body_of_its_run() {
    let mut h = image_run_harness("image-run");
    let spec = image_run_job("r1");
    let run = spec.run.clone().unwrap();
    assert_eq!(names(&h.start(spec)), ["started r1"]);
    let created = h.fake.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].body, Body::Native(NativeSpec::from(run)));
}

/// Asserts that a native start with network `on` (the filtered network) or `open`, on a
/// runtime that keeps the trait's default network check for a native body, is refused
/// `egress-unchecked` with the default's text and creates nothing, and that one with `off`
/// or `loopback` is admitted. It catches a default that passes a network the runtime never
/// said it limits, and one that refuses a job that asks for none.
#[test]
fn the_default_check_refuses_a_native_network_the_runtime_does_not_limit() {
    for (n, level) in [NetworkLevel::On, NetworkLevel::Open]
        .into_iter()
        .enumerate()
    {
        let mut h = Harness::new(&format!("nb-egress-{n}"));
        h.fake.run_native();
        let events = h.start(native_job("r1", level));
        let why = "this runtime does not say how it limits the network of a native job";
        assert_eq!(events, refused("r1", "egress-unchecked", why), "{level:?}");
        assert!(h.fake.created().is_empty(), "{level:?} was created");
    }
    for (n, level) in [NetworkLevel::Off, NetworkLevel::Loopback]
        .into_iter()
        .enumerate()
    {
        let mut h = Harness::new(&format!("nb-egress-ok-{n}"));
        h.fake.run_native();
        let events = h.start(native_job("r1", level));
        assert_eq!(names(&events), ["started r1"], "{level:?}");
        assert_eq!(h.fake.created().len(), 1, "{level:?}");
    }
}

/// Asserts that a runtime that refuses a native body at `check_native` makes the start
/// refused with the runtime's own code and text, and creates nothing. It catches a lifecycle
/// that ignores the runtime's answer for a native body, or answers a generic code for it.
#[test]
fn a_native_body_the_runtime_refuses_is_refused_with_its_answer() {
    let mut h = Harness::new("nb-refused");
    h.fake
        .refuse_native(refusal::INVALID_JOB, "the body asks for too much");
    let events = h.start(native_job("r1", NetworkLevel::Off));
    assert_eq!(
        events,
        refused("r1", refusal::INVALID_JOB, "the body asks for too much")
    );
    assert!(h.fake.created().is_empty());
}

/// Asserts that a start named by its image, on a runtime that does not run it (the Mac runtime
/// with no bundle store answers so), is refused with the runtime's own code and text and creates
/// nothing. It catches a lifecycle that creates an image start the runtime said it cannot run,
/// or refuses it with a generic code that hides why.
#[test]
fn an_image_start_the_runtime_does_not_run_is_refused_with_its_answer() {
    let mut h = Harness::new("nb-image-refused");
    let why = "this agent has no bundle store";
    h.fake.refuse_images(refusal::IMAGE_UNSUPPORTED_HERE, why);
    let events = h.start(job("r1"));
    assert_eq!(events, refused("r1", refusal::IMAGE_UNSUPPORTED_HERE, why));
    assert!(h.fake.created().is_empty(), "the image start was created");
}

/// Asserts that a native body's inputs are staged as read-only mounts: an input below a mount
/// root becomes a mount of that root at the input's `dest`, and an input that names no root (a
/// stage, which the native runtime stages itself) becomes none; the run is created with exactly
/// that one mount. It catches a lifecycle that drops a native job's mount inputs, binds one
/// writable or at the root's own path, or turns a stage into a mount of nothing.
#[test]
fn a_native_bodys_mount_input_is_staged_read_only_at_its_dest() {
    let mut h = Harness::new("nb-inputs");
    h.fake.run_native();
    std::fs::create_dir(h.dir.path("data/sub")).unwrap();
    let mut spec = native_job("r1", NetworkLevel::Off);
    let native = spec.native.as_mut().expect("a native body");
    native.inputs = vec![
        InputRef::Mount {
            root_name: "data".to_owned(),
            rel: "sub".to_owned(),
            dest: "inputs/data".to_owned(),
        },
        InputRef::Stage {
            stage_id: format!("sha256:{}", "a".repeat(64)),
            dest: "inputs/stage".to_owned(),
            placement: Placement::default(),
        },
    ];
    assert_eq!(names(&h.start(spec)), ["started r1"]);
    let created = h.fake.created();
    assert_eq!(created.len(), 1);
    let mounts: Vec<(&str, bool)> = created[0]
        .mounts
        .iter()
        .map(|mount| (mount.target.as_str(), mount.ro))
        .collect();
    assert_eq!(mounts, [("inputs/data", true)]);
}
