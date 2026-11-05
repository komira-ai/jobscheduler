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

/// The old `image::image`, body copied from `crates/js-agent-wire/src/image.rs` as of
/// main before the `native` body existed (commit 51bd69748). Its return type and the
/// `Rule` names are local stand-ins for the crate-private ones; the three checks and their
/// order are the original. The reference grammar, `is_reference`, is not copied: every
/// image in these tests is a well-formed one it accepted, so it is a stub that says yes.
mod old_image {
    pub const MAX_IMAGE_LEN: usize = js_agent_wire::limits::MAX_IMAGE_LEN;

    #[derive(Debug, PartialEq)]
    pub enum Rule {
        Empty,
        TooLong,
        BadChars,
    }

    fn is_reference(_value: &str) -> bool {
        true
    }

    pub fn image(value: &str) -> Result<(), Rule> {
        if value.is_empty() {
            return Err(Rule::Empty);
        }
        if value.len() > MAX_IMAGE_LEN {
            return Err(Rule::TooLong);
        }
        if !is_reference(value) {
            return Err(Rule::BadChars);
        }
        Ok(())
    }
}

/// The old `Command::validate` for a start as far as the image: both ids are tokens, then
/// the image is checked by the copy above, and a refusal there is `invalid-image` before
/// anything runs.
fn old_validate(spec: &OldJobSpec) -> Result<(), &'static str> {
    let token = |text: &str| !text.is_empty() && text.len() <= 128;
    if !token(&spec.run_id) || !token(&spec.lease_id) {
        return Err(refusal::INVALID_JOB);
    }
    old_image::image(&spec.image).map_err(|_| refusal::INVALID_IMAGE)
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
        ports: Vec::new(),
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

/// The native body of an agent from before `open`, as far as its network: the level had
/// three values, and every enum of the wire decodes only the values it names. Frozen: it
/// stands for agents already deployed and must not follow `NetworkLevel`.
mod before_open {
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum NetworkLevel {
        Off,
        Loopback,
        On,
    }

    #[derive(Debug, Deserialize)]
    pub struct NativeSpec {
        pub program: String,
        #[serde(default = "off")]
        pub network: NetworkLevel,
    }

    fn off() -> NetworkLevel {
        NetworkLevel::Off
    }

    /// A start as that agent decoded it, as far as the native body.
    #[derive(Debug, Deserialize)]
    pub struct Start {
        pub native: NativeSpec,
    }
}

/// Proves that every level a native body can ask for is written as its own word and reads
/// back as itself: `off`, `loopback`, `on` and the new `open`, which is not written as `on`.
/// Catches `open` aliased to `on` on the wire (an agent would run an open job behind the
/// filter, or a filtered one without it) and a level that is written one way and read another.
#[test]
fn open_level_round_trips() {
    let levels = [
        (NetworkLevel::Off, "off"),
        (NetworkLevel::Loopback, "loopback"),
        (NetworkLevel::On, "on"),
        (NetworkLevel::Open, "open"),
    ];
    for (level, word) in levels {
        let mut body = native();
        body.network = level;
        let wire = serde_json::to_value(&body).unwrap();
        assert_eq!(wire["network"], json!(word), "{level:?}");
        let back: NativeSpec = serde_json::from_value(wire).unwrap();
        assert_eq!(back.network, level, "{word}");
    }
}

/// Proves that an agent from before `open` refuses a start that asks for it rather than run
/// it at another level: its decoder fails on the word (and a reply that carries such a start
/// reads it as a command it does not know, which runs nothing, as this version reads any
/// level it does not know), while the levels it knew still decode as before. Catches an
/// `open` that an old agent would read as `on`, `off` or a default, and so run with no
/// network or behind a filter the job did not ask for.
#[test]
fn old_agent_reads_open_as_invalid() {
    let mut start = native_job();
    start["type"] = json!("start");
    start["native"]["network"] = json!("on");
    let on: before_open::Start = serde_json::from_value(start.clone()).unwrap();
    assert_eq!(on.native.network, before_open::NetworkLevel::On);
    assert_eq!(on.native.program, "/usr/bin/clang");

    let mut body = native();
    body.network = NetworkLevel::Open;
    start["native"] = serde_json::to_value(&body).unwrap();
    assert_eq!(start["native"]["network"], json!("open"));
    let old = serde_json::from_value::<before_open::Start>(start.clone());
    assert!(old.is_err(), "an old agent decodes no open start: {old:?}");

    let mut unknown = start.clone();
    unknown["native"]["network"] = json!("wide");
    let stop = json!({"type": "stop", "run_id": "r"});
    let reply: js_agent_wire::HeartbeatReply =
        serde_json::from_value(json!({"commands": [unknown, stop]})).unwrap();
    assert_eq!(
        reply.commands,
        vec![
            js_agent_wire::Command::Unknown,
            js_agent_wire::Command::Stop { run_id: "r".into() }
        ]
    );
    let reply: js_agent_wire::HeartbeatReply =
        serde_json::from_value(json!({"commands": [start]})).unwrap();
    assert!(
        matches!(
            reply.commands.as_slice(),
            [js_agent_wire::Command::Start(_)]
        ),
        "this version reads open: {:?}",
        reply.commands
    );
}
