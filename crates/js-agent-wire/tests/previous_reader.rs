//! A reader of the previous release against the hello of this one. The hello is JSON read
//! by serde with unknown fields skipped, so a host that sends `shape.accelerator` must stay
//! readable by a server that does not know it. The copies below are fixtures: they stand
//! for servers already deployed and must not follow later changes to `WireShape` or `Hello`.

mod common;

use std::collections::BTreeMap;

use js_agent_wire::{GpuDevice, HeldRun, Phase, ProtocolRange, WireAccelerator};
use serde::Deserialize;
use serde_json::{Value, json};

/// The previous release's `WireShape`: no `accelerator`.
#[derive(Debug, Deserialize)]
struct PreviousShape {
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: u64,
    #[serde(default)]
    disk_bytes: u64,
    #[serde(default)]
    gpu_class: Option<String>,
    #[serde(default)]
    max_leases: Option<u64>,
}

/// The previous release's `Hello`: the same fields, with the previous shape.
#[derive(Debug, Deserialize)]
struct PreviousHello {
    protocol: ProtocolRange,
    host_id: String,
    pool: String,
    agent_version: String,
    shape: PreviousShape,
    #[serde(default)]
    capabilities: BTreeMap<String, String>,
    #[serde(default)]
    gpu_devices: Vec<GpuDevice>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    phase: Phase,
    #[serde(default)]
    held: Vec<HeldRun>,
}

fn new_hello_json() -> Value {
    let mut hello = common::hello();
    hello.shape.disk_bytes = 1 << 30;
    hello.shape.accelerator = Some(WireAccelerator {
        vendor: "nvidia".into(),
        model: "nvidia-a100".into(),
        variant_spelling: None,
        count: 1,
        memory_per_device_bytes: Some(1 << 36),
    });
    serde_json::to_value(&hello).expect("the hello serializes")
}

/// Asserts that the shape of a new hello, which carries `accelerator`, decodes in the
/// previous release's `WireShape` with every legacy field equal to what was sent. Catches
/// a change to the shape that a server already deployed cannot read.
#[test]
fn a_previous_reader_keeps_the_legacy_fields_of_a_new_shape() {
    let json = new_hello_json();
    assert!(json["shape"].get("accelerator").is_some());
    let shape: PreviousShape =
        serde_json::from_value(json["shape"].clone()).expect("the previous shape decodes");
    assert_eq!(shape.cpu_millis, 8000);
    assert_eq!(shape.memory_bytes, 1 << 34);
    assert_eq!(shape.gpus, 1);
    assert_eq!(shape.disk_bytes, 1 << 30);
    assert_eq!(shape.gpu_class.as_deref(), Some("nvidia-a100"));
    assert_eq!(shape.max_leases, Some(4));
}

/// Asserts that a whole new hello with an accelerator decodes in the previous release's
/// `Hello` and keeps every legacy field, and that the extra shape field and an unknown
/// top-level field are skipped. Catches a new hello field that makes an older server
/// refuse the session.
#[test]
fn a_previous_reader_keeps_the_legacy_fields_of_a_new_hello() {
    let mut json = new_hello_json();
    json["field_from_a_later_release"] = json!({"any": ["thing"]});
    let hello: PreviousHello =
        serde_json::from_value(json).expect("the previous hello decodes a new one");
    let sent = common::hello();
    assert_eq!(hello.protocol, sent.protocol);
    assert_eq!(hello.host_id, "mac-01");
    assert_eq!(hello.pool, "builders");
    assert_eq!(hello.agent_version, "0.1.0");
    assert_eq!(hello.shape.gpus, 1);
    assert_eq!(hello.shape.gpu_class.as_deref(), Some("nvidia-a100"));
    assert_eq!(hello.shape.cpu_millis, 8000);
    assert_eq!(hello.capabilities, sent.capabilities);
    assert_eq!(hello.gpu_devices, sent.gpu_devices);
    assert_eq!(hello.labels, sent.labels);
    assert_eq!(hello.phase, Phase::Serving);
    assert_eq!(hello.held, sent.held);
}
