//! The whole-host jobs of the engine, driven as the placer's pass drives them: the queued
//! jobs and stored marks of the tables handed over at each pass, and the writes the engine
//! answers. The clocks are the test's: milliseconds passed by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_jobs::{Exclusion, JobRecord, MarkRecord};
use js_registry::{MarkedTails, Standing, WholeHost, WholeNodeConfig, WholeRound};
use js_sched::{Bar, Event, OpState, WholeState};
use js_state::StoreTime;
use js_types::LeaseId;
use js_types::sched::{LeaseKind, WorkKey};

use super::{PrepareCause, WholeJobStep, WholeJobs, backfill_wall, failed_prepare};
use crate::engine::rig::{
    Tables, at, beat, engine, every, host, hosts, mark, run, run_with, settings, store,
};
use crate::engine::whole::WHOLE_TERM;
use crate::engine::{Engine, Settings};
use crate::map;

/// Both hosts busy with a lease of the registry.
const BUSY: [(&str, &[u64]); 2] = [("a", &[1]), ("b", &[2])];

/// A pass of `e` at `now_ms` with the pool settings given (`None`: the pool has none).
fn pass(
    e: &mut Engine,
    (queued, marks): (&[JobRecord], &[MarkRecord]),
    settings: Option<&WholeNodeConfig>,
    now_ms: u64,
) -> Vec<WholeJobStep> {
    beat(e, &["a", "b"], now_ms);
    let both = hosts(&["a", "b"]);
    let input = WholeJobs {
        queued,
        marks,
        hello: &both,
        open: &both,
        settings,
        claim_by: at(15_000),
    };
    e.whole_jobs(&input, at(now_ms))
}

/// The mark `step` writes.
fn written(step: &WholeJobStep) -> &MarkRecord {
    match step {
        WholeJobStep::Mark(mark) => mark,
        other => panic!("not a mark: {other:?}"),
    }
}

/// The deletion of the mark of `job`.
fn unmark(job: &JobRecord) -> WholeJobStep {
    WholeJobStep::Unmark(job.id.clone())
}

/// A registry round with nothing waiting, over the hosts `a` and `b`.
fn empty_round() -> WholeRound {
    let stand = |name: &str| WholeHost {
        id: host(name),
        standing: Standing::Live,
        open: true,
    };
    WholeRound {
        settings: settings(),
        waiting: Vec::new(),
        hosts: vec![stand("a"), stand("b")],
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    }
}

/// Catches: the pool's settings taken from a pass when the registry's own round set them
/// (the two would swap the scheduler's settings back and forth), settings swapped in again
/// when they have not changed, a pool with none given a queue, and a stored mark left behind
/// for a job that is not queued because the pool has no settings. With none, the pass
/// deletes the stray mark and queues nothing; with settings they are taken once and again
/// only when they differ; after a registry round they are not taken.
#[test]
fn the_pool_settings_are_taken_without_a_round_and_only_when_they_differ() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let gone = tables.whole("gone");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    let stray = [mark(&gone, "a", (5_000, 65_000), None)];
    let steps = pass(&mut e, (&queue, &stray), None, 20_000);
    assert_eq!(steps, [unmark(&gone)]);
    assert_eq!(e.whole_settings, None);
    assert!(e.whole_jobs.is_empty());
    assert_eq!(e.sched.queued_len(), 0);
    let first = settings();
    pass(&mut e, (&queue, &[]), Some(&first), 20_000);
    assert_eq!(e.whole_settings, Some(first.clone()));
    assert_eq!(e.whole_jobs.len(), 1);
    pass(&mut e, (&queue, &[]), Some(&first), 21_000);
    assert_eq!(e.whole_settings, Some(first.clone()));
    let second = WholeNodeConfig {
        max_replace: 5,
        ..first.clone()
    };
    pass(&mut e, (&queue, &[]), Some(&second), 22_000);
    assert_eq!(e.whole_settings, Some(second.clone()));
    e.round = Some(empty_round());
    pass(&mut e, (&queue, &[]), Some(&first), 23_000);
    assert_eq!(e.whole_settings, Some(second));
}

/// Catches: a stored mark of a job that is not a queued whole-host job left in the table (a
/// cancelled, placed or ended job's, or a job that does not need the whole host), the mark
/// of a queued whole-host job deleted with them, and a mark rewritten because its times
/// differ below the millisecond the scheduler counts in. `gone` is not queued and `plain`
/// needs a share: both marks are deleted, in the order of the table; `j`'s mark, made half a
/// millisecond after its millisecond, is restored and written no more.
#[test]
fn a_stored_mark_of_a_job_that_is_not_a_queued_whole_host_job_is_deleted() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let plain = tables.share("plain");
    let gone = tables.whole("gone");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone(), plain.clone()];
    let mut held = mark(&j, "a", (5_000, 65_000), None);
    held.made = StoreTime::from_duration(Duration::from_micros(5_000_500));
    let marks = [
        mark(&gone, "b", (5_000, 65_000), None),
        held,
        mark(&plain, "b", (5_000, 65_000), None),
    ];
    let steps = run(&mut e, (&queue, &marks), 20_000);
    assert_eq!(steps, [unmark(&gone), unmark(&plain)]);
    assert_eq!(e.whole_jobs.keys().collect::<Vec<_>>(), [&j.id]);
    let kept = e.sched.whole_mark(e.whole_jobs[&j.id]).unwrap();
    assert_eq!(kept.worker, map::worker(&host("a")));
    assert_eq!(kept.made, at(5_000));
    assert_eq!(kept.deadline, at(65_000));
}

/// Catches: a queued whole-host job queued as another kind of request, with another name,
/// from the time the pass found it instead of the time it was submitted (it would wait out
/// `drain_after` afresh at every restart), without its wall limit, or queued again at each
/// pass. `j`, submitted at 3 s, is a whole-machine request named for the job, may first hold
/// a host at 13 s, may run ten minutes, and has one operation however many passes see it.
#[test]
fn a_queued_whole_host_job_is_queued_once_from_its_submit_time() {
    let tables = Tables::new();
    tables.set_ms(3_000);
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    assert!(run(&mut e, (&queue, &[]), 5_000).is_empty());
    let operation = e.whole_jobs[&j.id];
    let request = e.sched.request(operation).unwrap();
    assert_eq!(request.kind, LeaseKind::WholeMachine);
    assert_eq!(request.key, WorkKey::new("job", "j"));
    let early = WholeState::Early {
        may_hold_at: at(13_000),
    };
    assert_eq!(e.sched.whole_state(operation), Some(early));
    let wall = Duration::from_secs(600);
    assert_eq!(e.sched.max_wall(operation), Some(wall));
    assert!(run(&mut e, (&queue, &[]), 6_000).is_empty());
    assert_eq!(e.whole_jobs[&j.id], operation);
    assert_eq!(e.sched.queued_len(), 1);
}

/// Catches: a mark made and never written, a written mark that is written again at every
/// pass, and a mark given up or moved at its deadline (a write of a mark on another host,
/// or a back-off on `a`). `j` marks `a` at 20 s with a deadline a minute on and the mark is
/// written once; the next pass writes nothing, and neither does a pass at the deadline or
/// long past it.
#[test]
fn a_mark_is_written_when_made_and_not_rewritten_at_or_past_its_deadline() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    let steps = run(&mut e, (&queue, &[]), 20_000);
    assert_eq!(steps.len(), 1);
    let first = written(&steps[0]).clone();
    assert_eq!(first.job, j.id);
    assert_eq!(first.host, host("a"));
    assert_eq!(first.made, store(20_000));
    assert_eq!(first.deadline, store(80_000));
    assert_eq!(first.tail_end, None);
    assert!(first.exclusions.is_empty());
    let stored = [first];
    assert!(run(&mut e, (&queue, &stored), 25_000).is_empty());
    assert!(run(&mut e, (&queue, &stored), 80_000).is_empty());
    assert!(run(&mut e, (&queue, &stored), 400_000).is_empty());
}

/// Catches: a stored mark taken back on a host that never said hello, a deadline or tail
/// changed on the way back in (the store raised the deadline at boot; adding a grace would
/// stretch a drain at every restart loop), and the mark lost. `j`'s stored mark on `a`, made
/// at 5 s with a tail ending at 90 s, is restored exactly as stored when `a` said hello, and
/// writes nothing.
#[test]
fn a_stored_mark_whose_host_said_hello_is_restored_as_it_is_stored() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    let stored = [mark(&j, "a", (5_000, 65_000), Some(90_000))];
    assert!(run(&mut e, (&queue, &stored), 20_000).is_empty());
    let kept = e.sched.whole_mark(e.whole_jobs[&j.id]).unwrap();
    assert_eq!(kept.worker, map::worker(&host("a")));
    assert_eq!(kept.made, at(5_000));
    assert_eq!(kept.deadline, at(65_000));
    assert_eq!(kept.tail_end, Some(at(90_000)));
}

/// Catches: a stored mark dropped before `claim_by` while its host may still say hello (a
/// restart would lose every mark of an agent that is a few seconds late), a mark kept past
/// it (a host that never comes back would hold the job's place for ever), and a job whose
/// mark is dropped that is not marked again. With no hello, nothing is decided for `j` at
/// 14 s and its stored mark stands; at 15 s the mark is deleted and the job, queued
/// without it, is marked afresh.
#[test]
fn a_stored_mark_whose_host_never_says_hello_is_dropped_at_claim_by() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    let stored = [mark(&j, "a", (5_000, 65_000), None)];
    let none = hosts(&[]);
    let open = hosts(&["a", "b"]);
    let early = run_with(&mut e, (&queue, &stored), (&none, &open), 14_000);
    assert!(early.is_empty());
    assert!(e.whole_jobs.is_empty());
    let steps = run_with(&mut e, (&queue, &stored), (&none, &open), 15_000);
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0], unmark(&j));
    let again = written(&steps[1]);
    assert_eq!(again.host, host("a"));
    assert_eq!(again.made, store(15_000));
    assert_eq!(e.whole_jobs.len(), 1);
}

/// Catches: a stored mark on a host the engine cannot hold left standing under a job that
/// holds no mark (the table would show a host held for it that holds nothing), or a job
/// that is left unmarked for good. With the mark on `z`, which said hello and is no worker:
/// a job that may already hold a host is marked on one the engine knows, and the stored mark
/// is replaced; a job that must still wait has the stored mark deleted.
#[test]
fn a_stored_mark_the_scheduler_cannot_hold_is_replaced_or_deleted() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    let stored = [mark(&j, "z", (5_000, 65_000), None)];
    let known = hosts(&["a", "b", "z"]);
    let open = hosts(&["a", "b"]);
    let steps = run_with(&mut e, (&queue, &stored), (&known, &open), 20_000);
    assert_eq!(steps.len(), 1);
    assert_eq!(written(&steps[0]).host, host("a"));
    let later = Tables::new();
    later.set_ms(15_000);
    let waiting = later.whole("w");
    let queue = [waiting.clone()];
    let stored = [mark(&waiting, "z", (5_000, 65_000), None)];
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let steps = run_with(&mut e, (&queue, &stored), (&known, &open), 20_000);
    assert_eq!(steps, [unmark(&waiting)]);
}

/// Catches: the operation of a job that was cancelled, placed elsewhere or ended left in
/// the scheduler (it would hold a mark and block a host for a job that is gone), and a job
/// whose operation the scheduler dropped never queued again. `j` leaves the queue and its
/// operation goes; a second job whose operation was cancelled in the scheduler is queued
/// again under a new one.
#[test]
fn the_operation_of_a_job_that_left_the_queue_or_was_dropped_is_replaced() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let k = tables.whole("k");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    run(&mut e, (&[j.clone(), k.clone()], &[]), 5_000);
    let operation = e.whole_jobs[&j.id];
    let other = e.whole_jobs[&k.id];
    run(&mut e, (std::slice::from_ref(&k), &[]), 6_000);
    assert_eq!(e.whole_jobs.keys().collect::<Vec<_>>(), [&k.id]);
    assert_eq!(e.sched.state(operation), None);
    let _ = e.sched.cancel_whole(other);
    run(&mut e, (std::slice::from_ref(&k), &[]), 7_000);
    assert_ne!(e.whole_jobs[&k.id], other);
    assert_eq!(e.sched.queued_len(), 1);
}

/// Catches: a job granted a quiet host that is not committed (it would hold the host for
/// ever), one committed on a host that may not take a job now, a grant dropped because its
/// host is not open, and a grant numbered in another term. `b` is quiet: `j` is granted it
/// in the whole-node term; with only `a` open nothing is committed and the grant stays;
/// with `b` open the commit is the one step.
#[test]
fn a_job_granted_a_quiet_host_is_committed_when_the_host_is_open() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    let queue = [j.clone()];
    let both = hosts(&["a", "b"]);
    let only_a = hosts(&["a"]);
    let waiting = run_with(&mut e, (&queue, &[]), (&both, &only_a), 1_000);
    assert!(waiting.is_empty());
    let b = map::worker(&host("b"));
    let pending = e.pending_on(&b);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].term, WHOLE_TERM);
    let commit = WholeJobStep::Commit {
        job: j.id.clone(),
        host: host("b"),
    };
    assert_eq!(run(&mut e, (&queue, &[]), 2_000), [commit]);
    assert_eq!(e.pending_on(&b), pending);
}

/// Catches: a commit that landed and left the job queued in the engine (it would be granted
/// a second host), a hold the heartbeats do not list, a commit that failed and left its
/// grant standing (the host would be held for a job that is not placed), and a grant that
/// is not made again once put back. After the commit lands, the engine holds the allocation
/// on `b` with its ports, the job leaves the queue and a heartbeat lists the lease; after a
/// failure the grant is put back, the job stays queued, and the next pass commits it again.
/// A job the engine does not queue changes nothing either way.
#[test]
fn a_commit_that_landed_holds_the_host_and_one_that_failed_puts_the_grant_back() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    let queue = [j.clone()];
    let b = host("b");
    run(&mut e, (&queue, &[]), 1_000);
    e.whole_job_failed(&j.id);
    assert!(e.pending_on(&map::worker(&b)).is_empty());
    assert_eq!(e.whole_jobs.len(), 1);
    e.whole_job_failed(&j.id);
    let commit = WholeJobStep::Commit {
        job: j.id.clone(),
        host: b.clone(),
    };
    assert_eq!(run(&mut e, (&queue, &[]), 2_000), [commit]);
    e.whole_job_placed(&j.id, &b, vec![8080]);
    assert!(e.whole_jobs.is_empty());
    assert!(e.holds_jobs_on(&b));
    assert_eq!(e.job_leases_on(&b).count(), 1);
    assert_eq!(e.sched.leases_on(&map::worker(&b)).len(), 1);
    e.whole_job_placed(&j.id, &b, vec![8080]);
    e.whole_job_failed(&j.id);
    assert_eq!(e.job_leases_on(&b).count(), 1);
}

/// Catches: a job whose commit failed while it had no grant (a marked job) disturbing its
/// mark, and a commit that landed after the engine lost the grant (its host forgotten
/// between the pass and the commit) holding a lease that is not there. A marked `j` keeps
/// its mark through `whole_job_failed`; a job whose host was forgotten leaves the queue
/// with its operation cancelled and holds nothing, for the next pass to take the allocation
/// up from the table.
#[test]
fn a_job_without_a_grant_is_left_alone_or_leaves_the_queue_with_no_hold() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut marked = engine(&[("a", &[1]), ("b", &[2])]);
    run(&mut marked, (std::slice::from_ref(&j), &[]), 20_000);
    let operation = marked.whole_jobs[&j.id];
    marked.whole_job_failed(&j.id);
    assert!(marked.sched.whole_mark(operation).is_some());
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    run(&mut e, (std::slice::from_ref(&j), &[]), 1_000);
    let operation = e.whole_jobs[&j.id];
    e.host_gone(&host("b"));
    e.whole_job_placed(&j.id, &host("b"), Vec::new());
    assert!(e.whole_jobs.is_empty());
    assert!(!e.holds_jobs_on(&host("b")));
    assert_eq!(e.sched.state(operation), None);
}

/// Catches: the bars of a job's failed prepares dropped when it is queued again (it would
/// be granted the host that failed), those of its stored mark dropped, and the weaker of two
/// bars on one host kept. `j` is kept off `a` by a hard bar until 100 s, and its stored mark
/// on `b` records `a` for life: the scheduler bars `a` for life, and since `a` is barred the
/// pass writes nothing new for a mark that already records it.
#[test]
fn the_bars_of_a_job_and_of_its_mark_are_merged_by_strength() {
    let tables = Tables::new();
    let mut j = tables.whole("j");
    let until = Exclusion::Until(store(100_000));
    j.prepare_bars.insert(host("a"), until);
    let mut held = mark(&j, "b", (5_000, 65_000), None);
    held.exclusions.insert(host("a"), Exclusion::ForLife);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    assert!(run(&mut e, (&[j.clone()], &[held]), 20_000).is_empty());
    let barred = e.sched.whole_barred(e.whole_jobs[&j.id], at(20_000));
    assert_eq!(
        barred,
        BTreeMap::from([(map::worker(&host("a")), Bar::ForLife)])
    );
}

/// Catches: a job's failed prepares not kept off its host when it is queued again (its next
/// mark would be on the host that failed). `j` was barred from `a` until 100 s: it marks
/// `b`, and the mark written names `a` as excluded until then.
#[test]
fn a_job_is_marked_on_another_host_than_the_one_its_prepare_failed_on() {
    let tables = Tables::new();
    let mut j = tables.whole("j");
    let until = Exclusion::Until(store(100_000));
    j.prepare_bars.insert(host("a"), until);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let steps = run(&mut e, (&[j], &[]), 20_000);
    assert_eq!(steps.len(), 1);
    let marked = written(&steps[0]);
    assert_eq!(marked.host, host("b"));
    assert_eq!(marked.exclusions, BTreeMap::from([(host("a"), until)]));
}

/// Catches: a back-off a job's row carries (one an older build wrote when a hold could be
/// given up) dropped, or written back as a hard exclusion: `j` is backed off `a` until
/// 100 s, so it marks `b`, and the mark written keeps `a` as a back-off until then.
#[test]
fn a_back_off_on_a_jobs_row_is_kept_soft_in_the_mark_written() {
    let tables = Tables::new();
    let mut j = tables.whole("j");
    let backoff = Exclusion::Backoff(store(100_000));
    j.prepare_bars.insert(host("a"), backoff);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let steps = run(&mut e, (&[j], &[]), 20_000);
    assert_eq!(steps.len(), 1);
    let marked = written(&steps[0]);
    assert_eq!(marked.host, host("b"));
    assert_eq!(marked.exclusions, BTreeMap::from([(host("a"), backoff)]));
}

/// Catches: a grant a round of another term made (a start or a job placed in it) kept for
/// the job, numbered as the next lease. `j` marks `a`; `a`'s lease ends and a plain tick
/// grants `a` to `j` in the registry's term: it is put back at once, and the next pass
/// commits it in the whole-node term.
#[test]
fn a_grant_outside_the_whole_round_is_put_back_for_a_job_too() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let queue = [j.clone()];
    run(&mut e, (&queue, &[]), 20_000);
    let operation = e.whole_jobs[&j.id];
    e.ended(LeaseId::new(1, 1));
    e.feed(at(21_000), Event::Tick);
    assert_eq!(e.sched.state(operation), Some(&OpState::Queued));
    let steps = run(&mut e, (&queue, &[]), 22_000);
    let commit = WholeJobStep::Commit {
        job: j.id.clone(),
        host: host("a"),
    };
    assert_eq!(steps, [commit]);
    let a = map::worker(&host("a"));
    assert_eq!(e.pending_on(&a)[0].term, WHOLE_TERM);
}

/// Catches: the registry's round standing the engine down while a whole-host job is queued
/// (the cordons and phases it set would go, and the round would be forgotten), the job's mark
/// shown to the registry as one of its requests' reservations, and a round that never stands
/// down once the last job has left. With `j` marked, a round with nothing waiting is taken
/// and answers no reservation; once `j` has left the queue the next round stands down.
#[test]
fn the_registrys_round_stands_down_only_when_no_job_is_queued_either() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    run(&mut e, (std::slice::from_ref(&j), &[]), 20_000);
    assert!(e.whole_round(empty_round(), at(21_000)).is_empty());
    assert!(e.round.is_some());
    run(&mut e, (&[], &[]), 22_000);
    assert!(e.whole_round(empty_round(), at(23_000)).is_empty());
    assert!(e.round.is_none());
}

/// Catches: the queue of whole-host jobs carried over a rebuild (it is a cache of the table,
/// and a copy would hold a mark for a job the table has since changed), or a rebuilt
/// engine that does not queue the jobs again. After a rebuild nothing is queued and the pool
/// has no settings; the next pass queues `j` again and takes the settings.
#[test]
fn a_rebuild_does_not_carry_the_whole_host_jobs() {
    let tables = Tables::new();
    let j = tables.whole("j");
    let mut e = engine(&BUSY);
    run(&mut e, (std::slice::from_ref(&j), &[]), 20_000);
    let mut rebuilt = e.rebuild(Settings::default(), &every(&BUSY), at(21_000));
    assert!(rebuilt.whole_jobs.is_empty());
    assert_eq!(rebuilt.whole_settings, None);
    assert_eq!(rebuilt.sched.queued_len(), 0);
    run(&mut rebuilt, (std::slice::from_ref(&j), &[]), 22_000);
    assert_eq!(rebuilt.whole_jobs.len(), 1);
    assert_eq!(rebuilt.whole_settings, Some(settings()));
}

/// The failed prepare of `job` on `a` at 1000 s, for `cause`, under the test's settings.
fn fail(job: &JobRecord, cause: PrepareCause) -> (js_jobs::FailedPrepare, bool) {
    failed_prepare(job, &host("a"), cause, &settings(), store(1_000_000))
}

/// Catches: a failed prepare not counted, a cause that excludes the host for the wrong time
/// (the rows of the registry's table: a refusal that is not-quiet, noise-over-bound or
/// setup-failed, and a timeout, for `reserve_backoff`; unsupported for life), a host-unsafe
/// refusal that bars the host instead of tainting it, and a taint not reported. The back-off
/// ends 300 s after the failure.
#[test]
fn a_failed_prepare_counts_and_excludes_the_host_as_its_cause_says() {
    let tables = Tables::new();
    let job = tables.whole("j");
    let backoff = BTreeMap::from([(host("a"), Exclusion::Until(store(1_300_000)))]);
    let backed_off = [
        PrepareCause::Refused(RefusalReason::NotQuiet),
        PrepareCause::Refused(RefusalReason::NoiseOverBound),
        PrepareCause::Refused(RefusalReason::SetupFailed),
        PrepareCause::Timeout,
    ];
    for cause in backed_off {
        let (failed, taints) = fail(&job, cause);
        assert_eq!(failed.failed_prepares, 1, "{cause:?}");
        assert_eq!(failed.prepare_bars, backoff, "{cause:?}");
        assert!(!failed.terminal & !taints, "{cause:?}");
    }
    let unsupported = PrepareCause::Refused(RefusalReason::Unsupported);
    let (failed, taints) = fail(&job, unsupported);
    let for_life = BTreeMap::from([(host("a"), Exclusion::ForLife)]);
    assert_eq!(failed.prepare_bars, for_life);
    assert!(!taints);
    let unsafe_host = PrepareCause::Refused(RefusalReason::HostUnsafe);
    let (failed, taints) = fail(&job, unsafe_host);
    assert_eq!(failed.failed_prepares, 1);
    assert!(failed.prepare_bars.is_empty());
    assert!(taints & !failed.terminal);
}

/// Catches: the count restarted instead of added to, a job that never becomes terminal or
/// does so a failure early or late, and the bars of earlier failures lost or the weaker of two
/// bars on a host kept. With two failures behind it the next ends the job (the count reaches
/// `max_replace`, 3); a bar for life and a later bar are kept over the back-off, a back-off
/// is replaced by the hard bar, and the bars on other hosts stay.
#[test]
fn a_failed_prepare_adds_to_the_count_and_keeps_the_stronger_bar() {
    let tables = Tables::new();
    let mut job = tables.whole("j");
    let timeout = PrepareCause::Timeout;
    job.failed_prepares = 2;
    let (last, _) = fail(&job, timeout);
    assert_eq!(last.failed_prepares, 3);
    assert!(last.terminal);
    job.failed_prepares = 0;
    let one = WholeNodeConfig {
        max_replace: 1,
        ..settings()
    };
    let (only, _) = failed_prepare(&job, &host("a"), timeout, &one, store(1_000_000));
    assert!(only.terminal);
    let backoff = Exclusion::Until(store(1_300_000));
    let later = Exclusion::Until(store(2_000_000));
    let sooner = Exclusion::Until(store(1_000_001));
    let cases = [
        (Exclusion::ForLife, Exclusion::ForLife),
        (later, later),
        (sooner, backoff),
        (Exclusion::Backoff(store(5_000_000)), backoff),
    ];
    for (earlier, kept) in cases {
        job.prepare_bars.clear();
        job.prepare_bars.insert(host("a"), earlier);
        job.prepare_bars.insert(host("other"), Exclusion::ForLife);
        let (failed, _) = fail(&job, timeout);
        let mut expected = BTreeMap::new();
        expected.insert(host("a"), kept);
        expected.insert(host("other"), Exclusion::ForLife);
        assert_eq!(failed.prepare_bars, expected, "{earlier:?}");
    }
}

/// Catches: a backfilled job's wall limit not cut to the time left in the tail less the
/// margin, cut when it is already shorter (it would be lengthened), and a limit that goes
/// below zero when the tail is nearly spent. A tail with 200 s left less the 60 s margin
/// leaves 140 s: a ten-minute request is cut to it, a 100 s request is kept; with a minute
/// or less left, or the tail past, the limit is zero.
#[test]
fn a_backfilled_wall_is_cut_to_the_tail_less_the_margin_and_never_lengthened() {
    let secs = Duration::from_secs;
    let tail = at(200_000);
    assert_eq!(backfill_wall(secs(600), tail, at(0)), secs(140));
    assert_eq!(backfill_wall(secs(100), tail, at(0)), secs(100));
    assert_eq!(backfill_wall(secs(600), at(60_000), at(0)), secs(0));
    assert_eq!(backfill_wall(secs(600), at(30_000), at(0)), secs(0));
    assert_eq!(backfill_wall(secs(600), tail, at(300_000)), secs(0));
}
