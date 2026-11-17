//! A job's declared outputs with `CrunRuntime` against the stand-in: what the job left in
//! `/work`'s writable layer is put in the store when the run is removed, by a thread of its own
//! the test steps (its wake is manual), after the lease directory is gone.

use std::fs;

use js_agent::lifecycle::{Runtime, Status};
use js_logstore::JobPrefix;
use js_logstore::outputs::OutputPath;
use sha2::{Digest as _, Sha256};

use crate::logs::{Live, run_out, sink};
use crate::node::Node;

/// Proves the outputs of a run with a sink: once it has ended and is removed, the file it left
/// below a declared directory is at `P/outputs/<path>` and the manifest lists it with its
/// length and digest and the declared path the job did not make, though the lease directory
/// (the writable layer the file was read from) is gone. Catches outputs read after the layer
/// is removed, and a removal that waits for the upload.
#[test]
fn a_runs_outputs_are_put_when_it_is_removed() {
    let node = Node::new("outputs-put");
    let live = Live::new();
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j9-say", 60_000);
    spec.log_sink = Some(sink("j9-say"));
    spec.outputs = vec!["out".to_owned(), "absent".to_owned()];
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    run_out(&live.wake(0));
    let upper = node.lease_of(&id.0).join("upper");
    fs::create_dir_all(upper.join("out")).expect("a directory");
    fs::write(upper.join("out/report.json"), "{}\n").expect("a file");
    runtime.remove(&id).expect("removed");
    assert!(!node.lease_of(&id.0).exists(), "the lease is gone");
    run_out(&live.wake(1));
    let prefix = JobPrefix::new("js", "j9-say").expect("a prefix");
    let objects = live.store.objects();
    let report = OutputPath::parse("out/report.json").expect("a path");
    assert_eq!(objects[&prefix.output_key(&report)], b"{}\n");
    let sha: String = Sha256::digest(b"{}\n")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        String::from_utf8(objects[&prefix.outputs_manifest_key()].clone()).expect("text"),
        format!(
            r#"{{"version":1,"files":[{{"path":"out/report.json","bytes":3,"sha256":"{sha}"}}],"missing":["absent"],"failed":[]}}"#
        )
    );
}

/// Proves that a run whose start declared no outputs makes no outputs thread and puts nothing
/// under `outputs`. Catches a manifest put for every job.
#[test]
fn a_run_with_no_outputs_puts_none() {
    let node = Node::new("outputs-none");
    let live = Live::new();
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j8-say", 60_000);
    spec.log_sink = Some(sink("j8-say"));
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    run_out(&live.wake(0));
    runtime.remove(&id).expect("removed");
    assert_eq!(live.made(), 1, "only the logs' uploader");
    let keys: Vec<String> = live.store.objects().into_keys().collect();
    assert!(keys.iter().all(|key| !key.contains("outputs")), "{keys:?}");
}
