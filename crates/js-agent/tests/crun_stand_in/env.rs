//! A job's environment in the bundle of its container: the image's variables first, the
//! job's over them, and nothing made for an environment the runtime refuses.

use std::collections::BTreeMap;
use std::path::PathBuf;

use js_agent::image::DEFAULT_PATH;
use js_agent::lifecycle::{RunSpec, Runtime};
use serde_json::Value;

use crate::node::{Node, given};

/// `node.spec(run_id)` with the environment `env`, given by name in order.
fn with_env(node: &Node, run_id: &str, env: &[(&str, &str)]) -> RunSpec {
    let mut spec = node.spec(run_id);
    spec.env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    spec
}

/// The `process.env` of the bundle the runtime makes for `spec`, in order, without the
/// `HOSTNAME` and `HOME` js-contain adds when neither the image nor the job sets them; the
/// container is staged, read and removed.
fn process_env(node: &Node, spec: &RunSpec) -> Vec<String> {
    let mut runtime = node.open();
    let id = runtime.stage(spec).expect("staged");
    let path = node.lease_of(&id.0).join("bundle/config.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let bundle: Value = serde_json::from_str(&text).expect("a config");
    let env = bundle["process"]["env"].as_array().expect("env");
    let env = env
        .iter()
        .map(|entry| entry.as_str().expect("a string").to_owned())
        .filter(|entry| !entry.starts_with("HOSTNAME=") && !entry.starts_with("HOME="))
        .collect();
    runtime.remove(&id).expect("removed");
    env
}

/// Proves a job's environment reaches the process of its crun container after the image's
/// own variables (the image's `A=1` and the `PATH` the agent gives an image without one),
/// each pair once and unchanged, a value with a space included. Catches an environment
/// dropped on the way to the bundle, one set before the image's config is applied (which
/// replaces the whole environment), and one that replaces the image's variables instead of
/// adding to them.
#[test]
fn a_job_start_env_reaches_the_crun_process() {
    let node = Node::new("env-reaches");
    let spec = with_env(&node, "e1-hold", &[("B", "two words"), ("C", "1")]);
    assert_eq!(
        process_env(&node, &spec),
        [
            "A=1".to_owned(),
            format!("PATH={DEFAULT_PATH}"),
            "B=two words".to_owned(),
            "C=1".to_owned(),
        ]
    );
}

/// Proves a job variable of a name the image also sets replaces the image's value: the
/// container holds one `A` and one `PATH`, both the job's. Catches the image's value
/// winning, and both values left in the environment.
#[test]
fn a_job_env_replaces_an_image_variable_of_the_same_name() {
    let node = Node::new("env-replaces");
    let spec = with_env(&node, "e1-hold", &[("A", "2"), ("PATH", "/opt/bin")]);
    assert_eq!(process_env(&node, &spec), ["A=2", "PATH=/opt/bin"]);
}

/// Proves a run with no environment gets the image's alone. Catches an empty environment
/// that changes the bundle.
#[test]
fn a_job_with_no_env_is_unchanged() {
    let node = Node::new("env-none");
    assert_eq!(
        process_env(&node, &node.spec("e1-hold")),
        ["A=1".to_owned(), format!("PATH={DEFAULT_PATH}")]
    );
}

/// Proves a run whose environment names a reserved variable (the loader's `LD_PRELOAD`,
/// the agent's own `JS_AGENT_TOKEN`), handed to the runtime without the wire's validation,
/// is refused with the wire's own words before anything is made: nothing is given away, no
/// lease directory, sidecar, pin or cgroup is there, and nothing is listed. Catches a
/// runtime that trusts the wire to have checked an environment it may never have seen.
#[test]
fn a_runtime_refuses_a_reserved_env_name_it_is_handed_directly() {
    let node = Node::new("env-reserved");
    let mut runtime = node.open();
    let before = given(&node.dir).len();
    for name in ["LD_PRELOAD", "JS_AGENT_TOKEN"] {
        let spec = with_env(&node, "e1-exit", &[(name, "x")]);
        let why = runtime.create_and_start(&spec).expect_err(name);
        let reserved = BTreeMap::from([(name.to_owned(), "x".to_owned())]);
        let wire = js_agent_wire::check_env("env", &reserved).expect_err(name);
        assert_eq!(why.to_string(), wire.to_string(), "{name}");
    }
    assert_eq!(given(&node.dir).len(), before);
    assert!(runtime.list_labelled().expect("lists").is_empty());
    let dirs = [
        node.scratch.clone(),
        node.scratch.join("oci-labels"),
        node.scratch.join("oci-image-pins/leases"),
        node.cgroups.join("actions"),
    ];
    let left: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().contains("e1-exit"))
        .collect();
    assert_eq!(left, Vec::<PathBuf>::new());
}
