//! The reservation records ([`Scheduler::recording_reservations`]): one record per large
//! request when it leaves the queue, and one release per lease, with what placement
//! expected of it. Every time is farm time fed to the scheduler; each test takes the
//! events and compares them whole.

use std::time::Duration;

use crate::digest::{Digest, DigestFunction};
use crate::whole_rig::{CAPACITY, Farm, request, secs, w};
use js_caps::NodeCaps;
use js_sched::{
    Backfill, Cancelled, Event, LeaseRelease, ReservationEnd, ReservationEvent, ReservationRecord,
    Scheduler,
};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseKind, OperationId, Outcome, ResultRecord,
};
use js_whole::WholeNodeConfig;

/// A ceiling no wall time in these tests reaches.
const DAY: Duration = Duration::from_secs(86_400);

fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// A scheduler with whole-node settings, placing by the booked rule, with bounds enforced
/// up to a day, recording reservations.
fn recording() -> Farm {
    Farm::new(
        Scheduler::new(1)
            .with_load_config(crate::booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(4))
            .with_backfill(Backfill::enforced(DAY))
            .recording_reservations(),
    )
}

/// Worker `a` with one lease that may run 600 s, from 0, and a whole-machine request that
/// holds `a` from 0, its start expected at 600 s. The lease and the request; the events so
/// far are taken.
fn marked(f: &mut Farm) -> (LeaseGrant, OperationId) {
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(f.s.take_reservations(), []);
    (lease, whole)
}

/// Submits an action that may run `wall` at the farm time now and runs a round: its
/// grant, started, if it was placed.
fn job(f: &mut Farm, wall: Duration) -> Option<LeaseGrant> {
    f.next_n += 1;
    let operation = f.s.next_operation();
    f.s.set_max_wall(operation, wall);
    f.submit(request(f.next_n, LeaseKind::Action));
    let grant = f.grants().into_iter().find(|g| g.operation == operation);
    if let Some(grant) = &grant {
        f.start(grant);
    }
    grant
}

/// The release of `grant`, granted at `granted` and ended at `released`.
fn released(
    grant: &LeaseGrant,
    granted: u64,
    released: u64,
    estimate: Option<Duration>,
    backfilled: bool,
) -> ReservationEvent {
    ReservationEvent::Released(LeaseRelease {
        lease: grant.lease,
        operation: grant.operation,
        worker: grant.worker.clone(),
        granted_at: at(granted),
        released_at: at(released),
        estimate,
        backfilled,
    })
}

/// The record of the request [`marked`] holds on `a`: blocked and reserved at 0 with its
/// start expected at 600 s, never moved, ended at `ended` as `end` says.
fn marked_record(
    whole: OperationId,
    backfilled_core_seconds: u64,
    ended: u64,
    end: ReservationEnd,
) -> ReservationEvent {
    ReservationEvent::Ended(ReservationRecord {
        operation: whole,
        blocked_at: at(0),
        reserved_at: Some(at(0)),
        estimated_start: Some(at(600_000)),
        last_estimate: Some(at(600_000)),
        moves: 0,
        backfilled_core_seconds,
        ended_at: at(ended),
        end,
    })
}

/// Catches: a start that emits no record, emits one per placement round the request waits
/// in, or one with the wrong times. The request waits on `a` from 0, nothing is emitted
/// while it waits, and when the lease it waits for ends at 600 s it starts on `a`: the
/// lease's release and the request's one record, its actual start 600 s. Its own lease's
/// release, at 700 s, says it was no backfill.
#[test]
fn a_start_emits_one_record_with_its_estimates_and_its_actual_start() {
    let mut f = recording();
    let (lease, whole) = marked(&mut f);
    assert!(f.advance(599_000).is_empty());
    assert_eq!(f.s.take_reservations(), []);
    f.at(600_000);
    f.finish(&lease);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, whole);
    let started = ReservationEnd::Started { worker: w("a") };
    assert_eq!(
        f.s.take_reservations(),
        [
            released(&lease, 0, 600_000, Some(secs(600)), false),
            marked_record(whole, 0, 600_000, started),
        ]
    );
    assert_eq!(f.s.take_reservations(), []);
    f.start(&grants[0]);
    assert!(f.advance(700_000).is_empty());
    f.finish(&grants[0]);
    assert_eq!(
        f.s.take_reservations(),
        [released(&grants[0], 600_000, 700_000, None, false)]
    );
}

/// Catches: a cancel that emits no record, or a second cancel that emits another. The
/// request held on `a` is cancelled at 10 s: one record, ended at 10 s as cancelled; the
/// second cancel does nothing and emits nothing.
#[test]
fn a_cancel_emits_one_record_and_a_second_cancel_none() {
    let mut f = recording();
    let (_, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    assert!(matches!(f.s.cancel_whole(whole), Cancelled::Queued { .. }));
    assert_eq!(
        f.s.take_reservations(),
        [marked_record(whole, 0, 10_000, ReservationEnd::Cancelled)]
    );
    assert_eq!(f.s.cancel_whole(whole), Cancelled::Nothing);
    assert_eq!(f.s.take_reservations(), []);
}

/// Catches: a move that emits a record, is not counted, or overwrites the first estimate.
/// `a` carries a lease of 600 s and `b` one of 900 s, so the request holds `a`, its start
/// expected at 600 s. `a` is cordoned at 10 s and the hold moves to `b`, 900 s: nothing is
/// emitted. When it is cancelled at 20 s its record counts one move, keeps the 600 s it
/// was reserved with, and gives 900 s as the last estimate. Rounds on `b` after the move
/// count no more moves.
#[test]
fn a_move_counts_one_move_and_emits_no_record() {
    let mut f = recording();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(600));
    f.lease_on("b", secs(900));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.feed(Event::Cordon { worker: w("a") });
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
    assert_eq!(f.s.take_reservations(), []);
    f.at(20_000);
    f.beat();
    assert!(f.grants().is_empty());
    f.s.cancel_whole(whole);
    let record = ReservationRecord {
        operation: whole,
        blocked_at: at(0),
        reserved_at: Some(at(0)),
        estimated_start: Some(at(600_000)),
        last_estimate: Some(at(900_000)),
        moves: 1,
        backfilled_core_seconds: 0,
        ended_at: at(20_000),
        end: ReservationEnd::Cancelled,
    };
    assert_eq!(f.s.take_reservations(), [ReservationEvent::Ended(record)]);
}

/// Catches: a backfilled lease whose release does not say so, an ordinary one that says
/// it was, or backfill not counted to the request it went around. At 10 s a job of 300 s,
/// a core, is placed on `a` as backfill and ends at 310 s: its release says backfilled,
/// with its estimate of 300 s. The lease the request waits for, placed before any hold,
/// ends at 600 s: its release says it was not. The request then starts, its record
/// counting the job's 300 core-seconds.
#[test]
fn a_backfilled_lease_says_so_on_release_and_an_ordinary_one_does_not() {
    let mut f = recording();
    let (lease, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    let backfill = job(&mut f, secs(300)).expect("the job backfills on a");
    assert_eq!(backfill.worker, w("a"));
    assert!(f.advance(310_000).is_empty());
    f.finish(&backfill);
    assert_eq!(
        f.s.take_reservations(),
        [released(&backfill, 10_000, 310_000, Some(secs(300)), true)]
    );
    assert!(f.advance(599_000).is_empty());
    f.at(600_000);
    f.finish(&lease);
    assert_eq!(f.grants().len(), 1);
    let started = ReservationEnd::Started { worker: w("a") };
    assert_eq!(
        f.s.take_reservations(),
        [
            released(&lease, 0, 600_000, Some(secs(600)), false),
            marked_record(whole, 300, 600_000, started),
        ]
    );
}

/// Catches: backfill still running when the request leaves the queue left out of its
/// record, or counted again when it ends. The job of 300 s placed on `a` at 10 s still runs
/// when the request is cancelled at 110 s: its record counts 100 core-seconds. The job's
/// release at 310 s says backfilled and adds to no record.
#[test]
fn backfill_still_running_counts_up_to_the_record_and_not_again() {
    let mut f = recording();
    let (_, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    let backfill = job(&mut f, secs(300)).expect("the job backfills on a");
    assert!(f.advance(110_000).is_empty());
    f.s.cancel_whole(whole);
    let record = marked_record(whole, 100, 110_000, ReservationEnd::Cancelled);
    assert_eq!(f.s.take_reservations(), [record]);
    assert!(f.advance(310_000).is_empty());
    f.finish(&backfill);
    assert_eq!(
        f.s.take_reservations(),
        [released(&backfill, 10_000, 310_000, Some(secs(300)), true)]
    );
}

/// Catches: a request that fits at once taken for a large one. A whole-machine request on
/// an empty worker is placed in its first round and has no record; only its lease's release
/// is emitted.
#[test]
fn a_request_that_fits_at_once_has_no_record() {
    let mut f = recording();
    f.worker("a");
    let whole = f.submit_whole(None);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, whole);
    f.start(&grants[0]);
    assert_eq!(f.s.take_reservations(), []);
    f.at(5_000);
    f.finish(&grants[0]);
    assert_eq!(
        f.s.take_reservations(),
        [released(&grants[0], 0, 5_000, None, false)]
    );
}

/// Catches: a request another scheduler took left tracked, or recorded as a start or a
/// cancel. The request held on `a` is forgotten as taken elsewhere at 10 s: one record
/// that says so.
#[test]
fn a_request_taken_elsewhere_emits_one_record_that_says_so() {
    let mut f = recording();
    let (_, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    assert!(f.s.forget_queued(whole).is_some());
    let record = marked_record(whole, 0, 10_000, ReservationEnd::TakenElsewhere);
    assert_eq!(f.s.take_reservations(), [record]);
}

/// Catches: a refused request left tracked, or recorded at its proposal rather than its
/// commit. The request held on `a` cannot be served once `a` drops the whole-machine driver
/// at 10 s; the unservable wait of 30 s passes at 40 s, its refusal is proposed and, once
/// committed, emits one record that says refused.
#[test]
fn a_refused_request_emits_one_record_when_its_refusal_commits() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(crate::booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(4))
            .with_backfill(Backfill::enforced(DAY))
            .with_unservable_wait(secs(30))
            .recording_reservations(),
    );
    let (_, whole) = marked(&mut f);
    f.at(10_000);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"]);
    f.feed(Event::Capacity {
        worker: w("a"),
        capacity: CAPACITY,
        caps,
    });
    f.beat();
    assert!(f.grants().is_empty());
    f.at(40_000);
    f.beat();
    let refusal = f
        .feed(Event::Tick)
        .into_iter()
        .find_map(|e| match e {
            Effect::Commit(ControlRecord::Refusal(r)) if r.operation == whole => Some(r),
            _ => None,
        })
        .expect("the request is refused");
    assert_eq!(f.s.take_reservations(), []);
    f.feed(Event::Committed(ControlRecord::Refusal(refusal)));
    assert_eq!(
        f.s.take_reservations(),
        [marked_record(whole, 0, 40_000, ReservationEnd::Refused)]
    );
}

/// Catches: a request that waits again after its lease was given up taken for the
/// reservation it had, or a result of its earlier run that ends it left tracked. `b`
/// carries a lease of 900 s and the request starts on the empty `a` at 0 (no record).
/// `a` falls silent; at 60 s, the lease grace, its lease is given up and the request waits
/// again, now holding `b`. The earlier run's result then commits at 80 s and ends it: one
/// record, blocked and reserved at 60 s, that says finished.
#[test]
fn a_request_ended_by_an_earlier_runs_result_while_it_waits_emits_finished() {
    let mut f = recording();
    f.worker("a");
    f.worker("b");
    f.lease_on("b", secs(900));
    let whole = f.submit_whole(None);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].worker, w("a"));
    f.start(&grants[0]);
    f.workers.retain(|worker| *worker != w("a"));
    f.running.retain(|(worker, _)| *worker != w("a"));
    assert!(f.advance(70_000).is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
    assert_eq!(
        f.s.take_reservations(),
        [released(&grants[0], 0, 60_000, None, false)]
    );
    f.at(80_000);
    f.feed(Event::Committed(ControlRecord::Result(ResultRecord {
        lease: grants[0].lease,
        operation: whole,
        outcome: Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [200; 32], 200).into(),
        },
    })));
    let record = ReservationRecord {
        operation: whole,
        blocked_at: at(60_000),
        reserved_at: Some(at(60_000)),
        estimated_start: Some(at(900_000)),
        last_estimate: Some(at(900_000)),
        moves: 0,
        backfilled_core_seconds: 0,
        ended_at: at(80_000),
        end: ReservationEnd::Finished,
    };
    assert_eq!(f.s.take_reservations(), [ReservationEvent::Ended(record)]);
}

/// Catches: backfill counted to a request's earlier reservation counted again in its next
/// one. `a` carries a lease of 600 s and `b` one of 900 s; the request holds `a`, and at
/// 10 s a job of a core and 300 s is placed there as backfill. `a` is cordoned and the hold
/// moves to `b`; `b`'s lease ends at 20 s and the request starts there, its record counting
/// the job's 10 core-seconds so far. `b` falls silent, the request's lease is given up at
/// 80 s and it holds `a` again, where the job still runs. Cancelled at 90 s, its second
/// record counts no backfill: the job was placed around its first reservation.
#[test]
fn backfill_counted_to_one_reservation_is_not_counted_to_the_next() {
    let mut f = recording();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(600));
    let on_b = f.lease_on("b", secs(900));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.beat();
    let backfill = job(&mut f, secs(300)).expect("the job backfills on a");
    assert_eq!(backfill.worker, w("a"));
    f.feed(Event::Cordon { worker: w("a") });
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.at(20_000);
    f.beat();
    f.finish(&on_b);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, whole);
    f.start(&grants[0]);
    let first = ReservationRecord {
        operation: whole,
        blocked_at: at(0),
        reserved_at: Some(at(0)),
        estimated_start: Some(at(600_000)),
        last_estimate: Some(at(900_000)),
        moves: 1,
        backfilled_core_seconds: 10,
        ended_at: at(20_000),
        end: ReservationEnd::Started { worker: w("b") },
    };
    assert_eq!(
        f.s.take_reservations(),
        [
            released(&on_b, 0, 20_000, Some(secs(900)), false),
            ReservationEvent::Ended(first),
        ]
    );
    f.feed(Event::Uncordon { worker: w("a") });
    f.workers.retain(|worker| *worker != w("b"));
    f.running.retain(|(worker, _)| *worker != w("b"));
    assert!(f.advance(90_000).is_empty());
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.s.cancel_whole(whole);
    let second = ReservationRecord {
        operation: whole,
        blocked_at: at(80_000),
        reserved_at: Some(at(80_000)),
        estimated_start: Some(at(600_000)),
        last_estimate: Some(at(600_000)),
        moves: 0,
        backfilled_core_seconds: 0,
        ended_at: at(90_000),
        end: ReservationEnd::Cancelled,
    };
    assert_eq!(
        f.s.take_reservations(),
        [
            released(&grants[0], 20_000, 80_000, None, false),
            ReservationEvent::Ended(second),
        ]
    );
}

/// Catches: a lease placed on a worker held from outside the scheduler not reported as
/// backfill (it has no request here to count to). `a` is held from outside with its start
/// at 600 s; a job of 300 s placed there at 10 s is backfill, and its release says so.
#[test]
fn a_lease_on_a_worker_held_from_outside_is_backfilled() {
    let mut f = recording();
    f.worker("a");
    f.s.reserve_outside(w("a"), Some(at(600_000)));
    f.at(10_000);
    f.beat();
    let backfill = job(&mut f, secs(300)).expect("the job backfills on a");
    assert_eq!(backfill.worker, w("a"));
    assert!(f.advance(310_000).is_empty());
    f.finish(&backfill);
    assert_eq!(
        f.s.take_reservations(),
        [released(&backfill, 10_000, 310_000, Some(secs(300)), true)]
    );
}

/// Catches: events kept by a scheduler not asked to record them (they would pile up in a
/// caller that never takes them). The same start as above, unrecorded: nothing to take.
#[test]
fn a_scheduler_not_recording_keeps_nothing() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(crate::booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(4))
            .with_backfill(Backfill::enforced(DAY)),
    );
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    let whole = f.whole();
    assert!(f.advance(599_000).is_empty());
    f.at(600_000);
    f.finish(&lease);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, whole);
    assert_eq!(f.s.take_reservations(), []);
}
