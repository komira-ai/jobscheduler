//! The `native` body of a start: wire forms, defaults, unknown fields, and every rule.

mod common;

use std::collections::BTreeMap;

use common::{broken, expect, job, long};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    Command, HeartbeatReply, InputRef, Isolation, JobSpec, NativeSpec, NetworkLevel, OutputKind,
    OutputSpec, Placement, Rule,
};
use serde_json::{Value, json};

const STAGE: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn spec() -> JobSpec {
    let mut spec = job();
    spec.image = String::new();
    spec.args = Vec::new();
    spec.gpu_indices = Vec::new();
    spec.mounts = Vec::new();
    spec.isolation = Isolation::Contained;
    spec.native = Some(NativeSpec {
        program: "/usr/bin/clang".into(),
        args: vec!["-c".into(), "a.c".into()],
        env: BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]),
        cwd: "src".into(),
        inputs: vec![
            InputRef::Stage {
                stage_id: STAGE.into(),
                dest: "src".into(),
                placement: Placement::Writable,
            },
            InputRef::Mount {
                root_name: "cache".into(),
                rel: "sdk/14".into(),
                dest: "sdk".into(),
            },
        ],
        outputs: vec![
            OutputSpec {
                path: "out/a.o".into(),
                kind: OutputKind::File,
            },
            OutputSpec {
                path: "out/logs".into(),
                kind: OutputKind::Dir,
            },
        ],
        network: NetworkLevel::Loopback,
    });
    spec
}

fn check(edit: impl FnOnce(&mut NativeSpec)) -> js_agent_wire::ValidationError {
    broken(
        &spec(),
        |j| edit(j.native.as_mut().unwrap()),
        JobSpec::validate,
    )
}

fn clash(edit: impl FnOnce(&mut JobSpec)) -> js_agent_wire::ValidationError {
    broken(&spec(), edit, JobSpec::validate)
}

fn path(value: &str) -> NativeSpec {
    let mut native = spec().native.unwrap();
    native.cwd = value.into();
    native
}

/// The exact JSON of a native start with every field set. It catches a renamed field
/// (`args` in particular, which is not `argv`), a lost `snake_case` or a changed input tag,
/// and an `image` that is dropped when empty (an old agent must read it as empty, not as
/// missing).
#[test]
fn native_start_wire_form() {
    let expected = json!({
        "type": "start",
        "run_id": "run-1",
        "lease_id": "1-1",
        "image": "",
        "native": {
            "program": "/usr/bin/clang",
            "args": ["-c", "a.c"],
            "env": {"PATH": "/usr/bin"},
            "cwd": "src",
            "inputs": [
                {"type": "stage", "stage_id": STAGE, "dest": "src", "placement": "writable"},
                {"type": "mount", "root_name": "cache", "rel": "sdk/14", "dest": "sdk"}
            ],
            "outputs": [
                {"path": "out/a.o", "kind": "file"},
                {"path": "out/logs", "kind": "dir"}
            ],
            "network": "loopback"
        },
        "args": [],
        "limits": {"cpu_millis": 2000, "memory_bytes": 1073741824u64, "pids": 512, "wall_ms": 60000},
        "gpu_indices": [],
        "mounts": [],
        "isolation": "contained",
        "labels": {"team": "infra"},
        "heartbeat_seq": 7,
        "valid_for_ms": 10000
    });
    let command = Command::Start(Box::new(spec()));
    assert_eq!(serde_json::to_value(&command).unwrap(), expected);
    let decoded: Command = serde_json::from_value(expected).unwrap();
    assert_eq!(decoded, command);
    assert_eq!(command.validate(), Ok(()));
}

/// A container start encodes without a `native` key, so the existing wire form is
/// unchanged. It catches a `native: null` that a strict older reader would reject.
#[test]
fn container_start_has_no_native_key() {
    let wire = serde_json::to_value(job()).unwrap();
    assert!(wire.get("native").is_none());
}

/// A native body with only `program` decodes with no arguments, no environment, the exec
/// root as working directory, no inputs or outputs and no network; and a start without
/// `image` decodes. It catches a dropped default, which would reject a sender that leaves
/// a field out.
#[test]
fn minimal_native_start_takes_defaults() {
    let body = json!({
        "run_id": "r", "lease_id": "1-1",
        "native": {"program": "/bin/echo"},
        "limits": {"cpu_millis": 1, "memory_bytes": 1, "wall_ms": 1}
    });
    let decoded: JobSpec = serde_json::from_value(body).unwrap();
    let native = decoded.native.as_ref().unwrap();
    assert_eq!(native.program, "/bin/echo");
    assert!(native.args.is_empty() && native.env.is_empty() && native.cwd.is_empty());
    assert!(native.inputs.is_empty() && native.outputs.is_empty());
    assert_eq!(native.network, NetworkLevel::Off);
    assert_eq!(decoded.image, "");
    assert_eq!(decoded.validate(), Ok(()));
    assert_eq!(decoded.require_pinned(), Ok(()));
}

/// An input with no placement is read-only and an output with no kind is a file. It catches
/// a default that grants write access or a directory by omission.
#[test]
fn input_and_output_defaults_are_the_narrow_ones() {
    let input: InputRef =
        serde_json::from_value(json!({"type": "stage", "stage_id": STAGE, "dest": "d"})).unwrap();
    assert!(matches!(
        input,
        InputRef::Stage {
            placement: Placement::ReadOnly,
            ..
        }
    ));
    let output: OutputSpec = serde_json::from_value(json!({"path": "p"})).unwrap();
    assert_eq!(output.kind, OutputKind::File);
}

/// Fields a newer sender adds, at every level of the body, are ignored and a round trip
/// drops them. It catches a `deny_unknown_fields`, which would make a newer server's start
/// unreadable by this agent within major 1.
#[test]
fn unknown_fields_in_a_native_start_are_ignored() {
    let mut wire = serde_json::to_value(Command::Start(Box::new(spec()))).unwrap();
    wire["extra"] = json!(1);
    wire["native"]["extra"] = json!({"a": 1});
    wire["native"]["inputs"][0]["extra"] = json!(true);
    wire["native"]["inputs"][1]["extra"] = json!(true);
    wire["native"]["outputs"][0]["extra"] = json!([]);
    let decoded: Command = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded, Command::Start(Box::new(spec())));
    let again = serde_json::to_value(&decoded).unwrap();
    assert!(again["native"].get("extra").is_none());
    assert!(again.get("extra").is_none());
}

/// A network level, input type, placement or output kind this version does not know makes
/// that one start unreadable, and the commands beside it still arrive. It catches an
/// unknown value being read as a default (which would widen what the job may do).
#[test]
fn an_unknown_confinement_value_reads_as_an_unknown_command() {
    let stop = json!({"type": "stop", "run_id": "r"});
    let start = serde_json::to_value(Command::Start(Box::new(spec()))).unwrap();
    let edits: [fn(&mut Value); 4] = [
        |v| v["native"]["network"] = json!("bridge"),
        |v| v["native"]["inputs"][0]["type"] = json!("volume"),
        |v| v["native"]["inputs"][0]["placement"] = json!("shared"),
        |v| v["native"]["outputs"][0]["kind"] = json!("socket"),
    ];
    for edit in edits {
        let mut one = start.clone();
        edit(&mut one);
        let reply: HeartbeatReply =
            serde_json::from_value(json!({"commands": [one, stop.clone()]})).unwrap();
        assert_eq!(
            reply.commands,
            vec![Command::Unknown, Command::Stop { run_id: "r".into() }]
        );
    }
}

/// Exactly one of the image and the native body: neither is an empty image, both is an
/// inconsistent one, both under `invalid-image`. It catches a start that names nothing to
/// run, and one that an agent from before the field would run as a container.
#[test]
fn exactly_one_of_image_and_native() {
    let neither = clash(|j| j.native = None);
    expect(neither, "image", Rule::Empty, refusal::INVALID_IMAGE);
    let both = clash(|j| j.image = job().image);
    expect(both, "image", Rule::Inconsistent, refusal::INVALID_IMAGE);
    assert_eq!(job().validate(), Ok(()));
    assert_eq!(spec().require_pinned(), Ok(()));
}

/// A native job leaves the container-only fields unused and is contained. It catches a
/// native start asking for a GPU, a container mount, host isolation or container arguments,
/// none of which has a meaning for it.
#[test]
fn a_native_job_leaves_container_fields_unused() {
    let host = clash(|j| j.isolation = Isolation::Host);
    expect(host, "isolation", Rule::Inconsistent, refusal::INVALID_JOB);
    let args = clash(|j| j.args = vec!["x".into()]);
    expect(args, "args", Rule::Inconsistent, refusal::INVALID_JOB);
    let gpu = clash(|j| j.gpu_indices = vec![0]);
    expect(gpu, "gpu_indices", Rule::Inconsistent, refusal::INVALID_JOB);
    let mounts = clash(|j| j.mounts = job().mounts);
    expect(mounts, "mounts", Rule::Inconsistent, refusal::INVALID_JOB);
}

/// Limits and the rest of the job are checked for a native start as for a container one.
/// It catches a native branch that returns before the limit checks.
#[test]
fn a_native_job_keeps_the_job_checks() {
    let wall = clash(|j| j.limits.wall_ms = 0);
    assert_eq!(wall.code(), refusal::WALL_TIME_NOT_POSITIVE);
    let id = clash(|j| j.run_id = "bad id".into());
    expect(id, "run_id", Rule::BadChars, refusal::INVALID_JOB);
}

/// The program must be an absolute path with clean parts and a bounded length, under
/// `invalid-program`. It catches a relative name the agent would resolve through `PATH`,
/// a `..` that leaves a tool root, and control characters in a path.
#[test]
fn program_rules() {
    let bad = |program: String| check(|n| n.program = program);
    let field = "native.program";
    let code = refusal::INVALID_PROGRAM;
    expect(bad(String::new()), field, Rule::Empty, code);
    expect(bad(long(MAX_PROGRAM_LEN + 1)), field, Rule::TooLong, code);
    expect(bad("/usr/bi\0n".into()), field, Rule::BadChars, code);
    expect(bad("/usr/bi\nn".into()), field, Rule::BadChars, code);
    expect(bad("clang".into()), field, Rule::Escapes, code);
    expect(bad("usr/bin/clang".into()), field, Rule::Escapes, code);
    expect(bad("/usr/../bin/clang".into()), field, Rule::Escapes, code);
    expect(bad("/usr//bin".into()), field, Rule::Empty, code);
    expect(bad("/usr/bin/".into()), field, Rule::Empty, code);
    expect(bad("/".into()), field, Rule::Empty, code);
    expect(bad("/usr/./bin".into()), field, Rule::Empty, code);
    let mut at_cap = spec();
    at_cap.native.as_mut().unwrap().program = format!("/{}", "a".repeat(MAX_PROGRAM_LEN - 1));
    assert_eq!(at_cap.validate(), Ok(()));
}

/// Arguments are bounded as a container job's are: count, each length, total, no NUL. The
/// caps pass. It catches an unbounded argument list on the native path.
#[test]
fn native_args_are_bounded() {
    let field = "native.args";
    let code = refusal::INVALID_JOB;
    let many = check(|n| n.args = vec!["a".into(); MAX_ARGS + 1]);
    expect(many, field, Rule::TooMany, code);
    let big = check(|n| n.args = vec![long(MAX_ARG_LEN + 1)]);
    expect(big, field, Rule::TooLong, code);
    let nul = check(|n| n.args = vec!["a\0".into()]);
    expect(nul, field, Rule::BadChars, code);
    let total = check(|n| n.args = vec![long(MAX_ARG_LEN); MAX_ARGS_TOTAL_BYTES / MAX_ARG_LEN + 1]);
    expect(total, field, Rule::TooLong, code);
    let mut ok = spec();
    ok.native.as_mut().unwrap().args = vec![long(MAX_ARG_LEN); MAX_ARGS_TOTAL_BYTES / MAX_ARG_LEN];
    assert_eq!(ok.validate(), Ok(()));
}

/// Environment names and values are bounded and clean, and the names the loader, the
/// sandbox and the agent read are refused, under `invalid-env`. It catches `DYLD_INSERT_LIBRARIES`
/// and its relatives reaching a child, and a name that would add a second variable.
#[test]
fn env_rules() {
    let field = "native.env";
    let code = refusal::INVALID_ENV;
    let one = |key: &str, value: &str| {
        let (key, value) = (key.to_owned(), value.to_owned());
        check(move |n| n.env = BTreeMap::from([(key, value)]))
    };
    let many = check(|n| {
        n.env = (0..=MAX_ENV)
            .map(|i| (format!("K{i}"), String::new()))
            .collect();
    });
    expect(many, field, Rule::TooMany, code);
    expect(one("", "v"), field, Rule::Empty, code);
    expect(
        one(&long(MAX_ENV_KEY_LEN + 1), "v"),
        field,
        Rule::TooLong,
        code,
    );
    for key in ["1A", "A-B", "A=B", "A B", "A\0", "é"] {
        expect(one(key, "v"), field, Rule::BadChars, code);
    }
    for key in [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_",
        "LD_PRELOAD",
        "SANDBOX_X",
        "JS_AGENT_RUN",
    ] {
        expect(one(key, "v"), field, Rule::Reserved, code);
    }
    expect(
        one("A", &long(MAX_ENV_VALUE_LEN + 1)),
        field,
        Rule::TooLong,
        code,
    );
    expect(one("A", "x\0y"), field, Rule::BadChars, code);
    let total = check(|n| {
        let each = MAX_ENV_VALUE_LEN;
        n.env = (0..MAX_ENV_TOTAL_BYTES / each + 1)
            .map(|i| (format!("K{i}"), long(each)))
            .collect();
    });
    expect(total, field, Rule::TooLong, code);
    let mut ok = spec();
    let env = &mut ok.native.as_mut().unwrap().env;
    env.clear();
    env.extend([
        ("_A9".to_owned(), "line one\nline two \u{202e}".to_owned()),
        ("LDX".to_owned(), String::new()),
        ("dyld_lower".to_owned(), long(MAX_ENV_VALUE_LEN)),
    ]);
    assert_eq!(ok.validate(), Ok(()));
}

/// The working directory, an input `dest`, an output path and an input's mount path are
/// relative paths below the exec root as a mount's `rel` is; only the working directory
/// may be empty. It catches `..` and absolute paths on any of them.
#[test]
fn paths_are_relative_and_clean() {
    let code = refusal::INVALID_JOB;
    let cwd = |value: &str| check(|n| n.cwd = value.into());
    expect(
        cwd(&long(MAX_REL_LEN + 1)),
        "native.cwd",
        Rule::TooLong,
        code,
    );
    for bad in ["a:b", "a,b", "a\\b", "a\0b", "a\u{202e}"] {
        expect(cwd(bad), "native.cwd", Rule::BadChars, code);
    }
    for bad in ["/abs", "..", "a/../b", "a/.."] {
        expect(cwd(bad), "native.cwd", Rule::Escapes, code);
    }
    for bad in ["a//b", "a/", ".", "a/./b"] {
        expect(cwd(bad), "native.cwd", Rule::Empty, code);
    }
    for good in ["", "a", "a/b.c/d"] {
        assert_eq!(path(good).validate(), Ok(()), "{good}");
    }
    let dest = |value: &str| {
        let value = value.to_owned();
        check(move |n| {
            n.inputs = vec![InputRef::Stage {
                stage_id: STAGE.into(),
                dest: value,
                placement: Placement::ReadOnly,
            }]
        })
    };
    expect(dest(""), "native.inputs", Rule::Empty, code);
    expect(dest("../x"), "native.inputs", Rule::Escapes, code);
    let output = |value: &str| {
        let value = value.to_owned();
        check(move |n| {
            n.outputs = vec![OutputSpec {
                path: value,
                kind: OutputKind::File,
            }]
        })
    };
    expect(output(""), "native.outputs", Rule::Empty, code);
    expect(output("/etc"), "native.outputs", Rule::Escapes, code);
}

/// A stage id is `sha256:` and 64 lower case hex digits, and a mount input follows the
/// mount rules (`invalid-mount`). It catches a tree named by a short, upper-case or
/// other-algorithm digest, and a mount input that climbs out of its root.
#[test]
fn input_sources_are_checked() {
    let stage = |id: &str| {
        let id = id.to_owned();
        check(move |n| {
            n.inputs = vec![InputRef::Stage {
                stage_id: id,
                dest: "d".into(),
                placement: Placement::ReadOnly,
            }]
        })
    };
    let upper = STAGE.to_uppercase().replace("SHA256", "sha256");
    let short = &STAGE[..STAGE.len() - 1];
    let long_id = format!("{STAGE}0");
    let sha512 = STAGE.replace("sha256", "sha512");
    let bare = &STAGE["sha256:".len()..];
    for bad in [
        upper.as_str(),
        short,
        long_id.as_str(),
        sha512.as_str(),
        bare,
        "",
    ] {
        expect(
            stage(bad),
            "native.inputs",
            Rule::BadChars,
            refusal::INVALID_JOB,
        );
    }
    let mount = |root: &str, rel: &str| {
        let (root, rel) = (root.to_owned(), rel.to_owned());
        check(move |n| {
            n.inputs = vec![InputRef::Mount {
                root_name: root,
                rel,
                dest: "d".into(),
            }]
        })
    };
    expect(
        mount("cache", "../x"),
        "mounts.rel",
        Rule::Escapes,
        refusal::INVALID_MOUNT,
    );
    expect(
        mount("", "x"),
        "mounts.root_name",
        Rule::Empty,
        refusal::INVALID_MOUNT,
    );
}

/// Two inputs may not share a `dest` or nest one inside the other, and two outputs may not
/// name one path. A sibling that only shares a name prefix is fine. It catches a second
/// input cloned over the first.
#[test]
fn inputs_do_not_overlap() {
    let at = |dests: &[&str]| {
        let inputs = dests
            .iter()
            .map(|d| InputRef::Stage {
                stage_id: STAGE.into(),
                dest: (*d).into(),
                placement: Placement::ReadOnly,
            })
            .collect::<Vec<_>>();
        let mut job = spec();
        job.native.as_mut().unwrap().inputs = inputs;
        job.validate()
    };
    let field = "native.inputs";
    let code = refusal::INVALID_JOB;
    expect(at(&["a", "a"]).unwrap_err(), field, Rule::Duplicate, code);
    expect(
        at(&["a", "a/b"]).unwrap_err(),
        field,
        Rule::Inconsistent,
        code,
    );
    expect(
        at(&["a/b", "a"]).unwrap_err(),
        field,
        Rule::Inconsistent,
        code,
    );
    assert_eq!(at(&["a", "ab", "a.b", "b/a"]), Ok(()));
    let out = check(|n| {
        n.outputs.push(OutputSpec {
            path: "out/a.o".into(),
            kind: OutputKind::Dir,
        })
    });
    expect(out, "native.outputs", Rule::Duplicate, code);
}

/// The input and output counts are capped; the caps pass. It catches an unbounded list on
/// either.
#[test]
fn inputs_and_outputs_are_counted() {
    let code = refusal::INVALID_JOB;
    let inputs = |n: usize| -> Vec<InputRef> {
        (0..n)
            .map(|i| InputRef::Stage {
                stage_id: STAGE.into(),
                dest: format!("d{i}"),
                placement: Placement::ReadOnly,
            })
            .collect()
    };
    let outputs = |n: usize| -> Vec<OutputSpec> {
        (0..n)
            .map(|i| OutputSpec {
                path: format!("o{i}"),
                kind: OutputKind::File,
            })
            .collect()
    };
    let over_in = check(|n| n.inputs = inputs(MAX_INPUTS + 1));
    expect(over_in, "native.inputs", Rule::TooMany, code);
    let over_out = check(|n| n.outputs = outputs(MAX_OUTPUTS + 1));
    expect(over_out, "native.outputs", Rule::TooMany, code);
    let mut ok = spec();
    let native = ok.native.as_mut().unwrap();
    native.inputs = inputs(MAX_INPUTS);
    native.outputs = outputs(MAX_OUTPUTS);
    assert_eq!(ok.validate(), Ok(()));
}
