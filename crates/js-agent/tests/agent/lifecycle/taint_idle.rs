//! The taint and the idle baseline: a host a check or a leftover container tainted is not
//! reported idle to the checks.

use js_agent::lifecycle::CheckFinding;

use super::{Harness, Options, job, names};

/// The labels of a container no job can stand for (its run id names a path) that is not a
/// host job's.
const ODD: [(&str, &str); 2] = [
    ("jobscheduler.dev/run-id", "../x"),
    ("jobscheduler.dev/isolation", "contained"),
];

/// Asserts that a start on a host that a check tainted is refused before the checks are
/// told the host is idle: the first job starts on an idle host (one idle call), its check
/// finds something, and the next start is refused `host-tainted` with no second call. An
/// idle call there would take a new baseline over what the job left. It catches the idle
/// call moved above the refusal, or the taint guard dropped from it.
#[test]
fn a_tainted_host_refuses_a_start_before_the_checks_hear_it_is_idle() {
    let mut h = Harness::with(
        "idle-after-taint",
        Options {
            findings: vec![("r1", CheckFinding::residue("new-mount", "/mnt/x"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    assert_eq!(h.fake.idles(), 1, "r1 started on an idle host");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
    assert_eq!(h.fake.idles(), 1, "no idle call on a tainted host");
}

/// Asserts that a container the agent found at start and could not remove keeps the
/// checks from being told the host is idle: the host is tainted, and a start is refused
/// `host-tainted` with no idle call. It catches an idle test that looks at the jobs alone
/// and forgets a container no job stands for.
#[test]
fn a_container_that_could_not_be_removed_keeps_the_host_from_being_idle() {
    let mut h = Harness::new("idle-stuck");
    h.fake.leak(&ODD, 0);
    h.fake.fail("remove", 1);
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    assert_eq!(names(&h.start(job("r1"))), ["refused r1 host-tainted"]);
    assert_eq!(h.fake.idles(), 0);
}
