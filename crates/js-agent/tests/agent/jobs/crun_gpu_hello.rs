//! The hello of a whole `--runtime crun` agent over the node of `crun_e2e` (its stand-in files,
//! store and cgroups, and the real crun runtime opened over them): `gpu-passthrough` is
//! reported by an agent given a device list and by no other, and an operator cannot claim it.
//! `/dev/null` stands for a GPU's node: a character device every Linux host has, so the start
//! check reads a real node.

use std::sync::Arc;

use js_agent::Machine;
use serde_json::Value;

use super::crun_e2e::{CrunMachine, NODE_IN_USE, crun_flags, host};
use crate::support::{
    CountingSleeper, Jobs, Lowest, Ran, Script, ScriptConnector, code, flags, refused, run_on,
};

/// Runs a crun agent of the node with `extra` until the server refuses its hello; returns
/// what it printed and the hello it sent (`null` when it sent none).
fn crun_hello(extra: &[&str]) -> (Ran, Value) {
    let owned = crun_flags("h1", extra);
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![refused(403, "host-bound")]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine: Arc<dyn Machine> = CrunMachine::new();
    let jobs: Jobs<'_> = (machine, None);
    let ran = run_on(
        &flags(&all),
        &host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Proves a crun agent on a host with two GPUs reports `gpu-passthrough=true` beside its
/// runtime when it is given `--gpu-device`, and does not report it without one; and that
/// `--capability gpu-passthrough=true` is refused naming the flag before a hello is sent.
/// Catches an agent that claims to pass GPUs it has no device list for (its GPU jobs would be
/// refused `gpu-unsupported`), one that does not say it passes them, and a capability an
/// operator could set by hand.
#[test]
fn the_hello_reports_gpu_passthrough_only_with_a_device_list() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let (ran, hello) = crun_hello(&["--gpu-device", "/dev/null"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["gpu-passthrough"], "true", "{hello}");
    assert_eq!(caps["runtime"], "container", "{hello}");
    assert_eq!(hello["shape"]["gpus"], 2, "{hello}");

    let (ran, hello) = crun_hello(&[]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["runtime"], "container", "{hello}");
    assert!(caps.get("gpu-passthrough").is_none(), "{hello}");

    let (ran, hello) = crun_hello(&["--capability", "gpu-passthrough=true"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said = "--capability: gpu-passthrough is detected and cannot be set";
    assert!(ran.err.contains(said), "{}", ran.err);
    assert_eq!(hello, Value::Null);
}
