//! The finding `outputs-committed`: what a runtime's removal of a run committed of its
//! outputs to the put ingest (`Runtime::outputs_report`) is kept by the cleanup as a finding
//! whose message is the JSON of the object's key, digest and size. Only the native runtime of
//! a Mac commits; the fake stands in for it here, so the lifecycle's side runs on Linux.

use js_agent::lifecycle::{Event, OutputsReport, code};
use serde_json::{Value, json};

use super::deferring::Deferring;
use super::{Harness, cleanup, job, names};

fn report() -> OutputsReport {
    OutputsReport {
        key: "cas/b/r-build/n1/pack".to_owned(),
        sha256: "ab".repeat(32),
        bytes: 10_240,
    }
}

/// The message of each finding among `events` coded `code`.
fn said(events: &[Event], code: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Finding(finding) if finding.code == code => Some(finding.message.clone()),
            _ => None,
        })
        .collect()
}

/// The events of a run that exited 0 and was cleaned up, with `finding` after its result.
fn ended_with(run_id: &str, finding: Option<&str>) -> Vec<String> {
    let mut want = vec![format!("result {run_id}")];
    want.extend(finding.map(|code| format!("finding {run_id} {code}")));
    want.extend(cleanup(run_id));
    want
}

/// The finding's text for [`report`], whole.
fn report_said() -> String {
    format!(
        r#"{{"bytes":10240,"key":"cas/b/r-build/n1/pack","sha256":"{}"}}"#,
        "ab".repeat(32)
    )
}

/// Proves a removal that committed outputs is kept as the one finding `outputs-committed`,
/// made once the removal is done and before the container is reported removed, whose message
/// is exactly the JSON of the key, the digest and the size the runtime reported; the result
/// is the job's exit and the host is not tainted. Catches a commit report lost on the way to
/// the server, one whose message is not the JSON of all three, and a commit taken for a
/// failure.
#[test]
fn a_committed_removal_is_noted_with_its_key_digest_and_size() {
    let mut h = Harness::new("outputs-committed");
    h.start(job("r1"));
    h.fake.commit_outputs("r1", report());
    h.fake.exit("r1", 0);
    let events = h.life.step();
    assert_eq!(
        names(&events),
        ended_with("r1", Some(code::OUTPUTS_COMMITTED))
    );
    let said = said(&events, code::OUTPUTS_COMMITTED);
    assert_eq!(said, [report_said()]);
    let parsed: Value = serde_json::from_str(&said[0]).unwrap();
    let want = json!({"key": "cas/b/r-build/n1/pack", "sha256": "ab".repeat(32), "bytes": 10_240});
    assert_eq!(parsed, want);
    assert_eq!(h.life.results()[0].exit_code, Some(0));
    assert!(!h.life.is_tainted());
}

/// Proves a removal that runs off the worker and is pending for a step is asked for its
/// commit only once it is done, and gives exactly one `outputs-committed` finding however
/// many steps asked about the removal. Catches a finding made twice, or lost, when the
/// removal takes steps to be done.
#[test]
fn a_pending_removal_is_noted_once_when_it_is_done() {
    let (mut h, gate) = Deferring::harness("outputs-pending");
    h.start(job("r1"));
    h.fake.commit_outputs("r1", report());
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ["result r1"]);
    assert!(names(&h.life.step()).is_empty(), "the removal still runs");
    gate.release_removal("c1");
    let mut events = h.life.step();
    events.extend(h.life.step());
    let mut want = vec!["finding r1 outputs-committed".to_owned()];
    want.extend(cleanup("r1"));
    assert_eq!(names(&events), want);
    assert_eq!(said(&events, code::OUTPUTS_COMMITTED), [report_said()]);
}

/// Proves a runtime that committed nothing (every runtime but a Mac's native one, on the
/// trait's default) gives no `outputs-committed` finding, and neither does a run whose
/// container another run's report was given to: the report is answered for the removed
/// container it belongs to, once. Catches a note made for a run that committed nothing.
#[test]
fn a_removal_that_committed_nothing_is_not_noted() {
    let mut h = Harness::new("outputs-none");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    assert_eq!(names(&h.life.step()), ended_with("r1", None));

    h.start(job("r2"));
    h.fake.commit_outputs("r3", report());
    h.fake.exit("r2", 0);
    assert_eq!(names(&h.life.step()), ended_with("r2", None));
}

/// Proves a removal that fails as its outputs not copied (or not committed) is the finding
/// `outputs-failed` with the runtime's text and no `outputs-committed` finding, the cleanup
/// goes on in its order, and the host is not tainted. Catches a commit note for a run whose
/// outputs were not committed, and an outputs failure that taints the host.
#[test]
fn a_removal_failed_as_outputs_is_noted_failed_and_not_committed() {
    let mut h = Harness::new("outputs-failed");
    h.start(job("r1"));
    h.fake.fail("outputs", 1);
    h.fake.exit("r1", 0);
    let events = h.life.step();
    assert_eq!(names(&events), ended_with("r1", Some(code::OUTPUTS_FAILED)));
    let failed = said(&events, code::OUTPUTS_FAILED);
    assert_eq!(
        failed,
        ["outputs not copied: out: a symbolic link: not followed"]
    );
    assert!(said(&events, code::OUTPUTS_COMMITTED).is_empty());
    assert!(!h.life.is_tainted());
}
