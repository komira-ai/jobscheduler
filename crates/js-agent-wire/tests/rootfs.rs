//! A job start's file system beyond its image: the wire form of its `layers`, `mounts` and
//! `host_ports` ([`Layer`], [`PathMount`], [`MountMode`]), every rule `validate` holds them
//! to at both sides of its bound, and how an unknown layer kind or mode decodes.

mod common;

use common::{broken, expect};
use js_agent_wire::limits::{MAX_LAYERS, MAX_MOUNTS, MAX_PATH_LEN, MAX_PORTS};
use js_agent_wire::{
    BookedResources, Command, HeartbeatReply, Isolation, JobStartSpec, Layer, MountMode, PathMount,
    Rule, ValidationError, refusal,
};
use serde_json::{Value, json};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn pinned(name: &str) -> String {
    format!("{name}@sha256:{HEX}")
}

/// A start with an image, two layers (a host path and an image), three mounts (one of each
/// mode, the copy-on-write one at `/work` and a read-only one below it) and two host ports.
fn start() -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image: pinned("ubuntu:24.04"),
        command: vec!["make".into(), "all".into()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 2000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        layers: vec![
            Layer::HostPath {
                path: "/srv/trees/toolchain".into(),
            },
            Layer::Image {
                image: pinned("registry.example/sdk:14"),
            },
        ],
        mounts: vec![
            mount("/srv/trees/src", "/work", MountMode::CopyOnWrite),
            mount("/srv/trees/sdk", "/work/sdk", MountMode::ReadOnly),
            mount("/srv/out/job-1", "/out", MountMode::ReadWrite),
        ],
        host_ports: vec![9000, 9443],
    }
}

fn mount(source: &str, target: &str, mode: MountMode) -> PathMount {
    PathMount {
        source: source.into(),
        target: target.into(),
        mode,
    }
}

/// What the agent runs on a start: `validate`, then `require_pinned`, as a command.
fn validate_start(spec: &JobStartSpec) -> Result<(), ValidationError> {
    Command::JobStart(Box::new(spec.clone())).validate()
}

/// `start()` with `edit` applied, which must be refused with `field`, `rule` and `code`.
fn refused(edit: impl FnOnce(&mut JobStartSpec), field: &str, rule: Rule, code: &str) {
    expect(broken(&start(), edit, validate_start), field, rule, code);
}

/// `start()` with `edit` applied, which must pass.
fn accepted(edit: impl FnOnce(&mut JobStartSpec)) -> JobStartSpec {
    let mut spec = start();
    edit(&mut spec);
    assert_eq!(validate_start(&spec), Ok(()), "{spec:?}");
    spec
}

/// `/` and then `len - 1` bytes of the letter `a`.
fn rooted(len: usize) -> String {
    format!("/{}", "a".repeat(len - 1))
}

/// Asserts the exact text of a start with every new field set: each layer kind under its
/// `type` tag, each mode in snake case with read-only left out, and the host ports as plain
/// numbers; and that the text decodes back to the start. It catches a renamed tag, field or
/// mode, a lost `snake_case`, and a read-only mode written out, which would change the text
/// of every read-only mount.
#[test]
fn a_layered_start_has_the_documented_wire_form() {
    let wire = json!({
        "type": "job_start",
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "image": pinned("ubuntu:24.04"),
        "command": ["make", "all"],
        "booked": {"cpu_millis": 2000, "memory_bytes": 1073741824u64},
        "wall_ms": 60000,
        "layers": [
            {"type": "host_path", "path": "/srv/trees/toolchain"},
            {"type": "image", "image": pinned("registry.example/sdk:14")}
        ],
        "mounts": [
            {"source": "/srv/trees/src", "target": "/work", "mode": "copy_on_write"},
            {"source": "/srv/trees/sdk", "target": "/work/sdk"},
            {"source": "/srv/out/job-1", "target": "/out", "mode": "read_write"}
        ],
        "host_ports": [9000, 9443]
    });
    let command = Command::JobStart(Box::new(start()));
    assert_eq!(serde_json::to_value(&command).unwrap(), wire);
    assert_eq!(serde_json::from_value::<Command>(wire).unwrap(), command);
    assert_eq!(command.validate(), Ok(()));
    assert_eq!(MountMode::default(), MountMode::ReadOnly);
}

/// Asserts the minimal form of each field alone: a root of one host-path layer with no image
/// (the `image` key then left out, not written empty), one mount with no `mode` (read as
/// read-only), and one host port; each decodes from that text, validates, and is written
/// back the same. It catches an empty image written as `""`, a mount without a mode that
/// fails to decode, and a field that needs a sibling it does not.
#[test]
fn each_new_field_has_a_minimal_form() {
    let base = json!({
        "type": "job_start",
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "booked": {"cpu_millis": 2000, "memory_bytes": 1073741824u64},
        "wall_ms": 60000
    });
    let with = |pairs: Value| {
        let mut wire = base.clone();
        for (key, value) in pairs.as_object().unwrap() {
            wire[key] = value.clone();
        }
        wire
    };
    let image = json!(pinned("ubuntu:24.04"));
    let forms = [
        with(json!({
            "command": ["/usr/bin/make"],
            "layers": [{"type": "host_path", "path": "/srv/root"}]
        })),
        with(json!({
            "image": image,
            "mounts": [{"source": "/srv/sdk", "target": "/work/sdk"}]
        })),
        with(json!({"image": image, "host_ports": [9000]})),
    ];
    for wire in forms {
        let command: Command = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(command.validate(), Ok(()), "{wire}");
        assert_eq!(serde_json::to_value(&command).unwrap(), wire);
    }
    let read: Command = serde_json::from_value(with(json!({
        "image": image,
        "mounts": [{"source": "/srv/sdk", "target": "/work/sdk"}]
    })))
    .unwrap();
    let Command::JobStart(spec) = read else {
        panic!("a job start");
    };
    assert_eq!(spec.mounts[0].mode, MountMode::ReadOnly);
}

/// Asserts that an empty image passes only beside layers: without them it is refused
/// `invalid-job` on `image`; with them the command must name an absolute program (an empty
/// command, a relative one and one with `..` are refused `invalid-program` on `command`),
/// and a `run`, which names its job by the image, is refused `invalid-image` on `run`. A
/// start with mounts or host ports and no image still needs one. `require_pinned` passes an
/// empty image beside layers. It catches a layers-only root with no program to run, and a
/// start an agent would try to run with neither image nor layers.
#[test]
fn an_image_may_be_empty_only_beside_layers() {
    let program = refusal::INVALID_PROGRAM;
    let bare = |s: &mut JobStartSpec| {
        s.image.clear();
        s.command = vec!["/usr/bin/make".into(), "all".into()];
    };
    let spec = accepted(bare);
    assert_eq!(spec.require_pinned(), Ok(()));
    refused(
        |s| {
            bare(s);
            s.layers.clear();
        },
        "image",
        Rule::Empty,
        refusal::INVALID_JOB,
    );
    refused(
        |s| {
            bare(s);
            s.layers.clear();
            s.mounts.clear();
        },
        "image",
        Rule::Empty,
        refusal::INVALID_JOB,
    );
    for (command, rule) in [
        (Vec::new(), Rule::Empty),
        (vec!["make".to_owned()], Rule::Escapes),
        (vec!["/usr/../bin/make".to_owned()], Rule::Escapes),
        (vec!["/usr/bin/".to_owned()], Rule::Empty),
    ] {
        refused(
            |s| {
                bare(s);
                s.command = command;
            },
            "command",
            rule,
            program,
        );
    }
    refused(
        |s| {
            bare(s);
            s.command.clear();
            s.run = Some(js_agent_wire::RunSpec {
                program: "/usr/bin/make".into(),
                args: Vec::new(),
                env: Default::default(),
                cwd: String::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                network: Default::default(),
            });
        },
        "run",
        Rule::Inconsistent,
        refusal::INVALID_IMAGE,
    );
}

/// Asserts the layers' rules at their edges: 16 layers pass and 17 are refused `invalid-job`
/// on `layers`; a host path that is relative, not in normal form, holds a NUL, or is 4097
/// bytes is refused `invalid-job` on `layers.path`, and one of 4096 bytes passes; an image
/// layer outside the image grammar is refused `invalid-image` on `layers.image`, and a tag
/// alone is refused `image-digest-required` there by `require_pinned`, while `validate`
/// alone passes it, as for the image. It catches an unbounded list of layers, a host path
/// that names one directory in two ways, and an image layer whose content can move.
#[test]
fn layers_are_bounded_and_held_to_the_path_rules() {
    let job = refusal::INVALID_JOB;
    let host = |path: String| Layer::HostPath { path };
    accepted(|s| s.layers = (0..MAX_LAYERS).map(|n| host(format!("/l/{n}"))).collect());
    refused(
        |s| s.layers = (0..=MAX_LAYERS).map(|n| host(format!("/l/{n}"))).collect(),
        "layers",
        Rule::TooMany,
        job,
    );
    accepted(|s| s.layers = vec![host(rooted(MAX_PATH_LEN))]);
    for (path, rule) in [
        (rooted(MAX_PATH_LEN + 1), Rule::TooLong),
        ("srv/root".to_owned(), Rule::Escapes),
        ("/srv/../root".to_owned(), Rule::Escapes),
        ("/srv/root/".to_owned(), Rule::Empty),
        ("/srv//root".to_owned(), Rule::Empty),
        ("/srv/./root".to_owned(), Rule::Empty),
        (String::new(), Rule::Empty),
        ("/srv/ro\0ot".to_owned(), Rule::BadChars),
        ("/srv/ro\u{202e}ot".to_owned(), Rule::BadChars),
    ] {
        refused(|s| s.layers[0] = host(path), "layers.path", rule, job);
    }
    let image = |image: &str| Layer::Image {
        image: image.into(),
    };
    refused(
        |s| s.layers[1] = image("oci:ubuntu"),
        "layers.image",
        Rule::BadChars,
        refusal::INVALID_IMAGE,
    );
    refused(
        |s| s.layers[1] = image(""),
        "layers.image",
        Rule::Empty,
        refusal::INVALID_IMAGE,
    );
    let mut tagged = start();
    tagged.layers[1] = image("registry.example/sdk:14");
    assert_eq!(tagged.validate(), Ok(()));
    expect(
        tagged.require_pinned().unwrap_err(),
        "layers.image",
        Rule::NotPinned,
        refusal::IMAGE_DIGEST_REQUIRED,
    );
    expect(
        validate_start(&tagged).unwrap_err(),
        "layers.image",
        Rule::NotPinned,
        refusal::IMAGE_DIGEST_REQUIRED,
    );
}

/// Asserts the mounts' rules at their edges, each refused `invalid-mount`: 64 mounts pass
/// and 65 are refused on `mounts`; a source held to a host path's rules on `mounts.source`;
/// a target of `/`, relative, with `..`, `:`, `,`, a non-ASCII letter, at or below each of
/// the runtime's places, or of 4097 bytes refused on `mounts.target` (4096 passes); two
/// targets equal in different case refused as a repeat, while a target below another passes.
/// It catches a mount over `/proc` or the run files, a target that could add a field to a
/// mount option, two mounts at one place on case-insensitive macOS, and a rule that would
/// refuse a read-only mount below a copy-on-write `/work`.
#[test]
fn path_mounts_are_bounded_and_held_to_the_path_rules() {
    let code = refusal::INVALID_MOUNT;
    let many = |n: usize| -> Vec<PathMount> {
        (0..n)
            .map(|i| mount("/srv/m", &format!("/m/{i}"), MountMode::ReadOnly))
            .collect()
    };
    accepted(|s| s.mounts = many(MAX_MOUNTS));
    refused(
        |s| s.mounts = many(MAX_MOUNTS + 1),
        "mounts",
        Rule::TooMany,
        code,
    );
    for (source, rule) in [
        ("srv/sdk".to_owned(), Rule::Escapes),
        ("/srv/sdk/".to_owned(), Rule::Empty),
        ("/srv/\0".to_owned(), Rule::BadChars),
        (rooted(MAX_PATH_LEN + 1), Rule::TooLong),
    ] {
        refused(|s| s.mounts[1].source = source, "mounts.source", rule, code);
    }
    accepted(|s| s.mounts[1].source = rooted(MAX_PATH_LEN));
    accepted(|s| s.mounts[1].target = rooted(MAX_PATH_LEN));
    let mut targets = vec![
        ("/".to_owned(), Rule::Empty),
        ("work/sdk".to_owned(), Rule::Escapes),
        ("/work/../proc".to_owned(), Rule::Escapes),
        ("/work/a:b".to_owned(), Rule::BadChars),
        ("/work/a,b".to_owned(), Rule::BadChars),
        ("/work/a\\b".to_owned(), Rule::BadChars),
        ("/work/caf\u{e9}".to_owned(), Rule::BadChars),
        (rooted(MAX_PATH_LEN + 1), Rule::TooLong),
        ("/WORK".to_owned(), Rule::Duplicate),
    ];
    for place in js_types::path::RUNTIME_PLACES {
        targets.push((place.to_owned(), Rule::RuntimePlace));
        targets.push((format!("{place}/x"), Rule::RuntimePlace));
    }
    for (target, rule) in targets {
        refused(|s| s.mounts[1].target = target, "mounts.target", rule, code);
    }
}

/// Asserts the host ports' rules at their edges, each refused `invalid-job` on `host_ports`:
/// 16 ports pass and 17 are refused; port 0 is refused and 1 and 65535 pass; a port named
/// twice is refused. It catches an unbounded list and a port the agent could not bind.
#[test]
fn host_ports_are_bounded_distinct_and_above_zero() {
    let job = refusal::INVALID_JOB;
    let max = u16::try_from(MAX_PORTS).unwrap();
    accepted(|s| s.host_ports = (1..=max).collect());
    accepted(|s| s.host_ports = vec![1, u16::MAX]);
    refused(
        |s| s.host_ports = (1..=max + 1).collect(),
        "host_ports",
        Rule::TooMany,
        job,
    );
    refused(
        |s| s.host_ports = vec![9000, 0],
        "host_ports",
        Rule::OutOfRange,
        job,
    );
    refused(
        |s| s.host_ports = vec![9000, 9443, 9000],
        "host_ports",
        Rule::Duplicate,
        job,
    );
}

/// Asserts that a host start (`isolation: host`, which runs on the machine itself) is
/// refused `invalid-job` on `isolation` when it has any one of layers, mounts or host ports,
/// and passes with none of them. It catches a host job given a root or mounts it would
/// never be confined to.
#[test]
fn a_host_start_names_none_of_the_three() {
    let host = |s: &mut JobStartSpec| {
        s.isolation = Isolation::Host;
        s.layers.clear();
        s.mounts.clear();
        s.host_ports.clear();
    };
    accepted(host);
    let edits: [fn(&mut JobStartSpec); 3] = [
        |s| s.layers = start().layers,
        |s| s.mounts = start().mounts,
        |s| s.host_ports = start().host_ports,
    ];
    for edit in edits {
        refused(
            |s| {
                host(s);
                edit(s);
            },
            "isolation",
            Rule::Inconsistent,
            refusal::INVALID_JOB,
        );
    }
}

/// Asserts that a start whose layer has a kind this release does not know, or no `type` at
/// all, or whose mount has a mode it does not know, reads inside a reply as an unknown
/// command, while the job stop and the start beside it arrive whole. It catches a catch-all
/// variant that would let an agent run a job on another root, or with another mount, than
/// the one asked for, and an unknown value that costs the other commands of the reply.
#[test]
fn an_unknown_layer_kind_or_mount_mode_reads_as_an_unknown_command() {
    let start_value = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
    let stop = json!({"type": "job_stop", "job_id": "job-0"});
    let edits: [fn(&mut Value); 4] = [
        |v| v["layers"][0] = json!({"type": "overlay", "path": "/srv/root"}),
        |v| v["layers"][0] = json!({"path": "/srv/root"}),
        |v| v["mounts"][0]["mode"] = json!("shared"),
        |v| v["mounts"][0]["mode"] = json!("read-only"),
    ];
    for edit in edits {
        let mut one = start_value.clone();
        edit(&mut one);
        let reply: HeartbeatReply =
            serde_json::from_value(json!({"commands": [one, stop, start_value]})).unwrap();
        assert_eq!(
            reply.commands,
            vec![
                Command::Unknown,
                Command::JobStop {
                    job_id: "job-0".into()
                },
                Command::JobStart(Box::new(start())),
            ]
        );
    }
}
