//! The log tail of a job that exited: read before the container is removed, carried by
//! the result, and never able to change the result.

use js_agent_wire::{LogTail, Outcome};

use super::{Harness, job};

fn tail() -> LogTail {
    LogTail {
        stdout: "compiled\n".to_owned(),
        stderr: "error: boom\n".to_owned(),
        truncated: false,
    }
}

/// Asserts that the result of a job that exited carries the tail the runtime gave, and
/// that the log was read before the container was removed (the runtime has no log to read
/// once the container is gone). It catches a result sent without the tail, and a read
/// placed after the removal, which would find nothing.
#[test]
fn the_result_carries_the_tail_read_before_the_removal() {
    let mut h = Harness::new("log-tail");
    h.start(job("r1"));
    h.fake.print("r1", tail());
    h.fake.exit("r1", 3);
    h.life.step();
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Failed);
    assert_eq!(results[0].log_tail, Some(tail()));
    let log = h.log.borrow();
    let at = |prefix: &str| log.iter().position(|line| line.starts_with(prefix));
    let (read, removed) = (at("logs c1").unwrap(), at("remove c1").unwrap());
    assert!(read < removed, "{log:?}");
}

/// Asserts that a log that cannot be read leaves the result as it would have been without
/// it (the same outcome and exit code, the container removed, the slot free) but without a
/// tail, and notes a `logs-failed` finding for the operator. It catches a run that fails,
/// hangs or keeps its container because its log was unreadable.
#[test]
fn a_failed_logs_call_still_yields_the_result_without_a_tail() {
    let mut h = Harness::new("log-fail");
    h.start(job("r1"));
    h.fake.print("r1", tail());
    h.fake.fail("logs", u32::MAX);
    h.fake.exit("r1", 0);
    h.life.step();
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Succeeded);
    assert_eq!(results[0].exit_code, Some(0));
    assert_eq!(results[0].log_tail, None);
    assert!(h.fake.gone("r1"), "the container is removed all the same");
    assert_eq!(h.life.free().cpu_millis, 4000, "the slot is free");
    let findings = h.life.findings();
    assert!(
        findings.iter().any(|found| found.code == "logs-failed"),
        "{findings:?}"
    );
}

/// Asserts that a job whose runtime keeps no log (a tail of `None`) is reported without a
/// tail and without a finding. It catches a backend that cannot read logs being reported
/// as a failure on every run.
#[test]
fn a_runtime_without_logs_gives_no_tail_and_no_finding() {
    let mut h = Harness::new("log-none");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.results()[0].log_tail, None);
    assert!(h.life.findings().is_empty());
}
