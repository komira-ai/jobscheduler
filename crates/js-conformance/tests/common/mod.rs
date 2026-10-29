//! What the registry and layered conformance tests share.

use std::collections::BTreeMap;

use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_types::Shape;

/// A hello for `host` in the pool `pool`, claiming exactly `shape`.
pub fn hello(host: &str, shape: &Shape) -> Hello {
    let resources = shape.resources();
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: host.into(),
        pool: "pool".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            gpus: resources.gpus(),
            gpu_class: resources.gpu_class().map(str::to_string),
            max_leases: None,
        },
        capabilities: shape.capabilities().clone(),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
    }
}
