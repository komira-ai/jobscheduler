//! The whole-host jobs of the engine and the hosts a taint closes ([`WholeJobs::tainted`]):
//! a tainted host is never the host a job waits on, a grant there is put back, and a host
//! whose taint was cleared takes a job again. The clocks are the test's: milliseconds
//! passed by hand.
//!
//! [`WholeJobs::tainted`]: super::WholeJobs::tainted

use std::collections::BTreeSet;

use js_jobs::{JobId, JobRecord, MarkRecord};
use js_sched::OpState;
use js_types::HostId;

use super::WholeJobStep;
use crate::engine::Engine;
use crate::engine::rig::{Tables, at, candidate, engine, host, hosts, mark, run_tainted};
use crate::map;

/// The commit of `job` on the host `name`.
fn commit(job: &JobRecord, name: &str) -> WholeJobStep {
    WholeJobStep::Commit {
        job: job.id.clone(),
        host: host(name),
    }
}

/// A pass of `e` at `now_ms` over the one queued job `job` and the stored `marks`: every host
/// of `hello` said hello and is heard from, those of `open` may take a job, and those of
/// `tainted` are closed by a taint.
fn pass(
    e: &mut Engine,
    (job, marks): (&JobRecord, &[MarkRecord]),
    (hello, open, tainted): (&[&str], &[&str], &[&str]),
    now_ms: u64,
) -> Vec<WholeJobStep> {
    let sets = (hosts(hello), hosts(open), hosts(tainted));
    let queue = std::slice::from_ref(job);
    run_tainted(e, (queue, marks), (&sets.0, &sets.1, &sets.2), now_ms)
}

/// The one mark the steps write: the pass wrote nothing else.
fn only_mark(steps: &[WholeJobStep]) -> MarkRecord {
    match steps {
        [WholeJobStep::Mark(mark)] => mark.clone(),
        _ => panic!("not one mark: {steps:?}"),
    }
}

/// The pending grants of the whole-host jobs on the host `name`.
fn granted_on(e: &Engine, name: &str) -> usize {
    e.pending_on(&map::worker(&host(name))).len()
}

/// Catches komira-ai/jobscheduler#834: a job granted a quiet host a taint closes, which never
/// becomes open, waits on that grant for ever while another quiet host is open. `a` and `b`
/// are quiet, `a` is tainted: the first pass commits the job on `b`, and nothing is granted on
/// `a`.
#[test]
fn a_job_is_committed_on_the_open_host_and_not_granted_the_tainted_one() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[]), ("b", &[])]);
    let steps = pass(&mut e, (&j, &[]), (&["a", "b"], &["b"], &["a"]), 1_000);
    assert_eq!(steps, [commit(&j, "b")]);
    assert_eq!(granted_on(&e, "a"), 0);
}

/// Catches a grant made on a host before its taint that is left standing (a cordon alone keeps
/// it), and one put back as a new request (its queue time lost). `a` is quiet but not open,
/// `b` busy: `j` is granted `a` and keeps it, as a host that is only not open yet. Once `a` is
/// tainted the grant is put back on the same operation, which waits queued and marks `b` in
/// that very pass (the one other host), and when `b`'s allocation ends the job is committed on
/// `b`.
#[test]
fn a_grant_on_a_host_that_is_then_tainted_is_put_back() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[]), ("b", &[2])]);
    let kept = pass(&mut e, (&j, &[]), (&["a", "b"], &["b"], &[]), 1_000);
    assert!(kept.is_empty());
    assert_eq!(granted_on(&e, "a"), 1);
    let operation = e.whole_jobs[&j.id];
    let put_back = pass(&mut e, (&j, &[]), (&["a", "b"], &["b"], &["a"]), 2_000);
    let marked = only_mark(&put_back);
    assert_eq!(marked.host, host("b"));
    assert_eq!(granted_on(&e, "a"), 0);
    assert_eq!(e.whole_jobs[&j.id], operation);
    assert_eq!(e.sched.state(operation), Some(&OpState::Queued));
    e.job_failed(&JobId::parse("held-2").unwrap());
    let stored = [marked];
    let steps = pass(&mut e, (&j, &stored), (&["a", "b"], &["b"], &["a"]), 3_000);
    assert_eq!(steps, [commit(&j, "b")]);
}

/// Catches a job that writes on every pass while every host is tainted (a mark made, deleted
/// and made again, a grant churned), and one ended or forgotten for it. `a` and `b` are quiet
/// and both tainted: pass after pass, the first included, writes nothing, marks nothing,
/// grants nothing, and keeps the job queued on the one operation.
#[test]
fn a_job_with_every_host_tainted_stays_queued_and_writes_nothing() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[]), ("b", &[])]);
    let both = ["a", "b"];
    assert!(pass(&mut e, (&j, &[]), (&both, &[], &both), 1_000).is_empty());
    let operation = e.whole_jobs[&j.id];
    assert_eq!(e.sched.whole_mark(operation), None);
    for now_ms in [2_000, 20_000, 120_000] {
        assert!(pass(&mut e, (&j, &[]), (&both, &[], &both), now_ms).is_empty());
        assert_eq!(e.sched.state(operation), Some(&OpState::Queued));
        assert_eq!(e.sched.whole_mark(operation), None);
        assert_eq!((granted_on(&e, "a"), granted_on(&e, "b")), (0, 0));
    }
    assert_eq!(e.whole_jobs[&j.id], operation);
}

/// Catches a cordon the engine never lifts once the taint is cleared (the host would be lost to
/// whole-host jobs for good). `a` alone is tainted, then its taint is cleared and it is open:
/// the next pass commits the job on `a`.
#[test]
fn a_host_whose_taint_was_cleared_takes_a_job_again() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[])]);
    assert!(pass(&mut e, (&j, &[]), (&["a"], &[], &["a"]), 1_000).is_empty());
    assert_eq!(granted_on(&e, "a"), 0);
    let steps = pass(&mut e, (&j, &[]), (&["a"], &["a"], &[]), 2_000);
    assert_eq!(steps, [commit(&j, "a")]);
}

/// Catches a host the registry forgot and that comes back still tainted, which the engine would
/// believe still cordoned (forgetting the worker drops its cordon). `a` is tainted, forgotten
/// and back: the first pass marks the busy `b` and not `a`, the job is committed on `b` once
/// `b`'s allocation ends, and `a` is granted nothing.
#[test]
fn a_tainted_host_forgotten_and_back_is_cordoned_again() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[]), ("b", &[2])]);
    let first = pass(&mut e, (&j, &[]), (&["a", "b"], &["b"], &["a"]), 1_000);
    let marked = only_mark(&first);
    assert_eq!(marked.host, host("b"));
    let a = candidate("a", &[]);
    e.host_gone(&a.host);
    e.host_up(&a.host, &a.shape, at(1_500));
    e.job_failed(&JobId::parse("held-2").unwrap());
    let stored = [marked];
    let steps = pass(&mut e, (&j, &stored), (&["a", "b"], &["b"], &["a"]), 2_000);
    assert_eq!(steps, [commit(&j, "b")]);
    assert_eq!(granted_on(&e, "a"), 0);
}

/// Catches a job held to the tainted host while the others are busy: it would take the tainted
/// host's grant as soon as it is the one quiet host, and keep it after another frees. Four
/// hosts, `a` tainted and quiet, `b`, `c` and `d` busy: in the first pass that sees it the job
/// marks one of the busy hosts, never `a`, the next pass (given the mark) writes nothing, and
/// the job is committed there once that host's allocation ends.
#[test]
fn with_the_other_hosts_busy_a_job_marks_one_of_them_and_not_the_tainted_host() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[]), ("b", &[2]), ("c", &[3]), ("d", &[4])]);
    let (all, open) = (["a", "b", "c", "d"], ["b", "c", "d"]);
    let steps = pass(&mut e, (&j, &[]), (&all, &open, &["a"]), 1_000);
    let marked = &only_mark(&steps);
    assert_ne!(marked.host, host("a"));
    assert_eq!(granted_on(&e, "a"), 0);
    let seq = match marked.host.as_str() {
        "b" => 2,
        "c" => 3,
        _ => 4,
    };
    let stored = [marked.clone()];
    let kept = pass(&mut e, (&j, &stored), (&all, &open, &["a"]), 1_500);
    assert!(kept.is_empty(), "the mark is kept: {kept:?}");
    e.job_failed(&JobId::parse(&format!("held-{seq}")).unwrap());
    let steps = pass(&mut e, (&j, &stored), (&all, &open, &["a"]), 21_000);
    assert_eq!(steps, [commit(&j, marked.host.as_str())]);
}

/// Catches a stored mark left on a host that a taint then closes: the job would wait there for
/// a host that is never given to it. `j`'s mark on the busy `a` is stored and `a` is then
/// tainted: the pass writes the job's mark on `b`, the one other host, and not on `a`.
#[test]
fn a_mark_on_a_host_that_is_then_tainted_moves_off_it() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let stored = [mark(&j, "a", (5_000, 65_000), None)];
    let both = ["a", "b"];
    let steps = pass(&mut e, (&j, &stored), (&both, &both, &[]), 20_000);
    assert!(steps.is_empty(), "the stored mark is kept: {steps:?}");
    let steps = pass(&mut e, (&j, &stored), (&both, &["b"], &["a"]), 21_000);
    let moved: BTreeSet<&HostId> = steps
        .iter()
        .filter_map(|step| match step {
            WholeJobStep::Mark(mark) => Some(&mark.host),
            _ => None,
        })
        .collect();
    assert_eq!(moved, BTreeSet::from([&host("b")]));
}
