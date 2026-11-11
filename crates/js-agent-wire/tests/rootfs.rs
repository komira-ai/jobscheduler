//! A job start's file system beyond its image: the wire form of its `layers`, `mounts` and
//! `host_ports` ([`Layer`], [`JobMount`], [`MountSource`], [`MountMode`], [`Propagation`],
//! [`HostPort`], [`PortProtocol`]), every rule `validate` holds them
//! to at both sides of its bound, and how an unknown layer kind or mode decodes.

mod common;

use common::{broken, expect};
use js_agent_wire::limits::{MAX_LAYERS, MAX_MOUNTS, MAX_PATH_LEN, MAX_PORTS};
use js_agent_wire::{
    BookedResources, Command, HeartbeatReply, HostPort, Isolation, JobMount, JobStartSpec, Layer,
    MountMode, MountSource, PortProtocol, Propagation, Rule, ValidationError, refusal,
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
            JobMount {
                sub_path: Some("14/lib".into()),
                no_exec: true,
                ..mount("/srv/trees/sdk", "/work/sdk", MountMode::ReadOnly)
            },
            mount("/srv/out/job-1", "/out", MountMode::ReadWrite),
        ],
        host_ports: vec![port(9000, None), port(9443, Some(19443))],
    }
}

fn port(host_port: u16, container_port: Option<u16>) -> HostPort {
    HostPort {
        host_port,
        container_port,
        protocol: PortProtocol::Tcp,
    }
}

fn ports(numbers: &[u16]) -> Vec<HostPort> {
    numbers.iter().map(|&number| port(number, None)).collect()
}

fn host(path: &str) -> MountSource {
    MountSource::HostPath { path: path.into() }
}

fn mount(source: &str, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source: host(source),
        target: target.into(),
        mode,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
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
/// `type` tag, each mode in snake case with read-only left out, a `sub_path` and `no_exec`,
/// private propagation and TCP left out, and a host port with and one without its own job
/// port; and that the text decodes back to the start. It catches a renamed tag, field or
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
            {"source": {"type": "host_path", "path": "/srv/trees/src"}, "target": "/work", "mode": "copy_on_write"},
            {
                "source": {"type": "host_path", "path": "/srv/trees/sdk"},
                "target": "/work/sdk",
                "sub_path": "14/lib",
                "no_exec": true
            },
            {"source": {"type": "host_path", "path": "/srv/out/job-1"}, "target": "/out", "mode": "read_write"}
        ],
        "host_ports": [{"host_port": 9000}, {"host_port": 9443, "container_port": 19443}]
    });
    let command = Command::JobStart(Box::new(start()));
    assert_eq!(serde_json::to_value(&command).unwrap(), wire);
    assert_eq!(serde_json::from_value::<Command>(wire).unwrap(), command);
    assert_eq!(command.validate(), Ok(()));
    assert_eq!(MountMode::default(), MountMode::ReadOnly);
    assert_eq!(Propagation::default(), Propagation::Private);
    assert_eq!(PortProtocol::default(), PortProtocol::Tcp);
    assert_eq!(
        (
            port(9000, None).job_port(),
            port(9443, Some(19443)).job_port()
        ),
        (9000, 19443)
    );
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
            "mounts": [{"source": {"type": "host_path", "path": "/srv/sdk"}, "target": "/work/sdk"}]
        })),
        with(json!({"image": image, "host_ports": [{"host_port": 9000}]})),
    ];
    for wire in forms {
        let command: Command = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(command.validate(), Ok(()), "{wire}");
        assert_eq!(serde_json::to_value(&command).unwrap(), wire);
    }
    let read: Command = serde_json::from_value(with(json!({
        "image": image,
        "mounts": [{"source": {"type": "host_path", "path": "/srv/sdk"}, "target": "/work/sdk"}]
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
/// and 65 are refused on `mounts`; a host-path source held to a host path's rules on
/// `mounts.source.path`;
/// a target of `/`, relative, with `..`, `:`, `,`, a non-ASCII letter, at or below each of
/// the runtime's places, or of 4097 bytes refused on `mounts.target` (4096 passes); two
/// targets equal in different case refused as a repeat, while a target below another passes.
/// It catches a mount over `/proc` or the run files, a target that could add a field to a
/// mount option, two mounts at one place on case-insensitive macOS, and a rule that would
/// refuse a read-only mount below a copy-on-write `/work`.
#[test]
fn path_mounts_are_bounded_and_held_to_the_path_rules() {
    let code = refusal::INVALID_MOUNT;
    let many = |n: usize| -> Vec<JobMount> {
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
        refused(
            |s| s.mounts[1].source = host(&source),
            "mounts.source.path",
            rule,
            code,
        );
    }
    accepted(|s| s.mounts[1].source = host(&rooted(MAX_PATH_LEN)));
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
    accepted(|s| s.host_ports = ports(&(1..=max).collect::<Vec<_>>()));
    accepted(|s| s.host_ports = ports(&[1, u16::MAX]));
    accepted(|s| s.host_ports = vec![port(9000, Some(1)), port(9001, Some(u16::MAX))]);
    accepted(|s| s.host_ports = vec![port(9000, None), port(9000, Some(19000))]);
    accepted(|s| s.host_ports = vec![port(9000, Some(9000)), port(9001, None)]);
    refused(
        |s| s.host_ports = ports(&(1..=max + 1).collect::<Vec<_>>()),
        "host_ports",
        Rule::TooMany,
        job,
    );
    let checks: [(Vec<HostPort>, &str, Rule); 5] = [
        (ports(&[9000, 0]), "host_ports.host_port", Rule::OutOfRange),
        (
            vec![port(9000, Some(0))],
            "host_ports.container_port",
            Rule::OutOfRange,
        ),
        (
            ports(&[9000, 9443, 9000]),
            "host_ports.container_port",
            Rule::Duplicate,
        ),
        (
            vec![port(9000, Some(8000)), port(9001, Some(8000))],
            "host_ports.container_port",
            Rule::Duplicate,
        ),
        (
            vec![port(9000, None), port(9001, Some(9000))],
            "host_ports.container_port",
            Rule::Duplicate,
        ),
    ];
    for (host_ports, field, rule) in checks {
        refused(|s| s.host_ports = host_ports, field, rule, job);
    }
    let declared = |s: &mut JobStartSpec| {
        s.network = js_types::JobNetwork::AllowList;
        s.ports = vec![js_types::PortMap {
            host: 8080,
            container: 80,
        }];
    };
    accepted(declared);
    refused(
        |s| {
            declared(s);
            s.host_ports = vec![port(9000, Some(80))];
        },
        "host_ports.container_port",
        Rule::Inconsistent,
        job,
    );
    refused(
        |s| {
            declared(s);
            s.host_ports = ports(&[80]);
        },
        "host_ports.container_port",
        Rule::Inconsistent,
        job,
    );
    let mut wire = serde_json::to_value(start()).unwrap();
    wire["host_ports"][0]["container_port"] = json!(65536);
    assert!(serde_json::from_value::<JobStartSpec>(wire.clone()).is_err());
    wire["host_ports"][0] = json!({"host_port": 65536});
    assert!(serde_json::from_value::<JobStartSpec>(wire).is_err());
}

/// Asserts a mount's `sub_path` rules at their edges, refused `invalid-mount` on
/// `mounts.sub_path`: absolute, with `..`, not in normal form, with a NUL, or of 4097 bytes
/// refused; 4096 bytes, a relative path and the empty path (the source's root) accepted.
/// It catches a `sub_path` that leaves its source by its text, and one the agent's open
/// beneath the source could not take.
#[test]
fn a_sub_path_is_relative_and_in_normal_form() {
    let code = refusal::INVALID_MOUNT;
    let sub = |s: &mut JobStartSpec, path: String| s.mounts[1].sub_path = Some(path);
    for good in [
        "".to_owned(),
        "lib".to_owned(),
        "a/b-c.d".to_owned(),
        "a".repeat(MAX_PATH_LEN),
    ] {
        accepted(|s| sub(s, good));
    }
    for (path, rule) in [
        ("/lib".to_owned(), Rule::Escapes),
        ("../lib".to_owned(), Rule::Escapes),
        ("a/../../b".to_owned(), Rule::Escapes),
        ("a//b".to_owned(), Rule::Empty),
        ("a/./b".to_owned(), Rule::Empty),
        ("lib/".to_owned(), Rule::Empty),
        ("li\0b".to_owned(), Rule::BadChars),
        ("a".repeat(MAX_PATH_LEN + 1), Rule::TooLong),
    ] {
        refused(|s| sub(s, path), "mounts.sub_path", rule, code);
    }
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

/// Asserts that a start whose layer or mount source has a kind this release does not know
/// (a `tmpfs` source, say), or no `type` at all, or whose source is a bare path string, or
/// whose mount has a mode or propagation it does not know, or whose host port has a
/// protocol it does not know, or a `no_exec` that is not a boolean, reads inside a reply as an unknown
/// command, while the job stop and the start beside it arrive whole. It catches a catch-all
/// variant that would let an agent run a job on another root, or with another mount, than
/// the one asked for, and an unknown value that costs the other commands of the reply.
#[test]
fn an_unknown_layer_kind_or_mount_mode_reads_as_an_unknown_command() {
    let start_value = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
    let stop = json!({"type": "job_stop", "job_id": "job-0"});
    let edits: [fn(&mut Value); 10] = [
        |v| v["mounts"][0]["propagation"] = json!("receive"),
        |v| v["host_ports"][0]["protocol"] = json!("udp"),
        |v| v["mounts"][0]["no_exec"] = json!("yes"),
        |v| v["layers"][0] = json!({"type": "overlay", "path": "/srv/root"}),
        |v| v["mounts"][0]["source"] = json!({"type": "tmpfs", "size": 1024}),
        |v| v["mounts"][0]["source"] = json!({"path": "/srv/trees/src"}),
        |v| v["mounts"][0]["source"] = json!("/srv/trees/src"),
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

/// Asserts the two ways the messages grow without a new field name for an old meaning: a
/// mount, its source and a host port with a field this release does not know (mount
/// options, an owner, a protocol, a port in the job) decode with the field ignored and keep
/// their meaning, as a new optional field must; and a source, a layer and a host port are
/// objects, never bare strings or numbers, so a new kind is a new `type` and a new option a
/// new key. It catches a struct that refuses unknown fields, which would make every
/// addition a break, and a scalar where a message belongs.
#[test]
fn the_messages_grow_additively() {
    let mut wire = serde_json::to_value(start()).unwrap();
    wire["mounts"][1]["options"] = json!(["noexec"]);
    wire["mounts"][1]["source"]["uid"] = json!(1000);
    wire["mounts"][1]["read_only_recursive"] = json!(true);
    wire["host_ports"][0]["address"] = json!("127.0.0.1");
    let read: JobStartSpec = serde_json::from_value(wire).unwrap();
    assert_eq!(read, start());
    let text = serde_json::to_value(start()).unwrap();
    assert!(text["mounts"][0]["source"].is_object(), "{text}");
    assert!(text["layers"][0].is_object(), "{text}");
    assert!(text["host_ports"][0].is_object(), "{text}");
}

/// Asserts that a mount and a host port that spell every option at its default (`read_only`,
/// `private`, `no_exec: false`, `tcp`, a `container_port` equal to `host_port`) decode,
/// validate, and are written with the defaults left out, as the text of a start that never
/// named them; except the equal `container_port`, which is kept as given, so a start reads
/// back what it said. It catches a default written out, which would change the text of every
/// start with a mount and make an option added later present in starts that do not use it.
#[test]
fn defaults_spelled_out_are_written_as_left_out() {
    let mut spec = start();
    spec.mounts.truncate(1);
    spec.mounts[0] = mount("/srv/trees/src", "/work", MountMode::ReadOnly);
    spec.host_ports = vec![port(9000, None)];
    let mut spelled = serde_json::to_value(&spec).unwrap();
    spelled["mounts"][0]["mode"] = json!("read_only");
    spelled["mounts"][0]["propagation"] = json!("private");
    spelled["mounts"][0]["no_exec"] = json!(false);
    spelled["host_ports"][0]["protocol"] = json!("tcp");
    let read: JobStartSpec = serde_json::from_value(spelled).unwrap();
    assert_eq!(read, spec);
    assert_eq!(read.validate(), Ok(()));
    let text = serde_json::to_value(&read).unwrap();
    assert_eq!(
        text["mounts"],
        json!([{"source": {"type": "host_path", "path": "/srv/trees/src"}, "target": "/work"}])
    );
    assert_eq!(text["host_ports"], json!([{"host_port": 9000}]));
    spec.host_ports = vec![port(9000, Some(9000))];
    let text = serde_json::to_value(&spec).unwrap();
    assert_eq!(
        text["host_ports"],
        json!([{"host_port": 9000, "container_port": 9000}])
    );
}
