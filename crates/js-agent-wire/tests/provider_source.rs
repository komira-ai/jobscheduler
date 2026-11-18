//! A layer or a mount from a node-local provider (`docs/design/mount-providers.md`): the
//! wire form of `Layer::Provider` and `MountSource::Provider`, the rules of a provider's
//! name and ref at both sides of their bounds, and how an agent of the release before the
//! `provider` kind reads a start that has one (B8).

mod common;

use common::{broken, expect};
use js_agent_wire::limits::{
    MAX_LAYERS, MAX_MOUNTS, MAX_PROVIDER_NAME_LEN, MAX_PROVIDER_REF_LEN, REPLY_BODY_BUDGET,
};
use js_agent_wire::{
    BookedResources, Command, HeartbeatReply, JobMount, JobStartSpec, Layer, MountMode,
    MountSource, Propagation, Rule, ValidationError, is_provider_name, refusal,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A start with one layer of each kind (a host path, an image, a provider's) and three
/// mounts (a host path, a provider's, and a read-write provider's below it).
fn start() -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image: format!("ubuntu:24.04@sha256:{HEX}"),
        command: vec!["make".into()],
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
                image: format!("registry.example/sdk:14@sha256:{HEX}"),
            },
            provider_layer("tierstore", "toolchains/clang-18"),
        ],
        mounts: vec![
            host_mount("/srv/trees/src", "/work"),
            provider_mount("tierstore", "datasets/v3", "/data", MountMode::ReadOnly),
            provider_mount("scratch-2", "cache", "/data/cache", MountMode::ReadWrite),
        ],
        host_ports: Vec::new(),
        placed_at_ms: None,
    }
}

fn provider_layer(provider: &str, reference: &str) -> Layer {
    Layer::Provider {
        provider: provider.into(),
        reference: reference.into(),
    }
}

fn mount_of(source: MountSource, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source,
        target: target.into(),
        mode,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

fn host_mount(path: &str, target: &str) -> JobMount {
    let source = MountSource::HostPath { path: path.into() };
    mount_of(source, target, MountMode::ReadOnly)
}

fn provider_mount(provider: &str, reference: &str, target: &str, mode: MountMode) -> JobMount {
    let source = MountSource::Provider {
        provider: provider.into(),
        reference: reference.into(),
    };
    mount_of(source, target, mode)
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
fn accepted(edit: impl FnOnce(&mut JobStartSpec)) {
    let mut spec = start();
    edit(&mut spec);
    assert_eq!(validate_start(&spec), Ok(()), "{spec:?}");
}

/// `len` bytes of the letter `a`.
fn long(len: usize) -> String {
    "a".repeat(len)
}

/// Asserts which texts are a provider's name: 1 to 32 bytes of lower-case letters, digits
/// and `-`, the first a letter. It catches a name that starts with a digit or a dash, a name
/// with an upper-case letter, a `.`, a `_`, a space or a non-ASCII letter (a name becomes
/// part of a capability key and of a flag, which accept none of them), the empty name, and
/// a bound that is off by one on either side; and the two bounds and the spelling of the
/// refusal code a provider's own refusal is reported under.
#[test]
fn a_provider_name_is_a_lower_case_letter_then_letters_digits_and_dashes() {
    let longest = long(MAX_PROVIDER_NAME_LEN);
    let too_long = long(MAX_PROVIDER_NAME_LEN + 1);
    let table: [(&str, bool); 15] = [
        ("tierstore", true),
        ("a", true),
        ("z9-9", true),
        ("a-", true),
        (longest.as_str(), true),
        (too_long.as_str(), false),
        ("", false),
        ("9tier", false),
        ("-tier", false),
        ("Tier", false),
        ("tier.store", false),
        ("tier_store", false),
        ("tier store", false),
        ("tier\u{e9}", false),
        ("\u{e9}tier", false),
    ];
    for (name, expected) in table {
        assert_eq!(is_provider_name(name), expected, "{name:?}");
    }
    assert_eq!(MAX_PROVIDER_NAME_LEN, 32);
    assert_eq!(MAX_PROVIDER_REF_LEN, 1024);
    assert_eq!(refusal::PROVIDER_REFUSED, "provider-refused");
}

/// Asserts that a start with provider layers and mounts beside host-path and image ones
/// validates, and that a provider's name of 32 bytes and a ref of 1024 bytes pass; a ref may
/// hold spaces, slashes and non-ASCII letters, because only the provider reads it. It
/// catches a rule that refuses a provider source next to the other kinds, one that cuts a
/// name or a ref a byte short, and one that holds a ref to the path grammar.
#[test]
fn provider_layers_and_mounts_are_accepted() {
    assert_eq!(validate_start(&start()), Ok(()));
    let name = long(MAX_PROVIDER_NAME_LEN);
    let reference = long(MAX_PROVIDER_REF_LEN);
    accepted(|s| s.layers[2] = provider_layer(&name, &reference));
    accepted(|s| s.mounts[1] = provider_mount(&name, &reference, "/data", MountMode::ReadOnly));
    accepted(|s| s.layers[2] = provider_layer("tierstore", "a b/../c\u{e9}:;,\\"));
    accepted(|s| {
        s.mounts[1] = provider_mount(
            "tierstore",
            "a b/../c\u{e9}",
            "/data",
            MountMode::CopyOnWrite,
        )
    });
    accepted(|s| {
        s.image.clear();
        s.command = vec!["/usr/bin/make".into()];
        s.layers = vec![provider_layer("tierstore", "toolchains/clang-18")];
    });
}

/// Asserts that a start with every layer and every mount a provider's at the longest name
/// and ref validates, and that eight such starts in one reply stay within the reply budget
/// the agent reads. It catches a bound on a ref raised past what a reply can carry.
#[test]
fn starts_at_every_provider_cap_fit_a_reply() {
    let name = long(MAX_PROVIDER_NAME_LEN);
    let reference = long(MAX_PROVIDER_REF_LEN);
    let mut spec = start();
    spec.layers = (0..MAX_LAYERS)
        .map(|_| provider_layer(&name, &reference))
        .collect();
    spec.mounts = (0..MAX_MOUNTS)
        .map(|n| provider_mount(&name, &reference, &format!("/m/{n}"), MountMode::ReadOnly))
        .collect();
    assert_eq!(validate_start(&spec), Ok(()));
    let one = serde_json::to_string(&Command::JobStart(Box::new(spec))).unwrap();
    assert!(8 * one.len() <= REPLY_BODY_BUDGET, "{} bytes", one.len());
}

/// Asserts that a provider's name that is empty, of 33 bytes, starts with a digit, holds an
/// upper-case letter or a byte outside `[a-z0-9-]` is refused `bad-chars` on
/// `layers.provider` (`invalid-job`) for a layer and on `mounts.source.provider`
/// (`invalid-mount`) for a mount. It catches a name that reaches the capability key or the
/// provider's socket flag unchecked, and a refusal that names the wrong field.
#[test]
fn a_bad_provider_name_is_refused_on_its_field() {
    let too_long = long(MAX_PROVIDER_NAME_LEN + 1);
    for name in [
        "",
        too_long.as_str(),
        "1tier",
        "-tier",
        "Tier",
        "tier_store",
        "tier.store",
        "tier\0",
        "tier\u{e9}",
    ] {
        refused(
            |s| s.layers[2] = provider_layer(name, "k"),
            "layers.provider",
            Rule::BadChars,
            refusal::INVALID_JOB,
        );
        refused(
            |s| s.mounts[1] = provider_mount(name, "k", "/data", MountMode::ReadOnly),
            "mounts.source.provider",
            Rule::BadChars,
            refusal::INVALID_MOUNT,
        );
    }
}

/// Asserts that a provider's ref that is empty (`empty`), of 1025 bytes (`too-long`), or
/// holds a control character, a NUL or a format character (`bad-chars`) is refused on
/// `layers.ref` (`invalid-job`) for a layer and on `mounts.source.ref` (`invalid-mount`) for
/// a mount, and that the name is looked at before the ref. It catches a ref that reaches a
/// provider or a log line with a character that reorders or hides text, and a bound that is
/// off by one.
#[test]
fn a_bad_provider_ref_is_refused_on_its_field() {
    let too_long = long(MAX_PROVIDER_REF_LEN + 1);
    for (reference, rule) in [
        ("", Rule::Empty),
        (too_long.as_str(), Rule::TooLong),
        ("a\nb", Rule::BadChars),
        ("a\0b", Rule::BadChars),
        ("a\u{7f}", Rule::BadChars),
        ("a\u{202e}b", Rule::BadChars),
    ] {
        refused(
            |s| s.layers[2] = provider_layer("tierstore", reference),
            "layers.ref",
            rule,
            refusal::INVALID_JOB,
        );
        refused(
            |s| s.mounts[1] = provider_mount("tierstore", reference, "/data", MountMode::ReadOnly),
            "mounts.source.ref",
            rule,
            refusal::INVALID_MOUNT,
        );
    }
    refused(
        |s| s.layers[2] = provider_layer("Bad", ""),
        "layers.provider",
        Rule::BadChars,
        refusal::INVALID_JOB,
    );
    refused(
        |s| s.mounts[1] = provider_mount("Bad", "", "/data", MountMode::ReadOnly),
        "mounts.source.provider",
        Rule::BadChars,
        refusal::INVALID_MOUNT,
    );
}

/// Asserts that a provider source may not stand in a start that runs on the host itself,
/// as no layer or mount may: refused `invalid-job` on `isolation`. It catches a host job
/// given a tree it would never be confined to.
#[test]
fn a_host_start_may_not_name_a_provider() {
    refused(
        |s| {
            s.isolation = js_agent_wire::Isolation::Host;
            s.layers = vec![provider_layer("tierstore", "k")];
            s.mounts.clear();
        },
        "isolation",
        Rule::Inconsistent,
        refusal::INVALID_JOB,
    );
}

/// Asserts the exact text of a provider layer and of a provider mount source, `ref` under
/// that name and not `reference`, that each decodes back, and that a text that spells
/// the field `reference` is not a provider source. It catches a Rust field name that leaks
/// into the wire and a lost `type` tag.
#[test]
fn a_provider_source_has_the_documented_wire_form() {
    let layer = provider_layer("tierstore", "toolchains/clang-18");
    let text = json!({"type": "provider", "provider": "tierstore", "ref": "toolchains/clang-18"});
    assert_eq!(serde_json::to_value(&layer).unwrap(), text);
    assert_eq!(
        serde_json::from_value::<Layer>(text.clone()).unwrap(),
        layer
    );

    let source = MountSource::Provider {
        provider: "tierstore".into(),
        reference: "toolchains/clang-18".into(),
    };
    assert_eq!(serde_json::to_value(&source).unwrap(), text);
    assert_eq!(serde_json::from_value::<MountSource>(text).unwrap(), source);

    let renamed = json!({"type": "provider", "provider": "tierstore", "reference": "k"});
    assert!(serde_json::from_value::<Layer>(renamed.clone()).is_err());
    assert!(serde_json::from_value::<MountSource>(renamed).is_err());

    let mount = provider_mount("tierstore", "datasets/v3", "/data", MountMode::ReadWrite);
    assert_eq!(
        serde_json::to_value(&mount).unwrap(),
        json!({
            "source": {"type": "provider", "provider": "tierstore", "ref": "datasets/v3"},
            "target": "/data",
            "mode": "read_write"
        })
    );
    let command = Command::JobStart(Box::new(start()));
    let text = serde_json::to_string(&command).unwrap();
    assert_eq!(serde_json::from_str::<Command>(&text).unwrap(), command);
}

/// The layer of the release before the `provider` kind: the two kinds it had. It must not
/// follow later changes, because it stands for agents already deployed.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousLayer {
    HostPath { path: String },
    Image { image: String },
}

/// The mount source of that release: the one kind it had.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousMountSource {
    HostPath { path: String },
}

/// A mount of that release: its source as it knew it, and the other fields as they come.
#[derive(Debug, Deserialize, Serialize)]
struct PreviousMount {
    source: PreviousMountSource,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

/// The part of a job start that release read here: the id, the layers and the mounts.
#[derive(Debug, Deserialize)]
struct PreviousStart {
    job_id: String,
    #[serde(default)]
    layers: Vec<PreviousLayer>,
    #[serde(default)]
    mounts: Vec<PreviousMount>,
}

/// The commands of that release a test here sends; any other tag reads as `Unknown`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousCommand {
    JobStart(Box<PreviousStart>),
    JobStop {
        job_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// That release's reply decoding: each command on its own, one that does not decode read
/// as `Unknown`.
fn previous_commands(reply: &Value) -> Vec<PreviousCommand> {
    reply["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| PreviousCommand::deserialize(item).unwrap_or(PreviousCommand::Unknown))
        .collect()
}

/// `start()` as a command, with `edit` applied, as the text of a reply command.
fn start_text(edit: impl FnOnce(&mut JobStartSpec)) -> Value {
    let mut spec = start();
    edit(&mut spec);
    serde_json::to_value(Command::JobStart(Box::new(spec))).unwrap()
}

/// Asserts that an agent of the release before the `provider` kind reads a reply of a job
/// stop, a start with a provider layer, a start with a provider mount and a start with
/// neither as `Unknown` for each start with a provider and whole for the stop and the other
/// start; and that this release reads the same reply with every command whole and valid. It
/// catches a provider kind an old agent would take as another kind, which would run the job
/// on another root than the one asked for, and one that costs the commands beside it.
#[test]
fn a_previous_agent_reads_a_start_with_a_provider_as_unknown() {
    let plain = |s: &mut JobStartSpec| {
        s.layers.truncate(2);
        s.mounts.truncate(1);
    };
    let reply = json!({
        "commands": [
            {"type": "job_stop", "job_id": "job-0"},
            start_text(|s| {
                s.job_id = "job-2".into();
                s.booked.gpu_indices.clear();
                s.mounts.truncate(1);
            }),
            start_text(|s| {
                s.job_id = "job-3".into();
                s.booked.gpu_indices.clear();
                s.layers.truncate(2);
            }),
            start_text(plain)
        ]
    });
    let there = previous_commands(&reply);
    assert_eq!(there.len(), 4);
    assert!(matches!(&there[0], PreviousCommand::JobStop { job_id } if job_id == "job-0"));
    assert!(matches!(&there[1], PreviousCommand::Unknown));
    assert!(matches!(&there[2], PreviousCommand::Unknown));
    assert!(matches!(&there[3], PreviousCommand::JobStart(s) if s.job_id == "job-1"));

    let here: HeartbeatReply = serde_json::from_value(reply).unwrap();
    assert_eq!(here.commands.len(), 4);
    let Command::JobStart(first) = &here.commands[1] else {
        panic!("a job start: {:?}", here.commands);
    };
    assert_eq!(
        first.layers[2],
        provider_layer("tierstore", "toolchains/clang-18")
    );
    let Command::JobStart(second) = &here.commands[2] else {
        panic!("a job start: {:?}", here.commands);
    };
    assert_eq!(
        second.mounts[2],
        provider_mount("scratch-2", "cache", "/data/cache", MountMode::ReadWrite)
    );
    assert_eq!(here.validate().refused, []);
}

/// Asserts that a start with host-path and image layers and a host-path mount reads the
/// same to the release before the `provider` kind as to this one: it decodes there, and the
/// layers and mounts it writes back are the text this release writes, with no `provider`
/// key anywhere. It catches a change to the text of a start that names no provider, which
/// would break every agent already deployed.
#[test]
fn a_start_without_a_provider_is_the_previous_text() {
    let spec = JobStartSpec {
        layers: start().layers[..2].to_vec(),
        mounts: start().mounts[..1].to_vec(),
        ..start()
    };
    let text = serde_json::to_value(Command::JobStart(Box::new(spec.clone()))).unwrap();
    let PreviousCommand::JobStart(there) = PreviousCommand::deserialize(&text).unwrap() else {
        panic!("a job start");
    };
    assert_eq!(there.job_id, "job-1");
    assert_eq!(
        serde_json::to_value(&there.layers).unwrap(),
        serde_json::to_value(&spec.layers).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&there.mounts).unwrap(),
        serde_json::to_value(&spec.mounts).unwrap()
    );
    assert!(!text.to_string().contains("provider"), "{text}");
}
