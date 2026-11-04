//! The previous release against this one, both ways. Its starts, frozen in
//! `data/previous_release_starts.jsonl`, must decode, validate and re-encode byte for byte
//! here; and a frozen copy of its `JobSpec` envelope, its native body and the checks it
//! made of them decodes a start in the new form, and a downgraded one, and does what that
//! release did with it. The copy is a fixture: it must not follow later changes to
//! `JobSpec`, because it stands for agents already deployed.

use std::collections::BTreeMap;

use js_agent_wire::{
    InputRef, JobSpec, NetworkLevel, OutputKind, OutputSpec, Placement, RunSpec, refusal,
};
use serde::Deserialize;
use serde_json::{Value, json};

const PREVIOUS: &str = include_str!("data/previous_release_starts.jsonl");

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The previous release's `Limits`: three required fields and two optional ones.
#[derive(Debug, Deserialize)]
struct PreviousLimits {
    cpu_millis: u64,
    memory_bytes: u64,
    #[serde(default)]
    disk_bytes: Option<u64>,
    #[serde(default)]
    pids: Option<u64>,
    wall_ms: u64,
}

/// The previous release's `NetworkLevel`: an unknown value fails to decode.
#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreviousNetwork {
    #[default]
    Off,
    Loopback,
    On,
}

/// The previous release's `Placement`.
#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreviousPlacement {
    #[default]
    ReadOnly,
    Writable,
}

/// The previous release's `OutputKind`.
#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PreviousOutputKind {
    #[default]
    File,
    Dir,
}

/// The previous release's `InputRef`, tagged by `type`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousInput {
    Stage {
        stage_id: String,
        dest: String,
        #[serde(default)]
        placement: PreviousPlacement,
    },
    Mount {
        root_name: String,
        rel: String,
        dest: String,
    },
}

/// The previous release's `OutputSpec`.
#[derive(Debug, PartialEq, Deserialize)]
struct PreviousOutput {
    path: String,
    #[serde(default)]
    kind: PreviousOutputKind,
}

/// The previous release's `NativeSpec`: only `program` is required.
#[derive(Debug, Deserialize)]
struct PreviousNative {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    inputs: Vec<PreviousInput>,
    #[serde(default)]
    outputs: Vec<PreviousOutput>,
    #[serde(default)]
    network: PreviousNetwork,
}

/// The previous release's `JobSpec`: the envelope with its required ids and limits, and the
/// fields its body checks read. Unknown fields are ignored, as that release ignored them: it
/// has no `run`, `os` or `arch`.
#[derive(Debug, Deserialize)]
struct PreviousJobSpec {
    run_id: String,
    lease_id: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    native: Option<PreviousNative>,
    #[serde(default)]
    args: Vec<String>,
    limits: PreviousLimits,
    #[serde(default)]
    gpu_indices: Vec<u32>,
    #[serde(default)]
    bundle: Option<String>,
}

/// The previous release's checks of a start, as main had them before the `run` body, in
/// its order: the ids, the limits, `check_bundle` (a bundle needs a native body and an
/// empty image), `check_body` (without a native body the image must be a non-empty
/// reference; a native body beside an image is `invalid-image`), the native body (its
/// program, arguments, environment names, working directory, inputs and outputs) and the
/// fields a native job leaves unused. The reference grammar is a stub that says yes: every
/// image here is a well-formed one that release accepted.
fn previous_check(spec: &PreviousJobSpec) -> Result<(), &'static str> {
    let token = |text: &str| !text.is_empty() && text.len() <= 128;
    if !token(&spec.run_id) || !token(&spec.lease_id) {
        return Err(refusal::INVALID_JOB);
    }
    let limits = &spec.limits;
    let positive = [limits.cpu_millis, limits.memory_bytes, limits.wall_ms]
        .into_iter()
        .chain(limits.disk_bytes)
        .chain(limits.pids)
        .all(|value| value > 0);
    if !positive {
        return Err(refusal::INVALID_JOB);
    }
    if spec.bundle.is_some() && (spec.native.is_none() || !spec.image.is_empty()) {
        return Err(refusal::INVALID_BUNDLE);
    }
    let native = match (&spec.native, spec.image.is_empty()) {
        (None, true) | (Some(_), false) => return Err(refusal::INVALID_IMAGE),
        (None, false) => return Ok(()),
        (Some(native), true) => native,
    };
    previous_native(native)?;
    let wants_bundle = native.program.starts_with("@bundle/");
    if wants_bundle && spec.bundle.is_none() {
        return Err(refusal::INVALID_PROGRAM);
    }
    if !spec.args.is_empty() || !spec.gpu_indices.is_empty() {
        return Err(refusal::INVALID_JOB);
    }
    Ok(())
}

/// The previous release's native body checks, as far as these starts reach them.
fn previous_native(native: &PreviousNative) -> Result<(), &'static str> {
    let program = &native.program;
    let entrypoint = program.strip_prefix("@bundle/");
    let absolute = program.starts_with('/') && !program.split('/').any(|part| part == "..");
    if !entrypoint.map_or(absolute, |name| !name.is_empty()) {
        return Err(refusal::INVALID_PROGRAM);
    }
    if native.args.iter().any(|arg| arg.contains('\0')) {
        return Err(refusal::INVALID_JOB);
    }
    let env_name = |key: &str| {
        let lead = key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
        lead && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    if !native.env.keys().all(|key| env_name(key)) {
        return Err(refusal::INVALID_ENV);
    }
    let relative = |path: &str| !path.starts_with('/') && !path.split('/').any(|p| p == "..");
    let dests_ok = native.inputs.iter().all(|input| match input {
        PreviousInput::Stage { stage_id, dest, .. } => {
            relative(dest)
                && stage_id
                    .strip_prefix("sha256:")
                    .is_some_and(|h| h.len() == 64)
        }
        PreviousInput::Mount {
            root_name,
            rel,
            dest,
        } => !root_name.is_empty() && relative(rel) && relative(dest),
    });
    let outputs_ok = native.outputs.iter().all(|output| relative(&output.path));
    if !(relative(&native.cwd) && dests_ok && outputs_ok) {
        return Err(refusal::INVALID_JOB);
    }
    Ok(())
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
/// byte for byte, and pass the frozen previous checks. The frozen previous `JobSpec`
/// decodes the new form, ignoring `run`, `os` and `arch`, and does what it did before: a
/// run beside an empty image is `invalid-image` there as here, so it runs nothing; a run
/// beside a container image passes there as a plain image start, which is why a server
/// writes the legacy form for such a host (`downgrade_for`, which refuses that start and
/// turns a macOS one into the native start with a bundle that release runs). Catches a
/// legacy field renamed, made required or dropped from the output, a new `JobSpec` that
/// refuses the old shape, and a new start that an older agent would run as something else.
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
                assert_eq!(previous.native.unwrap().program, "@bundle/tool");
                assert_eq!(previous.bundle, Some(format!("toolchain:1@sha256:{HEX}")));
            }
        }
    }
}

/// A macOS start whose run sets every field, as a host without `one-job` is sent it.
fn downgraded() -> Value {
    let mut spec: JobSpec =
        serde_json::from_value(new_start(&format!("toolchain:1@sha256:{HEX}"), "macos")).unwrap();
    spec.run = Some(RunSpec {
        program: "@bundle/cc".into(),
        args: vec!["-c".into(), "a.c".into()],
        env: BTreeMap::from([("LANG".to_owned(), "C".to_owned())]),
        cwd: "src".into(),
        inputs: vec![
            InputRef::Stage {
                stage_id: format!("sha256:{HEX}"),
                dest: "src".into(),
                placement: Placement::Writable,
            },
            InputRef::Mount {
                root_name: "cache".into(),
                rel: "sdk/14".into(),
                dest: "sdk".into(),
            },
        ],
        outputs: vec![OutputSpec {
            path: "out/logs".into(),
            kind: OutputKind::Dir,
        }],
        network: NetworkLevel::Loopback,
    });
    spec.limits.pids = Some(64);
    let legacy = spec.downgrade_for(&Default::default()).unwrap();
    serde_json::to_value(legacy).unwrap()
}

/// Asserts that a downgraded start with every run field set decodes through the frozen
/// previous envelope and native body with every value as the run had it (ids, limits,
/// program, arguments, environment, working directory, both inputs with the placement, the
/// output kind and the network), and passes the previous checks; and that the frozen copy
/// is strict where that release was: a start without `lease_id` or `limits`, a native body
/// without `program` or with a network that release cannot name fails to decode, and a
/// relative program, a bad environment name or a climbing working directory is refused
/// with that release's code. Catches a downgrade that drops a field the previous release
/// requires, writes a native body it cannot read, or one it would refuse.
#[test]
fn a_downgraded_start_reads_as_the_previous_release_read_its_own() {
    let wire = downgraded();
    let previous: PreviousJobSpec = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(previous_check(&previous), Ok(()));
    assert_eq!(
        (previous.run_id.as_str(), previous.lease_id.as_str()),
        ("run-9", "1-1")
    );
    let limits = &previous.limits;
    assert_eq!(
        (limits.cpu_millis, limits.memory_bytes, limits.wall_ms),
        (1000, 1024, 60000)
    );
    assert_eq!((limits.disk_bytes, limits.pids), (None, Some(64)));
    assert_eq!(previous.image, "");
    assert!(previous.args.is_empty() && previous.gpu_indices.is_empty());
    let native = previous.native.as_ref().unwrap();
    assert_eq!(native.program, "@bundle/cc");
    assert_eq!(native.args, ["-c", "a.c"]);
    assert_eq!(
        native.env,
        BTreeMap::from([("LANG".to_owned(), "C".to_owned())])
    );
    assert_eq!(native.cwd, "src");
    assert_eq!(
        native.inputs,
        [
            PreviousInput::Stage {
                stage_id: format!("sha256:{HEX}"),
                dest: "src".into(),
                placement: PreviousPlacement::Writable,
            },
            PreviousInput::Mount {
                root_name: "cache".into(),
                rel: "sdk/14".into(),
                dest: "sdk".into(),
            },
        ]
    );
    assert_eq!(
        native.outputs,
        [PreviousOutput {
            path: "out/logs".into(),
            kind: PreviousOutputKind::Dir,
        }]
    );
    assert_eq!(native.network, PreviousNetwork::Loopback);

    let undecodable: [(&str, fn(&mut Value)); 4] = [
        ("no lease_id", |wire| {
            wire.as_object_mut().unwrap().remove("lease_id");
        }),
        ("no limits", |wire| {
            wire.as_object_mut().unwrap().remove("limits");
        }),
        ("no program", |wire| {
            wire["native"].as_object_mut().unwrap().remove("program");
        }),
        ("unknown network", |wire| {
            wire["native"]["network"] = json!("open")
        }),
    ];
    for (why, edit) in undecodable {
        let mut broken = wire.clone();
        edit(&mut broken);
        assert!(
            serde_json::from_value::<PreviousJobSpec>(broken).is_err(),
            "{why}"
        );
    }
    let refused: [(&str, fn(&mut Value), &str); 3] = [
        (
            "relative program",
            |wire| wire["native"]["program"] = json!("bin/cc"),
            refusal::INVALID_PROGRAM,
        ),
        (
            "bad environment name",
            |wire| wire["native"]["env"] = json!({"1X": "a"}),
            refusal::INVALID_ENV,
        ),
        (
            "climbing working directory",
            |wire| wire["native"]["cwd"] = json!("../up"),
            refusal::INVALID_JOB,
        ),
    ];
    for (why, edit, code) in refused {
        let mut broken = wire.clone();
        edit(&mut broken);
        let previous: PreviousJobSpec = serde_json::from_value(broken).unwrap();
        assert_eq!(previous_check(&previous), Err(code), "{why}");
    }
}
