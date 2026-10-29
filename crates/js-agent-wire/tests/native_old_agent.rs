//! A native start against a frozen copy of the struct and the checks an agent had before
//! the `native` body existed. The copy is a fixture: it must not follow later changes to
//! `JobSpec`, because it stands for agents already deployed.

mod common;

use std::collections::BTreeMap;

use common::job;
use js_agent_wire::{NativeSpec, NetworkLevel, refusal};
use serde::Deserialize;
use serde_json::{Value, json};

/// The `JobSpec` of the agent before `native`, with the fields the checks before the image
/// read. Unknown fields are ignored, as that agent ignored them.
#[derive(Debug, Deserialize)]
struct OldJobSpec {
    run_id: String,
    lease_id: String,
    image: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

/// The old `validate` as far as the image: both ids are tokens, then the image is checked,
/// and a start with an empty image is refused with `invalid-image` before anything runs.
/// The old grammar for a non-empty image is not copied (every image in these tests is a
/// well-formed one it accepted).
fn old_validate(spec: &OldJobSpec) -> Result<(), &'static str> {
    let token = |text: &str| !text.is_empty() && text.len() <= 128;
    if !token(&spec.run_id) || !token(&spec.lease_id) {
        return Err(refusal::INVALID_JOB);
    }
    if spec.image.is_empty() {
        return Err(refusal::INVALID_IMAGE);
    }
    Ok(())
}

fn native() -> NativeSpec {
    NativeSpec {
        program: "/usr/bin/clang".into(),
        args: vec!["-c".into(), "a.c".into()],
        env: BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]),
        cwd: "src".into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
    }
}

fn native_job() -> Value {
    let mut spec = job();
    spec.image = String::new();
    spec.args = Vec::new();
    spec.gpu_indices = Vec::new();
    spec.mounts = Vec::new();
    spec.isolation = js_agent_wire::Isolation::Contained;
    spec.native = Some(native());
    assert_eq!(spec.validate(), Ok(()));
    serde_json::to_value(&spec).unwrap()
}

/// An agent from before the field decodes the new native start and refuses it as
/// `invalid-image`, so it runs nothing. It catches a native start that reaches an old agent
/// with an image it can run, or with no `image` key (which that agent cannot decode at
/// all, and so would not report).
#[test]
fn an_old_agent_refuses_a_native_start_as_an_invalid_image() {
    let wire = native_job();
    assert_eq!(wire["image"], json!(""));
    assert_eq!(wire["native"]["program"], json!("/usr/bin/clang"));
    let old: OldJobSpec = serde_json::from_value(wire).unwrap();
    assert_eq!(old_validate(&old), Err(refusal::INVALID_IMAGE));
    assert!(old.args.is_empty());
    assert_eq!(old.labels.len(), 1);
}

/// The same start with an image beside the native body passes the old agent's checks, which
/// is the reason the new side refuses it: it would run as a container there.
#[test]
fn a_start_with_both_would_run_as_a_container_on_an_old_agent() {
    let mut wire = native_job();
    wire["image"] =
        json!("alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let old: OldJobSpec = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(old_validate(&old), Ok(()));
    let new: js_agent_wire::JobSpec = serde_json::from_value(wire).unwrap();
    assert_eq!(new.validate().unwrap_err().code(), refusal::INVALID_IMAGE);
}
