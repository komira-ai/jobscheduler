//! A job start's declared outputs (`docs/design/job-outputs.md`): their wire form, their bound,
//! and that a start from a server that predates the field reads as one with none.

use js_agent_wire::limits::MAX_JOB_OUTPUTS;
use js_agent_wire::{BookedResources, Command, JobStartSpec, Rule, refusal};
use serde_json::{Value, json};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with `outputs`.
fn start(outputs: &[&str]) -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image: format!("ubuntu:24.04@{DIGEST}"),
        command: vec!["make".into()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
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
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        outputs: outputs.iter().map(|path| (*path).to_owned()).collect(),
    }
}

/// Asserts that a start's outputs are the list `outputs` in its JSON, in order, and read back
/// as written; that a start with none leaves the field out; and that the JSON of a start that has
/// no `outputs` (a server's from before the field) reads as a start with none. It catches the
/// field renamed, sent empty, or required of a server that does not send it.
#[test]
fn outputs_are_a_list_left_out_when_empty() {
    let with = start(&["out", "report.json"]);
    let text = serde_json::to_value(&with).unwrap();
    assert_eq!(text["outputs"], json!(["out", "report.json"]));
    let back: JobStartSpec = serde_json::from_value(text).unwrap();
    assert_eq!(back, with);
    let without = serde_json::to_value(start(&[])).unwrap();
    assert_eq!(without.get("outputs"), None);
    let mut older: Value = serde_json::to_value(start(&[])).unwrap();
    older.as_object_mut().unwrap().remove("outputs");
    let read: JobStartSpec = serde_json::from_value(older).unwrap();
    assert!(read.outputs.is_empty());
}

/// Asserts that a start may declare up to 64 outputs and that the 65th is refused
/// `invalid-job` naming `outputs` with the rule `too-many`; and that the paths themselves are
/// not judged by the start's check (the agent holds them to the store's rules when it uploads).
/// It catches an unbounded list and a path refused before the job could run.
#[test]
fn a_start_declares_at_most_64_outputs() {
    let paths: Vec<String> = (0..=MAX_JOB_OUTPUTS).map(|at| format!("f{at}")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let validate = |spec: JobStartSpec| Command::JobStart(Box::new(spec)).validate();
    assert_eq!(validate(start(&refs[..MAX_JOB_OUTPUTS])), Ok(()));
    let refused = validate(start(&refs)).unwrap_err();
    assert_eq!(
        (refused.code(), refused.field(), refused.rule()),
        (refusal::INVALID_JOB, "outputs", Rule::TooMany)
    );
    assert_eq!(validate(start(&["../outside", "a b"])), Ok(()));
}
