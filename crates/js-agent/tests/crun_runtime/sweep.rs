//! The labels, the sweep and the removal of an orphan over a real `crun`: what a runtime
//! that opens beside the leftovers of a process that died (here a runtime of the same
//! subject that is not used again) does with them. Every test is `#[ignore]`, as the
//! suite's are.

use std::fs;
use std::time::Duration;

use js_agent::crun::{Left, Reason};
use js_agent::lifecycle::{Runtime as _, label};

use crate::adopt::labelled;
use crate::runtime_suite::{Program, Subject as _, remove, spec_of, stop};
use crate::subject::{HOST, crun_subject};

const OTHER: &str = "crun-other";

/// Catches a restart that mishandles the leases of a dead process: a run that runs (booked as
/// a job start books it) is adopted and its lease stays, and a lease made and never started is
/// removed when a runtime opens on the store (container state, cgroup, directories and
/// sidecar), reported with the labels the run had and the reason that it never ran; the new
/// runtime lists the run that runs and no other, and a lease the new runtime makes is its own.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_restart_sweeps_the_leases_of_a_dead_process() {
    let mut s = crun_subject("sweep_dead");
    s.prepared();
    s.before(Program::Hold);
    let up_spec = labelled(&mut s, "r-up", Program::Hold, 600_000);
    let started = s.runtime().borrow_mut().create_and_start(&up_spec);
    let up = started.expect("create and start");
    s.after("r-up", Program::Hold);
    let spec = spec_of(&mut s, "r-made", Program::Hold);
    let made = s.unstarted(&spec);
    for id in [&up, &made] {
        assert!(s.lease_dir(&id.0).is_dir(), "{}", id.0);
        assert!(s.sidecar(&id.0).is_file(), "{}", id.0);
        assert!(s.lease_cgroup(&id.0).is_dir(), "{}", id.0);
    }

    let mut second = s.open_as(HOST);
    let swept = second.swept();
    assert_eq!(swept.adopted, ["r-up"], "{swept:?}");
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    let removed = &swept.removed[0];
    assert_eq!(
        (removed.run_id.as_str(), &removed.name),
        ("r-made", &made.0)
    );
    assert_eq!(removed.labels[label::RUN], "r-made");
    assert_eq!(removed.labels[label::AGENT], HOST);
    assert_eq!(removed.reason, Reason::NeverRan);
    assert!(
        swept.failed.is_empty() && swept.left.is_empty(),
        "{swept:?}"
    );
    assert!(!s.lease_dir(&made.0).exists(), "{}", made.0);
    assert!(!s.sidecar(&made.0).exists(), "{}", made.0);
    assert!(!s.lease_cgroup(&made.0).exists(), "{}", made.0);
    assert!(s.lease_dir(&up.0).is_dir() && s.sidecar(&up.0).is_file());
    let listed: Vec<_> = second
        .list_labelled()
        .expect("list")
        .into_iter()
        .map(|found| found.id)
        .collect();
    assert_eq!(listed, [up.clone()]);

    // The run that runs is ended through the subject's own runtime, which holds the same
    // container.
    stop(&s, &up, Duration::ZERO);
    s.settled(&up);
}

/// Catches two agents on one store taking each other's leases: the runtime of agent
/// `other` opens beside the lease of the suite's agent and removes none of it (it reports
/// the sidecar as another agent's); the suite's agent opening again removes its own lease
/// and none of the other's (its directory, cgroup and sidecar stay, and the other agent
/// can still remove it).
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn two_agents_on_one_store_never_sweep_each_others_leases() {
    let mut s = crun_subject("sweep_two");
    s.prepared();
    let spec = spec_of(&mut s, "r1", Program::Hold);
    let mine = s.unstarted(&spec);

    let mut other = s.open_as(OTHER);
    assert!(other.swept().removed.is_empty(), "{:?}", other.swept());
    assert!(
        matches!(&other.swept().left[..], [Left::Foreign { agent, .. }] if agent == HOST),
        "{:?}",
        other.swept()
    );
    assert!(s.lease_dir(&mine.0).is_dir() && s.sidecar(&mine.0).is_file());
    let spec = spec_of(&mut s, "r2", Program::Hold);
    let theirs = other.stage(&spec).expect("stage");

    let again = s.open_as(HOST);
    let swept = again.swept();
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    assert_eq!(swept.removed[0].name, mine.0);
    assert!(
        matches!(&swept.left[..], [Left::Foreign { agent, .. }] if agent == OTHER),
        "{swept:?}"
    );
    assert!(!s.lease_dir(&mine.0).exists() && !s.sidecar(&mine.0).exists());
    assert!(s.lease_dir(&theirs.0).is_dir());
    assert!(s.lease_cgroup(&theirs.0).is_dir());
    assert!(s.sidecar(&theirs.0).is_file());
    let kept = fs::read_to_string(s.sidecar(&theirs.0)).expect("read the sidecar");
    assert!(kept.contains(OTHER) && kept.contains("r2"), "{kept}");

    other.remove(&theirs).expect("remove");
    assert!(!s.lease_dir(&theirs.0).exists() && !s.sidecar(&theirs.0).exists());
}

/// Catches an orphan removal that misses its lease, removes another agent's, or fails on
/// one that is gone: a lease another process of the agent made after this runtime opened
/// is removed by its run id (everything, sidecar included), a second call finds nothing,
/// and the lease of another agent is neither removed by its run id nor by its handle (the
/// handle's removal is refused), and stays.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn an_orphan_is_removed_by_its_run_id_and_only_this_agents() {
    let mut s = crun_subject("sweep_orphan");
    s.prepared();
    let mut second = s.open_as(HOST);
    let spec = spec_of(&mut s, "r1", Program::Hold);
    let orphan = s.unstarted(&spec);
    assert!(s.lease_dir(&orphan.0).is_dir());

    assert!(second.remove_orphan("r1").expect("remove the orphan"));
    assert!(!s.lease_dir(&orphan.0).exists());
    assert!(!s.lease_cgroup(&orphan.0).exists());
    assert!(!s.sidecar(&orphan.0).exists());
    assert!(!second.remove_orphan("r1").expect("remove it again"));

    let mut other = s.open_as(OTHER);
    let spec = spec_of(&mut s, "r2", Program::Hold);
    let theirs = other.stage(&spec).expect("stage");
    assert!(!second.remove_orphan("r2").expect("another agent's run id"));
    let refused = second.remove(&theirs).expect_err("another agent's lease");
    assert!(refused.to_string().contains(OTHER), "{refused}");
    assert!(s.lease_dir(&theirs.0).is_dir() && s.sidecar(&theirs.0).is_file());
}

/// Catches a sweep that cannot take what a crash between the sidecar and the lease leaves
/// (the sidecar is written first): a sidecar with no lease, no cgroup and no container is
/// removed and reported; a lease directory with no sidecar (not made by this code) is
/// not touched.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_sidecar_without_its_lease_is_swept_and_a_lease_without_one_is_not() {
    let mut s = crun_subject("sweep_crash");
    s.prepared();
    let spec = spec_of(&mut s, "r1", Program::Hold);
    let made = s.unstarted(&spec);
    let sidecar = fs::read(s.sidecar(&made.0)).expect("read the sidecar");
    s.runtime().borrow_mut().remove(&made).expect("remove");
    assert!(!s.lease_dir(&made.0).exists() && !s.sidecar(&made.0).exists());
    fs::write(s.sidecar(&made.0), sidecar).expect("write the sidecar back");
    let stray = s.lease_dir("js-ffffffffffffffff-r9");
    fs::create_dir(&stray).expect("a lease with no sidecar");

    let second = s.open_as(HOST);
    let swept = second.swept();
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    assert_eq!(swept.removed[0].name, made.0);
    assert!(
        swept.failed.is_empty() && swept.left.is_empty(),
        "{swept:?}"
    );
    assert!(!s.sidecar(&made.0).exists());
    assert!(stray.is_dir(), "a lease with no sidecar was removed");
}

/// Catches a sweep that acts on a sidecar it cannot read as its own: garbage and a
/// sidecar under another lease's name are both left where they are, reported as malformed,
/// and do not stop the runtime from opening.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_malformed_sidecar_is_left_alone_and_reported() {
    let mut s = crun_subject("sweep_malformed");
    s.prepared();
    let spec = spec_of(&mut s, "r1", Program::Hold);
    let made = s.unstarted(&spec);
    let good = fs::read(s.sidecar(&made.0)).expect("read the sidecar");
    let junk = s.sidecar("js-0000000000000001-junk");
    let moved = s.sidecar("js-0000000000000002-moved");
    fs::write(&junk, "not json").expect("write");
    fs::write(&moved, good).expect("write");

    let second = s.open_as(HOST);
    let swept = second.swept();
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
    let paths: Vec<_> = swept
        .left
        .iter()
        .map(|left| match left {
            Left::Malformed { path, .. } => path.clone(),
            Left::Foreign { .. } => panic!("not a foreign sidecar: {left:?}"),
        })
        .collect();
    assert_eq!(paths, [junk.clone(), moved.clone()]);
    assert!(junk.is_file() && moved.is_file());
}

/// Catches a sidecar written after the lease instead of before it (a process that died
/// between the two would leave a lease nobody can attribute): with a file already where the
/// run's sidecar goes, the start is refused at the record and nothing of the lease is made
/// (no directory, no cgroup), and the file stays. A record after the lease would leave the
/// lease behind, or remove the file with it.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn the_sidecar_is_recorded_before_anything_of_the_lease_is_made() {
    let mut s = crun_subject("sweep_order");
    s.prepared();
    let spec = spec_of(&mut s, "r1", Program::Hold);
    let first = s.unstarted(&spec);
    remove(&s, &first);
    assert!(!s.lease_dir(&first.0).exists() && !s.sidecar(&first.0).exists());
    fs::write(s.sidecar(&first.0), "in the way").expect("write");

    let why = s
        .runtime()
        .borrow_mut()
        .create_and_start(&spec)
        .expect_err("refused");
    assert!(why.to_string().contains("record"), "{why}");
    assert!(!s.lease_dir(&first.0).exists(), "a lease was made first");
    assert!(
        !s.lease_cgroup(&first.0).exists(),
        "a cgroup was made first"
    );
    assert_eq!(
        fs::read_to_string(s.sidecar(&first.0)).unwrap(),
        "in the way"
    );
}
