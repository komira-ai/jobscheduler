//! The FUSE device in the bundle of a contained run: a run with `fuse` gets `/dev/fuse` and
//! `CAP_SYS_ADMIN` of its own user namespace, a plain run gets neither.

use js_agent::lifecycle::{RunSpec, Runtime};
use serde_json::{Value, json};

use crate::node::Node;

/// The five capability sets of a process, by their names in the bundle.
const SETS: [&str; 5] = [
    "bounding",
    "effective",
    "permitted",
    "inheritable",
    "ambient",
];

/// The bundle the runtime makes for `spec`; the container is staged, read and removed.
fn bundle_of(node: &Node, spec: &RunSpec) -> Value {
    let mut runtime = node.open();
    let id = runtime.stage(spec).expect("staged");
    let path = node.lease_of(&id.0).join("bundle/config.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    runtime.remove(&id).expect("removed");
    serde_json::from_str(&text).expect("a config")
}

/// The paths of the nodes a bundle gives its container (none when it has no device section).
fn device_paths(bundle: &Value) -> Vec<String> {
    bundle["linux"]["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|device| device["path"].as_str().expect("a path").to_owned())
        .collect()
}

/// Proves a run with `fuse` reaches the bundle as the FUSE device and nothing wider: the one
/// node `/dev/fuse` (character 10:229, mode 0666), `CAP_SYS_ADMIN` alone in each of the five
/// capability sets, `noNewPrivileges` still on, and the seccomp filter allowing the three
/// io_uring calls; and a plain run's bundle has no `/dev/fuse` and no `CAP_SYS_ADMIN` in any
/// set. Catches a `fuse` that never leaves the lifecycle's run, a device given to every run,
/// and a capability set left wider than the one the job needs.
#[test]
fn a_fuse_run_gets_the_fuse_device_and_cap_sys_admin_and_a_plain_run_gets_neither() {
    let node = Node::new("fuse-bundle");
    let fuse = bundle_of(
        &node,
        &RunSpec {
            fuse: true,
            ..node.spec("f1-hold")
        },
    );
    assert_eq!(device_paths(&fuse), ["/dev/fuse"]);
    let device = &fuse["linux"]["devices"][0];
    assert_eq!(
        (&device["type"], &device["major"], &device["minor"]),
        (&json!("c"), &json!(10), &json!(229))
    );
    assert_eq!(device["fileMode"], json!(0o666));
    let held = &fuse["process"]["capabilities"];
    for set in SETS {
        assert_eq!(held[set], json!(["CAP_SYS_ADMIN"]), "{set}");
    }
    assert_eq!(fuse["process"]["noNewPrivileges"], json!(true));
    let calls: Vec<&str> = fuse["linux"]["seccomp"]["syscalls"]
        .as_array()
        .expect("seccomp rules")
        .iter()
        .flat_map(|rule| rule["names"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    for call in ["io_uring_setup", "io_uring_enter", "io_uring_register"] {
        assert!(calls.contains(&call), "{call}");
    }

    let plain = bundle_of(&node, &node.spec("p1-hold"));
    assert!(device_paths(&plain).is_empty());
    for set in SETS {
        let names = plain["process"]["capabilities"][set].as_array();
        let names = names.into_iter().flatten();
        assert!(
            !names.into_iter().any(|name| name == "CAP_SYS_ADMIN"),
            "{set}"
        );
    }
    assert_eq!(plain["process"]["noNewPrivileges"], json!(true));
}
