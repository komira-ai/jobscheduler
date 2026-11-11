//! Both directions of the wire between this release and the release before a job start's
//! `layers`, `mounts` and `host_ports` (B8).
//!
//! That release's starts are frozen in `data/previous_release_job_starts.jsonl`, written by
//! its own code: a container start with every field it had, a start with a `run`, `os`,
//! `arch` and host isolation, and a VM start. Its decoding of a job start is the frozen
//! `PreviousJobStart` below, the fields it had, with its defaults; it must not follow later
//! changes, because it stands for agents already deployed.

use std::collections::BTreeMap;

use js_agent_wire::{Command, Layer, MountMode, PathMount};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PREVIOUS: &str = include_str!("data/previous_release_job_starts.jsonl");

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The job start of the release before the three fields: what it read, and what it wrote
/// back. It ignores fields it does not know.
#[derive(Debug, Deserialize, Serialize)]
struct PreviousJobStart {
    job_id: String,
    allocation_id: String,
    #[serde(default)]
    image: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    arch: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    files: Vec<Value>,
    booked: Value,
    wall_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ports: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_sink: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    isolation: Option<String>,
}

/// The commands of that release a test here sends; any other tag reads as `Unknown`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreviousCommand {
    JobStart(Box<PreviousJobStart>),
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

/// The frozen container start, with `fields` added to its text.
fn layered(fields: Value) -> Value {
    let mut wire: Value = serde_json::from_str(PREVIOUS.lines().next().unwrap()).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        wire[key] = value.clone();
    }
    wire
}

/// Asserts that every start the previous release wrote decodes here with no layers, mounts
/// or host ports, validates, and is written back byte for byte. It catches a new field that
/// is sent when empty, which would change the text of every start a server sends, and a
/// default that reads an old start as layered.
#[test]
fn a_start_without_the_new_fields_is_the_previous_text() {
    let lines: Vec<&str> = PREVIOUS.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in lines {
        let command: Command = serde_json::from_str(line).unwrap();
        assert_eq!(command.validate(), Ok(()), "{line}");
        let Command::JobStart(start) = &command else {
            panic!("a job start: {line}");
        };
        assert!(start.layers.is_empty(), "{line}");
        assert!(start.mounts.is_empty(), "{line}");
        assert!(start.host_ports.is_empty(), "{line}");
        assert_eq!(serde_json::to_string(&command).unwrap(), line);
    }
}

/// Asserts the hazard placement guards against (design 2.4): an agent of the previous
/// release decodes a start with an image and layers, mounts and host ports as a job start,
/// keeps its image and command, and has nothing of the three; written back, it is the start
/// without them, so it would run the job on its image alone. A layers-only start (no image)
/// reads there with an empty image, which that release refuses `invalid-job`. It catches
/// the three fields made required or given a decode-failing form, which would change what an
/// old agent does with such a start without the capability that keeps it from getting one.
#[test]
fn a_previous_agent_drops_the_new_fields() {
    let image = format!("ubuntu:24.04@sha256:{HEX}");
    let new_fields = json!({
        "layers": [{"type": "host_path", "path": "/srv/trees/toolchain"}],
        "mounts": [{"source": "/srv/trees/src", "target": "/work", "mode": "copy_on_write"}],
        "host_ports": [9000]
    });
    let wire = layered(new_fields);
    let here: Command = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(here.validate(), Ok(()));
    let Command::JobStart(start) = &here else {
        panic!("a job start");
    };
    assert_eq!(
        start.layers,
        [Layer::HostPath {
            path: "/srv/trees/toolchain".into()
        }]
    );
    assert_eq!(
        start.mounts,
        [PathMount {
            source: "/srv/trees/src".into(),
            target: "/work".into(),
            mode: MountMode::CopyOnWrite,
        }]
    );
    assert_eq!(start.host_ports, [9000]);

    let there: PreviousJobStart = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(there.image, image);
    assert_eq!(there.command, ["make", "all"]);
    let mut without = wire;
    let fields = without.as_object_mut().unwrap();
    for key in ["layers", "mounts", "host_ports", "type"] {
        fields.remove(key);
    }
    assert_eq!(serde_json::to_value(&there).unwrap(), without);

    let mut bare = layered(json!({
        "command": ["/usr/bin/make"],
        "layers": [{"type": "host_path", "path": "/srv/root"}]
    }));
    bare.as_object_mut().unwrap().remove("image");
    let here: Command = serde_json::from_value(bare.clone()).unwrap();
    assert_eq!(here.validate(), Ok(()));
    let there: PreviousJobStart = serde_json::from_value(bare).unwrap();
    assert_eq!(there.image, "");
}

/// Asserts that a reply holding a layered start, between a job stop and a start of the
/// previous form, reads to the previous release as three commands: the layered start as a
/// job start (its new fields unread), and the stop and the other start whole; and that this
/// release reads the same reply with the layered start's fields intact. It catches a new
/// field that would cost an old agent the commands around it.
#[test]
fn a_layered_start_costs_a_previous_agent_no_other_command() {
    let plain: Value = serde_json::from_str(PREVIOUS.lines().nth(2).unwrap()).unwrap();
    let reply = json!({
        "commands": [
            {"type": "job_stop", "job_id": "job-0"},
            layered(json!({"host_ports": [9000, 9443]})),
            plain
        ]
    });
    let there = previous_commands(&reply);
    assert_eq!(there.len(), 3);
    assert!(matches!(&there[0], PreviousCommand::JobStop { job_id } if job_id == "job-0"));
    assert!(matches!(&there[1], PreviousCommand::JobStart(s) if s.job_id == "job-1"));
    assert!(matches!(&there[2], PreviousCommand::JobStart(s) if s.kind.as_deref() == Some("vm")));
    let here: js_agent_wire::HeartbeatReply = serde_json::from_value(reply).unwrap();
    let Command::JobStart(start) = &here.commands[1] else {
        panic!("a job start: {:?}", here.commands);
    };
    assert_eq!(start.host_ports, [9000, 9443]);
    assert_eq!(here.validate().refused, []);
}
