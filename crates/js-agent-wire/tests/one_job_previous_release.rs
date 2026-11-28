//! The previous release against this one, both ways. Its run starts, frozen in
//! `data/previous_release_starts.jsonl`, must decode, validate and re-encode here as the
//! `JobSpec` of this release, which no longer carries `valid_for_ms`, byte for byte; sent as
//! a `start` command they read here as an unknown command; and a frozen copy of its
//! `JobSpec` envelope, its native body and the checks it made of them reads them as it did.
//! The copy is a fixture: it must not follow later changes to `JobSpec`, because it stands
//! for agents already deployed.
//!
//! The release before tree inputs, the same way: its starts with stage and mount inputs,
//! frozen in `data/previous_release_input_starts.jsonl`, read the same here; and its
//! `InputRef` (the frozen `PreviousInput` below, which that release still had) cannot read
//! a tree input, so a start with one reads there as an unknown command beside the commands
//! it can read.

mod common;

use std::collections::BTreeMap;

use js_agent_wire::{
    Command, HeartbeatReply, InputRef, JobSpec, NetworkLevel, Placement, RunSpec, TreeUnavailable,
    refusal,
};
use serde::Deserialize;
use serde_json::{Value, json};

const PREVIOUS: &str = include_str!("data/previous_release_starts.jsonl");

/// Starts written by the release before tree inputs: a job start and a start in the one job
/// definition, each with a `run` holding a stage and a mount input.
const PREVIOUS_INPUTS: &str = include_str!("data/previous_release_input_starts.jsonl");

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

/// `line` without its `valid_for_ms` member, the one change this release makes to the text
/// of a previous release start.
fn without_valid_for_ms(line: &str) -> String {
    let key = ",\"valid_for_ms\":";
    let at = line
        .find(key)
        .expect("the frozen start carries valid_for_ms");
    let digits = line[at + key.len()..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    format!("{}{}", &line[..at], &line[at + key.len() + digits..])
}

/// Asserts both directions of B8 for the previous release's run starts (a container start, a
/// native start and a native start with a bundle). Here each decodes as a `JobSpec`,
/// validates, pins and re-encodes byte for byte as the same text without its `valid_for_ms`
/// member; the same text under a `start` tag reads as an unknown command; and the frozen
/// previous `JobSpec` still reads each line and passes the frozen previous checks, the native
/// bodies with the network they name. A run beside an empty image is `invalid-image` in the
/// frozen reader as here. Catches a legacy field renamed, made required or dropped from the
/// output, a `JobSpec` that refuses the old shape or keeps the dropped member, and a
/// `start` command that is still read as a start.
#[test]
fn the_old_native_body_still_parses() {
    let lines: Vec<&str> = PREVIOUS.lines().collect();
    assert_eq!(lines.len(), 3);
    let mut networks = Vec::new();
    for line in &lines {
        let spec: JobSpec = serde_json::from_str(line).unwrap();
        assert_eq!(spec.validate(), Ok(()), "{line}");
        assert_eq!(spec.require_pinned(), Ok(()), "{line}");
        assert_eq!((spec.run.is_none(), spec.os, spec.arch), (true, None, None));
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            without_valid_for_ms(line)
        );
        let mut tagged: Value = serde_json::from_str(line).unwrap();
        tagged["type"] = json!("start");
        assert_eq!(
            serde_json::from_value::<Command>(tagged).unwrap(),
            Command::Unknown,
            "{line}"
        );
        let previous: PreviousJobSpec = serde_json::from_str(line).unwrap();
        assert_eq!(previous_check(&previous), Ok(()), "{line}");
        networks.push(previous.native.map(|native| native.network));
    }
    assert_eq!(
        networks,
        [
            None,
            Some(PreviousNetwork::Loopback),
            Some(PreviousNetwork::Off)
        ]
    );
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
}

/// The job start of the release before tree inputs, the fields a test here reads. Its `run`
/// has the fields of the native body above, and its inputs are `PreviousInput`.
#[derive(Debug, Deserialize)]
struct PreviousJobStart {
    job_id: String,
    #[serde(default)]
    run: Option<PreviousNative>,
}

/// The commands of that release a test here sends; any other tag reads as `Unknown`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousCommand {
    Start(Box<PreviousJobSpec>),
    JobStart(Box<PreviousJobStart>),
    Stop {
        run_id: String,
    },
    JobStop {
        job_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// That release's `commands_or_unknown`: each command decoded on its own, and one that does
/// not decode read as `Unknown`.
fn previous_commands(reply: &Value) -> Vec<PreviousCommand> {
    reply["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| PreviousCommand::deserialize(item).unwrap_or(PreviousCommand::Unknown))
        .collect()
}

/// A run body with `input` beside a mount input.
fn run_with(program: &str, input: InputRef) -> RunSpec {
    RunSpec {
        program: program.into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: vec![
            input,
            InputRef::Mount {
                root_name: "cache".into(),
                rel: "sdk/14".into(),
                dest: "sdk".into(),
            },
        ],
        outputs: Vec::new(),
        network: NetworkLevel::Off,
    }
}

/// Asserts the B8 direction an agent not yet upgraded meets: the frozen `InputRef` of the
/// release before tree inputs does not decode a tree input, so a job start whose `run` has
/// one reads there as an unknown command, while the job stop and the job start with a stage
/// input beside it arrive whole, and so do the run `start` and `stop` that release also read;
/// this release reads the three job commands as themselves (the tree start valid) and the
/// `start` and `stop` as unknown commands, none refused. Catches a tree input that an older
/// agent could read as something else (a stage or a mount, or a start with the input dropped)
/// and run without its tree, a tree start that costs an older agent the other commands of its
/// reply, and an older server's run commands that this release still reads.
#[test]
fn a_tree_start_reads_as_unknown_to_the_previous_release() {
    let tree = InputRef::Tree {
        digest: format!("sha256:{HEX}"),
        dest: "src".into(),
        placement: Placement::ReadOnly,
        unavailable: TreeUnavailable::JobStages,
    };
    let as_input = serde_json::to_value(&tree).unwrap();
    assert!(serde_json::from_value::<PreviousInput>(as_input).is_err());

    let image = format!("ubuntu:24.04@sha256:{HEX}");
    let lines: Vec<&str> = PREVIOUS_INPUTS.lines().collect();
    let Command::JobStart(staged) = serde_json::from_str(lines[0]).unwrap() else {
        panic!("the first frozen start is a job start");
    };
    let mut tree_start = staged.clone();
    tree_start.job_id = "job-2".into();
    tree_start.image = image;
    tree_start.run = Some(run_with("/usr/bin/make", tree));
    let commands = vec![
        Command::JobStart(tree_start),
        Command::JobStop {
            job_id: "job-0".into(),
            reason: None,
        },
        Command::JobStart(staged),
    ];
    for command in &commands {
        assert_eq!(command.validate(), Ok(()), "{command:?}");
    }
    let mut reply = serde_json::to_value(HeartbeatReply {
        commands: commands.clone(),
        ..HeartbeatReply::default()
    })
    .unwrap();
    let run_start: Value = serde_json::from_str(lines[1]).unwrap();
    let run_commands = reply["commands"].as_array_mut().unwrap();
    run_commands.push(run_start);
    run_commands.push(json!({"type": "stop", "run_id": "run-0"}));
    let here: HeartbeatReply = serde_json::from_value(reply.clone()).unwrap();
    let mut expected = commands;
    expected.extend([Command::Unknown, Command::Unknown]);
    assert_eq!(here.commands, expected);
    let checked = here.validate();
    assert_eq!(checked.refused, []);
    assert_eq!(checked.commands.len(), 5);

    let there = previous_commands(&reply);
    assert_eq!(there.len(), 5);
    assert!(matches!(there[0], PreviousCommand::Unknown));
    assert!(matches!(&there[1], PreviousCommand::JobStop { job_id } if job_id == "job-0"));
    let PreviousCommand::JobStart(start) = &there[2] else {
        panic!("the job start beside the tree start is lost: {there:?}");
    };
    assert_eq!(start.job_id, "job-1");
    let inputs = &start.run.as_ref().unwrap().inputs;
    assert!(matches!(&inputs[0], PreviousInput::Stage { dest, .. } if dest == "src"));
    assert!(matches!(&there[3], PreviousCommand::Start(spec) if spec.run_id == "run-1"));
    assert!(matches!(&there[4], PreviousCommand::Stop { run_id } if run_id == "run-0"));
}

/// Asserts the other direction: the job start the release before tree inputs wrote, with a
/// stage and a mount input, decodes here as the same stage and mount, validates, and is
/// written back byte for byte; its start in the one job definition (a run start) reads as an
/// unknown command, and as a `JobSpec` it keeps the same inputs and encodes as the same
/// object without its tag and its `valid_for_ms`; and the frozen `InputRef` of that release
/// reads the same inputs from what this release writes. Catches a stage or mount input
/// re-meant or re-spelled by the change that added trees, a tree field (`unavailable`)
/// leaking into the other inputs' text, and a run start that is still read as a start.
#[test]
fn a_previous_release_start_reads_the_same_on_this_release() {
    let lines: Vec<&str> = PREVIOUS_INPUTS.lines().collect();
    assert_eq!(lines.len(), 2);
    let stage = InputRef::Stage {
        stage_id: format!("sha256:{HEX}"),
        dest: "src".into(),
        placement: Placement::Writable,
    };
    let mount = InputRef::Mount {
        root_name: "cache".into(),
        rel: "sdk/14".into(),
        dest: "sdk".into(),
    };
    let previous = || {
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
    };
    let command: Command = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(command.validate(), Ok(()));
    let Command::JobStart(start) = &command else {
        panic!("the first frozen start is a job start");
    };
    assert_eq!(
        start.run.as_ref().unwrap().inputs,
        [stage.clone(), mount.clone()]
    );
    let written = serde_json::to_string(&command).unwrap();
    assert_eq!(written, lines[0]);
    let wire: Value = serde_json::from_str(&written).unwrap();
    let inputs: Vec<PreviousInput> = serde_json::from_value(wire["run"]["inputs"].clone()).unwrap();
    assert_eq!(inputs, previous());

    let old: Command = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(old, Command::Unknown);
    let spec: JobSpec = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(spec.validate(), Ok(()));
    assert_eq!(spec.run.as_ref().unwrap().inputs, [stage, mount]);
    let mut object: Value = serde_json::from_str(lines[1]).unwrap();
    let members = object.as_object_mut().unwrap();
    assert_eq!(members.remove("type"), Some(json!("start")));
    assert_eq!(members.remove("valid_for_ms"), Some(json!(0)));
    let written = serde_json::to_value(&spec).unwrap();
    assert_eq!(written, object);
    let inputs: Vec<PreviousInput> =
        serde_json::from_value(written["run"]["inputs"].clone()).unwrap();
    assert_eq!(inputs, previous());
}
