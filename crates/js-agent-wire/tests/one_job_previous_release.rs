//! The previous release against this one, both ways. Its starts, frozen in
//! `data/previous_release_starts.jsonl`, must decode, validate and re-encode byte for byte
//! here; and a frozen copy of its `JobSpec` and of the checks it made of a start's body
//! decodes a start in the new form and does what that release did with it. The copy is a
//! fixture: it must not follow later changes to `JobSpec`, because it stands for agents
//! already deployed.

use js_agent_wire::{JobSpec, refusal};
use serde::Deserialize;
use serde_json::{Value, json};

const PREVIOUS: &str = include_str!("data/previous_release_starts.jsonl");

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The fields of the previous release's `JobSpec` that its body checks read. Unknown
/// fields are ignored, as that release ignored them: it has no `run`, `os` or `arch`.
#[derive(Debug, Deserialize)]
struct PreviousJobSpec {
    run_id: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    native: Option<Value>,
    #[serde(default)]
    bundle: Option<String>,
}

/// The previous release's `check_bundle` and `check_body`, as main had them before the
/// `run` body: a bundle needs a native body and an empty image; without a native body the
/// image must be a non-empty reference (`invalid-image`); a native body beside an image is
/// `invalid-image`. The reference grammar is a stub that says yes: every image here is a
/// well-formed one that release accepted.
fn previous_check(spec: &PreviousJobSpec) -> Result<(), &'static str> {
    if spec.bundle.is_some() && (spec.native.is_none() || !spec.image.is_empty()) {
        return Err(refusal::INVALID_BUNDLE);
    }
    match (&spec.native, spec.image.is_empty()) {
        (None, true) | (Some(_), false) => Err(refusal::INVALID_IMAGE),
        (None, false) | (Some(_), true) => Ok(()),
    }
}

/// A start in the new form: `run`, `os` and `arch` beside `image`.
fn new_start(image: &str, os: &str) -> Value {
    json!({
        "run_id": "run-9",
        "lease_id": "1-1",
        "image": image,
        "run": {"program": "@bundle/tool", "args": ["--version"]},
        "os": os,
        "arch": "arm64",
        "limits": {"cpu_millis": 1000, "memory_bytes": 1024, "wall_ms": 60000}
    })
}

/// Asserts both directions of B8. The previous release's starts (a container start, a
/// native start and a native start with a bundle) decode, validate, pin and re-encode here
/// byte for byte. The frozen previous `JobSpec` decodes the new form, ignoring `run`, `os`
/// and `arch`, and does what it did before: a run beside an empty image is `invalid-image`
/// there as here, so it runs nothing; a run beside a container image passes there as a
/// plain image start, which is why a server writes the legacy form for such a host
/// (`downgrade_for`, which refuses that start and turns a macOS one into the native start
/// with a bundle that release runs). Catches a legacy field renamed, made required or
/// dropped from the output, a new `JobSpec` that refuses the old shape, and a new start that
/// an older agent would run as something else.
#[test]
fn the_old_native_body_still_parses() {
    let lines: Vec<&str> = PREVIOUS.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        let spec: JobSpec = serde_json::from_str(line).unwrap();
        assert_eq!(spec.validate(), Ok(()), "{line}");
        assert_eq!(spec.require_pinned(), Ok(()), "{line}");
        assert_eq!((spec.run.is_none(), spec.os, spec.arch), (true, None, None));
        assert_eq!(serde_json::to_string(&spec).unwrap(), *line);
        let previous: PreviousJobSpec = serde_json::from_str(line).unwrap();
        assert_eq!(previous_check(&previous), Ok(()), "{line}");
    }
    let native: JobSpec = serde_json::from_str(lines[1]).unwrap();
    assert!(native.native.is_some() && native.bundle.is_none());
    let bundled: JobSpec = serde_json::from_str(lines[2]).unwrap();
    assert!(bundled.native.is_some() && bundled.bundle.is_some());

    let imageless = new_start("", "macos");
    let previous: PreviousJobSpec = serde_json::from_value(imageless.clone()).unwrap();
    assert_eq!(previous.run_id, "run-9");
    assert_eq!(previous_check(&previous), Err(refusal::INVALID_IMAGE));
    let here: JobSpec = serde_json::from_value(imageless).unwrap();
    assert_eq!(here.validate().unwrap_err().code(), refusal::INVALID_IMAGE);

    for os in ["linux", "macos"] {
        let full = new_start(&format!("toolchain:1@sha256:{HEX}"), os);
        let previous: PreviousJobSpec = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(
            previous_check(&previous),
            Ok(()),
            "a plain image start there"
        );
        let here: JobSpec = serde_json::from_value(full).unwrap();
        assert_eq!(here.validate(), Ok(()));
        let legacy = here.downgrade_for(&Default::default());
        match os {
            "linux" => assert_eq!(legacy.unwrap_err().code(), refusal::INVALID_JOB),
            _ => {
                let legacy = serde_json::to_value(legacy.unwrap()).unwrap();
                let previous: PreviousJobSpec = serde_json::from_value(legacy).unwrap();
                assert_eq!(previous_check(&previous), Ok(()), "a native start there");
                assert_eq!(previous.image, "");
                assert_eq!(previous.native.unwrap()["program"], json!("@bundle/tool"));
                assert_eq!(previous.bundle, Some(format!("toolchain:1@sha256:{HEX}")));
            }
        }
    }
}
