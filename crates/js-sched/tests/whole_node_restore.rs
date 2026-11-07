//! Restored whole-node state and bars at their edges ([`Scheduler::restore_whole`],
//! [`Scheduler::bar_whole`], [`Scheduler::unplace_keeping_queued_at`]): bars restored for
//! workers that register later, a hard bar next to a soft preference, a restore that
//! replaces what the operation had, a deadline already passed,
//! restored marks over the ceiling, the history a requeue keeps, bars on a probe, a held
//! operation and an action, and the backfill start of a mark whose tail is unknown or
//! restored. Every time is farm time fed to the scheduler; nothing waits.

#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_api/mod.rs"]
mod whole_api;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use std::collections::{BTreeMap, BTreeSet};

use js_sched::{
    Backfill, Bar, Cancelled, Event, Fate, OpState, Recovered, Request, RestoredMark,
    RestoredWhole, Scheduler, WholeError, WholeState, WholeWait,
};
use js_types::sched::{
    ControlRecord, Effect, LeaseGrant, LeaseId, LeaseKind, OperationId, WaiterId,
};
use js_whole::WholeNodeConfig;

use crate::whole_api::{
    DAY, MAX_WALL, ask, cfg, farm, job, mark_on, placed, restored, snapshot, t,
};
use crate::whole_rig::{Farm, request, secs, w};

/// Catches: a hard bar counted as the other worker that soft avoidance waits for. `a` is
/// barred for life and `b` is avoided (a farm fault ended a run there, or a back-off):
/// with `a` out for good, `b` is the only worker left, and the request is granted it,
/// as it would be with `a` not registered at all.
#[test]
fn a_hard_bar_does_not_hide_the_worker_left_to_avoid() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    f.worker("b");
    let op = f.s.next_operation();
    f.next_n += 1;
    let fate = Fate {
        avoid: BTreeSet::from([w("b")]),
        ..Fate::default()
    };
    let taken = f.s.take_in(Recovered {
        operation: op,
        request: request(f.next_n, LeaseKind::WholeMachine),
        waiter: WaiterId(1),
        lease: None,
        losses: 0,
        fate: Some(fate),
    });
    assert!(taken);
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::ForLife), Ok(()));
    assert_eq!(placed(&f.grants()), [(op, w("b"))]);

    let mut f = farm(cfg(0, 2));
    f.worker("a");
    f.worker("b");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::ForLife), Ok(()));
    let backoff = Bar::Backoff(t(100_000));
    assert_eq!(f.s.bar_whole(op, w("b"), backoff), Ok(()));
    assert_eq!(placed(&f.grants()), [(op, w("b"))]);
}

/// Catches: restored bars dropped or applied only to workers registered at the restore,
/// and hard bars counted as the other worker a back-off waits for: bars for life, until
/// 50 s and a back-off are restored before any worker registers, read back exactly, keep
/// `a` and `b` from the request once they register, and leave `c`, in a back-off, to be
/// granted when it is the only worker the request may take.
#[test]
fn restored_bars_hold_and_apply_to_workers_that_register_later() {
    let mut f = farm(cfg(0, 2));
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let bars = BTreeMap::from([
        (w("a"), Bar::ForLife),
        (w("b"), Bar::Until(t(50_000))),
        (w("c"), Bar::Backoff(t(50_000))),
    ]);
    let state = RestoredWhole {
        bars: bars.clone(),
        ..restored(op, None)
    };
    assert_eq!(f.s.restore_whole(state), Ok(()));
    assert_eq!(f.s.whole_barred(op, t(0)), bars);
    f.worker("a");
    f.worker("b");
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_mark(op), None);
    f.worker("c");
    assert_eq!(placed(&f.grants()), [(op, w("c"))]);
}

/// Catches: a restore that adds to what the operation had instead of replacing it, one
/// that is not the same when repeated, and a refused restore that changes a populated
/// state. The request marks `a`; a restore gives it a bar and a mark; a refused
/// one leaves all of it; a restore with nothing clears all of it, and again changes
/// nothing more.
#[test]
fn a_restore_replaces_what_the_operation_had() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    f.lease_on("a", secs(MAX_WALL));
    f.worker("c");
    f.lease_on("c", secs(2 * MAX_WALL));
    let op = ask(&mut f);
    let full = RestoredWhole {
        bars: BTreeMap::from([(w("c"), Bar::ForLife)]),
        ..restored(op, Some(mark_on("a")))
    };
    assert_eq!(f.s.restore_whole(full), Ok(()));
    assert_eq!(f.s.whole_mark(op).map(|m| m.deadline), Some(t(600_000)));
    let before = snapshot(&f);
    let refused = f.s.restore_whole(restored(op, Some(mark_on("z"))));
    let unknown = WholeError::UnknownWorker {
        operation: op,
        worker: w("z"),
    };
    assert_eq!(refused, Err(unknown));
    assert_eq!(snapshot(&f), before);
    assert_eq!(f.s.restore_whole(restored(op, None)), Ok(()));
    assert_eq!(f.s.whole_mark(op), None);
    assert_eq!(f.s.whole_barred(op, t(0)), BTreeMap::new());
    let cleared = snapshot(&f);
    assert_eq!(f.s.restore_whole(restored(op, None)), Ok(()));
    assert_eq!(snapshot(&f), cleared);
}

/// One worker `a` with a lease that may run ten minutes from zero, and a whole-machine
/// request that marks it: the farm, the lease and the request.
fn marked(config: WholeNodeConfig) -> (Farm, LeaseGrant, OperationId) {
    let mut f = farm(config);
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    let op = ask(&mut f);
    (f, lease, op)
}

/// Catches: a restored deadline that has passed acted on, and restored marks over the
/// ceiling all kept. A hold restored with a deadline of 30 s is kept at 40 s, showing that
/// deadline, and the request is granted `a` when its lease ends. A restored deadline of
/// zero is kept in the next round with nothing barred. With a ceiling of one, two
/// restored marks lose the newer one, and that request is stopped by a cap.
#[test]
fn restored_deadlines_and_marks_meet_the_caps() {
    let (mut f, lease, op) = marked(cfg(0, 2));
    let passed = RestoredWhole {
        mark: Some(RestoredMark {
            deadline: t(30_000),
            ..mark_on("a")
        }),
        ..restored(op, None)
    };
    assert_eq!(f.s.restore_whole(passed), Ok(()));
    assert!(f.advance(40_000).is_empty());
    let Some(WholeState::Holding {
        worker, deadline, ..
    }) = f.s.whole_state(op)
    else {
        panic!("holds");
    };
    assert_eq!((worker, deadline), (w("a"), t(30_000)));
    f.finish(&lease);
    assert_eq!(placed(&f.grants()), [(op, w("a"))]);

    let (mut f, _, op) = marked(cfg(0, 2));
    let zero = RestoredWhole {
        mark: Some(RestoredMark {
            deadline: t(0),
            ..mark_on("a")
        }),
        ..restored(op, None)
    };
    assert_eq!(f.s.restore_whole(zero), Ok(()));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_mark(op).map(|m| m.worker.clone()), Some(w("a")));
    assert_eq!(f.s.whole_barred(op, t(0)), BTreeMap::new());

    let one = WholeNodeConfig {
        max_whole_node: 1,
        ..cfg(0, 2)
    };
    let mut f = farm(one);
    f.worker("a");
    f.lease_on("a", secs(MAX_WALL));
    f.worker("c");
    f.lease_on("c", secs(2 * MAX_WALL));
    let older = f.submit_whole(Some(secs(MAX_WALL)));
    let newer = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(
        f.s.restore_whole(restored(older, Some(mark_on("a")))),
        Ok(())
    );
    let later = RestoredMark {
        made: t(5_000),
        ..mark_on("c")
    };
    assert_eq!(f.s.restore_whole(restored(newer, Some(later))), Ok(()));
    assert!(f.grants().is_empty());
    assert_eq!(
        f.s.whole_mark(older).map(|m| m.worker.clone()),
        Some(w("a"))
    );
    assert_eq!(f.s.whole_mark(newer), None);
    assert!(matches!(f.s.whole_wait(newer), Some(WholeWait::Cap(_))));
    assert_eq!(f.s.whole_summary().map(|s| s.marks), Some(1));
}

/// Catches: `unplace_keeping_queued_at` undoing what it should refuse, or losing a
/// restored history. A lease the scheduler does not hold, a running one and one already
/// undone are refused with nothing changed. A request restored as queued at 20 s with a
/// bar, granted `b` and undone at 110 s, keeps both: it marks the busy
/// `a` at once (110 s is past 20 s plus `drain_after`, 30 s), its bar stands,
/// `b`'s booking is released, and the grant fed back late starts nothing.
#[test]
fn a_requeue_keeps_the_restored_history_and_refuses_what_unplace_refuses() {
    let mut f = farm(cfg(30_000, 2));
    f.worker("a");
    let running = f.lease(secs(MAX_WALL));
    f.worker("b");
    let empty = f.s.booked(&w("b"));
    assert!(f.advance(100_000).is_empty());
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let bars = BTreeMap::from([(w("c"), Bar::ForLife)]);
    let state = RestoredWhole {
        queued_at: t(20_000),
        bars: bars.clone(),
        mark: None,
        operation: op,
    };
    assert_eq!(f.s.restore_whole(state), Ok(()));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(op, w("b"))]);
    assert!(f.advance(110_000).is_empty());
    f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    f.feed(Event::Cordon { worker: w("b") });

    let before = snapshot(&f);
    assert_eq!(f.s.unplace_keeping_queued_at(LeaseId::new(9, 9)), None);
    assert_eq!(f.s.unplace_keeping_queued_at(running.lease), None);
    assert_eq!(snapshot(&f), before);

    assert_eq!(f.s.unplace_keeping_queued_at(grants[0].lease), Some(op));
    let undone = snapshot(&f);
    assert_eq!(f.s.unplace_keeping_queued_at(grants[0].lease), None);
    assert_eq!(snapshot(&f), undone);
    assert_eq!(f.s.booked(&w("b")), empty);
    let late = f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    assert!(
        !late.iter().any(|e| matches!(e, Effect::Start(_))),
        "{late:?}"
    );
    assert!(f.grants().is_empty());
    let mark = f.s.whole_mark(op).expect("marked");
    assert_eq!((&mark.worker, mark.made), (&w("a"), t(110_000)));
    assert_eq!(f.s.whole_barred(op, t(110_000)), bars);
}

/// Catches: a bar that a pinned probe walks past, a bar on a held operation that ends its
/// lease or is lost by the time it is queued again, and a bar taken for an action. A
/// whole-machine probe pinned to the cordoned `p` barred to it is not placed and stays
/// queued. A request leased on `b` and barred from `b` keeps its lease; undone, it is not
/// granted `b` again though `b` is quiet. A bar for an action is refused, changing
/// nothing.
#[test]
fn bars_on_a_probe_a_held_operation_and_an_action() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    let action = f.lease(secs(MAX_WALL));
    f.worker("p");
    f.feed(Event::Cordon { worker: w("p") });
    let probe = f.s.next_operation();
    f.next_n += 1;
    f.submit(Request {
        pin: Some(w("p")),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    assert_eq!(f.s.bar_whole(probe, w("p"), Bar::ForLife), Ok(()));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.state(probe), Some(&OpState::Queued));
    let cancelled = f.s.cancel_whole(probe);
    assert!(
        matches!(cancelled, Cancelled::Queued { mark: None, .. }),
        "{cancelled:?}"
    );

    f.worker("b");
    let held = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(held, w("b"))]);
    f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    assert_eq!(f.s.bar_whole(held, w("b"), Bar::ForLife), Ok(()));
    assert!(matches!(f.s.state(held), Some(OpState::Leased { .. })));
    assert_eq!(f.s.leases_on(&w("b")), [grants[0].lease]);
    assert_eq!(f.s.unplace_keeping_queued_at(grants[0].lease), Some(held));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.state(held), Some(&OpState::Queued));

    let before = snapshot(&f);
    let refused = f.s.bar_whole(action.operation, w("a"), Bar::ForLife);
    assert_eq!(refused, Err(WholeError::NotWholeMachine(action.operation)));
    assert_eq!(snapshot(&f), before);
}

/// A scheduler with whole-node settings and backfill enforced up to a day, by the booked
/// rule.
fn backfilled() -> Farm {
    Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(4))
            .with_backfill(Backfill::enforced(DAY)),
    )
}

/// Catches: a mark made on a worker of unknown tail admitting backfill once the tail
/// becomes known (the unknown tail must fail closed for as long as the mark stands). `a`'s
/// lease has no wall time when the request marks it; given one of 600 s after, a job of a
/// second still does not backfill.
#[test]
fn a_mark_made_with_no_known_tail_admits_no_backfill() {
    let mut f = backfilled();
    f.worker("a");
    let lease = f.place_on("a", None);
    let whole = f.whole();
    assert_eq!(f.s.whole_mark(whole).map(|m| m.tail_end), Some(None));
    f.s.set_max_wall(lease.operation, secs(600));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, secs(1)), None);
    assert_eq!(f.s.whole_mark(whole).map(|m| m.tail_end), Some(None));
}

/// Catches: a restored mark held to the start the mark it replaced kept, and a restored
/// tail end lost where the tail now is unknown. `a` is marked by a tail of 600 s, its
/// lease's bound then becomes 1200 s, and a mark with a tail end of 900 s is restored: at
/// 10 s a job of 700 s (in time for 900 s, not for 600 s) backfills and one of 850 s
/// does not. On a worker whose lease has no known end, a restored tail end of 400 s
/// admits a job of 300 s at 20 s and not one of 350 s.
#[test]
fn a_restored_mark_bounds_backfill_by_its_own_tail() {
    let mut f = backfilled();
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    let whole = f.whole();
    let mark = f.s.whole_mark(whole).cloned().expect("marked");
    assert_eq!(mark.tail_end, Some(t(600_000)));
    f.s.set_max_wall(lease.operation, secs(1_200));
    let replaced = RestoredMark {
        worker: w("a"),
        made: t(5_000),
        deadline: mark.deadline,
        tail_end: Some(t(900_000)),
    };
    let state = restored(whole, Some(replaced));
    assert_eq!(f.s.restore_whole(state), Ok(()));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, secs(850)), None);
    assert_eq!(job(&mut f, secs(700)), Some(w("a")));

    let mut f = backfilled();
    f.worker("a");
    f.place_on("a", None);
    let whole = f.whole();
    let mark = f.s.whole_mark(whole).cloned().expect("marked");
    let known = RestoredMark {
        worker: w("a"),
        made: mark.made,
        deadline: mark.deadline,
        tail_end: Some(t(400_000)),
    };
    assert_eq!(f.s.restore_whole(restored(whole, Some(known))), Ok(()));
    f.at(20_000);
    f.beat();
    assert_eq!(job(&mut f, secs(350)), None);
    assert_eq!(job(&mut f, secs(300)), Some(w("a")));
    let kept = f.s.whole_mark(whole).map(|m| m.tail_end);
    assert_eq!(kept, Some(Some(t(400_000))));
}
