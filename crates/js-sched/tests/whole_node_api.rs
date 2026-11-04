//! The whole-node state a caller gives the scheduler from outside, and reads back
//! ([`Scheduler::restore_whole`], [`Scheduler::bar_whole`], [`Scheduler::cancel_whole`],
//! [`Scheduler::unplace_keeping_queued_at`], [`Scheduler::whole_room`] and the other
//! reads): restored marks, counts and bars, hard bars, the queue time kept across a
//! requeue, cancel in every state, and the tail a mark keeps. Every time is farm time fed
//! to the scheduler; nothing waits.

#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_api/mod.rs"]
mod whole_api;

use std::collections::{BTreeMap, BTreeSet};

use js_sched::{
    Backfill, Bar, Cancelled, Event, Recovered, RestoredMark, RestoredWhole, Room, Scheduler,
    WholeError, WholeMark, WholeState,
};
use js_types::sched::{ControlRecord, LeaseKind, OperationId, Outcome, WaiterId};
use js_whole::WholeNodeConfig;

use crate::digest::{Digest, DigestFunction};
use crate::whole_api::{
    DAY, MAX_WALL, ask, busy, cfg, farm, job, mark_on, ordinary_worker, placed, restored, snapshot,
    t, waiter,
};
use crate::whole_rig::{Farm, request, secs, w};

/// Catches: a restart that loses the queue time, the give-up count or the mark (today's
/// `recover` takes the time now and starts every count at zero): an operation taken up at
/// 200 s and restored as queued at 100 s with one give-up and a mark on `a` keeps the mark
/// in the next round without waiting `drain_after` (30 s) again, with the restored time,
/// deadline and tail end (500 s, not the 600 s the worker's tail reads now), and the give-up at that deadline is counted as the second, with
/// the back-off it sets readable.
#[test]
fn restore_whole_keeps_give_ups_queued_at_and_the_mark() {
    let mut f = busy(cfg(30_000, 2));
    assert!(f.advance(200_000).is_empty());
    let op = f.s.next_operation();
    f.next_n += 1;
    let taken = f.s.take_in(Recovered {
        operation: op,
        request: request(f.next_n, LeaseKind::WholeMachine),
        waiter: WaiterId(1),
        lease: None,
        losses: 0,
        fate: None,
    });
    assert!(taken);
    let mark = RestoredMark {
        worker: w("a"),
        made: t(150_000),
        deadline: t(260_000),
        tail_end: Some(t(500_000)),
    };
    let state = RestoredWhole {
        operation: op,
        queued_at: t(100_000),
        give_ups: 1,
        bars: BTreeMap::new(),
        mark: Some(mark),
    };
    assert_eq!(f.s.restore_whole(state), Ok(()));
    assert!(f.grants().is_empty());
    let standing = WholeMark {
        worker: w("a"),
        made: t(150_000),
        deadline: t(260_000),
        tail_end: Some(t(500_000)),
    };
    assert_eq!(f.s.whole_mark(op), Some(&standing));
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert!(f.advance(259_999).is_empty());
    assert_eq!(f.s.whole_mark(op), Some(&standing));
    assert!(f.advance(260_000).is_empty());
    assert_eq!(f.s.whole_give_ups(op), 2);
    assert_eq!(f.s.whole_mark(op), None);
    assert_eq!(
        f.s.whole_barred(op, t(260_000)),
        BTreeMap::from([(w("a"), Bar::Backoff(t(270_000)))])
    );
    assert_eq!(f.s.whole_barred(op, t(270_000)), BTreeMap::new());
}

/// Worker `a` busy and `b` quiet; a whole-machine request granted `b` at zero, then at
/// 40 s (past `drain_after`, 30 s) `b` cordoned and the grant committed and undone (its
/// run never started, as when the prepare fails), keeping the queue
/// time if `keep`. The farm after a round at 40 s, and the request.
fn requeued(keep: bool) -> (Farm, OperationId) {
    let mut f = busy(cfg(30_000, 2));
    f.worker("b");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(op, w("b"))]);
    assert!(f.advance(40_000).is_empty());
    f.feed(Event::Cordon { worker: w("b") });
    f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    let lease = grants[0].lease;
    let back = if keep {
        f.s.unplace_keeping_queued_at(lease)
    } else {
        f.s.unplace(lease)
    };
    assert_eq!(back, Some(op));
    assert!(f.grants().is_empty());
    (f, op)
}

/// Catches: a requeue after a failed prepare that restarts `drain_after`: put back with
/// its queue time kept (zero), the request marks the busy `a` in the round at 40 s; put
/// back with plain `unplace` it takes the time now and is early until 70 s.
#[test]
fn a_requeue_keeps_queued_at() {
    let (f, op) = requeued(true);
    let mark = f.s.whole_mark(op).expect("marked");
    assert_eq!((&mark.worker, mark.made), (&w("a"), t(40_000)));
    let (f, op) = requeued(false);
    assert_eq!(f.s.whole_mark(op), None);
    assert_eq!(
        f.s.whole_state(op),
        Some(WholeState::Early {
            may_hold_at: t(70_000)
        })
    );
}

/// Catches: a hard bar that is soft (the only quiet worker granted because it is the last
/// one that could run the request), one that applies to every operation, one that does
/// not end, and a bar for life that ends: barred until 50 s, the request is not granted
/// the only worker while it is quiet, and is granted it at 50 s; meanwhile another request
/// is granted it; a request barred for life never is.
#[test]
fn a_hard_bar_is_never_offered() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    let first = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(f.s.bar_whole(first, w("a"), Bar::Until(t(50_000))), Ok(()));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_mark(first), None);
    assert_eq!(
        f.s.whole_barred(first, t(0)),
        BTreeMap::from([(w("a"), Bar::Until(t(50_000)))])
    );
    assert_eq!(
        f.s.whole_state(first),
        Some(WholeState::Barred {
            workers: BTreeMap::from([(w("a"), t(50_000))]),
            give_ups: 0,
        })
    );
    let other = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(other, w("a"))]);
    assert!(f.advance(10_000).is_empty());
    f.start(&grants[0]);
    f.finish(&grants[0]);
    assert!(f.grants().is_empty());
    assert!(f.advance(49_999).is_empty());
    let grants = f.advance(50_000);
    assert_eq!(placed(&grants), [(first, w("a"))]);
    let last = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(f.s.bar_whole(last, w("a"), Bar::ForLife), Ok(()));
    f.start(&grants[0]);
    f.finish(&grants[0]);
    assert!(f.grants().is_empty());
    assert!(f.advance(1_000_000).is_empty());
    assert_eq!(
        f.s.whole_barred(last, t(1_000_000)),
        BTreeMap::from([(w("a"), Bar::ForLife)])
    );
    assert_eq!(
        f.s.whole_state(last),
        Some(WholeState::Barred {
            workers: BTreeMap::from([(w("a"), t(u64::MAX))]),
            give_ups: 0,
        })
    );
}

/// Catches: a bar on the marked worker that leaves the mark (the request would keep
/// holding a worker it may not take), or one that drops a mark it does not bar (a bar
/// that has already ended, a bar on another worker): only the bar that holds on `a` drops
/// the mark, with no give-up counted, and the request marks `a` again when it ends.
#[test]
fn a_bar_on_the_marked_worker_drops_the_mark() {
    let mut f = busy(cfg(0, 2));
    let op = ask(&mut f);
    let mark = f.s.whole_mark(op).cloned().expect("marked");
    assert_eq!(mark.worker, w("a"));
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::Until(t(0))), Ok(()));
    assert_eq!(f.s.whole_mark(op), Some(&mark));
    assert_eq!(f.s.bar_whole(op, w("b"), Bar::ForLife), Ok(()));
    assert_eq!(f.s.whole_mark(op), Some(&mark));
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::Until(t(30_000))), Ok(()));
    assert_eq!(f.s.whole_mark(op), None);
    assert_eq!(f.s.whole_give_ups(op), 0);
    assert_eq!(
        f.s.whole_barred(op, t(0)),
        BTreeMap::from([(w("a"), Bar::Until(t(30_000))), (w("b"), Bar::ForLife)])
    );
    assert!(f.advance(29_999).is_empty());
    assert_eq!(f.s.whole_mark(op), None);
    assert!(f.advance(30_000).is_empty());
    let again = f.s.whole_mark(op).expect("marked again");
    assert_eq!((&again.worker, again.made), (&w("a"), t(30_000)));
}

/// Catches: a cancelled operation left holding its worker or its mark, a cancel that
/// changes a finished operation, and a second cancel that does more than the first. A
/// queued request that marks `a` leaves with its mark, and `a` takes an action; a leased
/// and a running whole-machine operation on `b` release `b` (quiet at once, granted to the
/// next request) and name their lease to stop; a result for the cancelled lease is
/// dropped; a finished action is left as it is; cancelling again changes nothing.
#[test]
fn cancel_whole_ends_a_leased_and_a_running_operation() {
    let mut f = busy(cfg(0, 2));
    let marked = ask(&mut f);
    let marked_by = waiter(&f);
    assert_eq!(
        f.s.cancel_whole(marked),
        Cancelled::Queued {
            waiters: vec![marked_by],
            mark: Some(w("a")),
        }
    );
    assert_eq!(f.s.whole_mark(marked), None);
    assert_eq!(f.s.state(marked), None);
    assert_eq!(f.s.whole_room(&w("a")).marked_for, None);
    f.next_n += 1;
    f.submit(request(f.next_n, LeaseKind::Action));
    let action = f.grants();
    assert_eq!(placed(&action)[0].1, w("a"));
    f.start(&action[0]);

    f.worker("b");
    let leased = f.submit_whole(Some(secs(MAX_WALL)));
    let leased_by = waiter(&f);
    let grants = f.grants();
    assert_eq!(placed(&grants), [(leased, w("b"))]);
    f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    assert_eq!(f.s.whole_holder(&w("b")), Some(leased));
    assert_eq!(
        f.s.cancel_whole(leased),
        Cancelled::Held {
            lease: grants[0].lease,
            worker: w("b"),
            waiters: vec![leased_by],
        }
    );
    assert_eq!(f.s.whole_holder(&w("b")), None);
    assert!(f.s.whole_room(&w("b")).quiet);

    let running = f.submit_whole(Some(secs(MAX_WALL)));
    let running_by = waiter(&f);
    let grants = f.grants();
    assert_eq!(placed(&grants), [(running, w("b"))]);
    f.start(&grants[0]);
    assert_eq!(f.s.whole_marked_worker(running), Some(&w("b")));
    assert_eq!(
        f.s.cancel_whole(running),
        Cancelled::Held {
            lease: grants[0].lease,
            worker: w("b"),
            waiters: vec![running_by],
        }
    );
    assert!(f.s.whole_room(&w("b")).quiet);
    let late = f.feed(Event::Report {
        operation: running,
        lease: grants[0].lease,
        outcome: Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [200; 32], 200).into(),
        },
    });
    assert!(late.is_empty(), "{late:?}");

    f.finish(&action[0]);
    let done = action[0].operation;
    let finished = f.s.state(done).cloned();
    assert!(finished.as_ref().is_some_and(js_sched::OpState::is_done));
    assert_eq!(f.s.cancel_whole(done), Cancelled::Nothing);
    assert_eq!(f.s.state(done).cloned(), finished);

    let before = snapshot(&f);
    assert_eq!(f.s.cancel_whole(leased), Cancelled::Nothing);
    assert_eq!(snapshot(&f), before);
}

/// Catches: a read that disagrees with the placement round (a mark it does not show, a
/// quiet worker that is busy, cordoned, unregistered or serves no whole-machine lease,
/// a bar on the wrong operation), or that changes what it reads. `a` is busy and marked,
/// `c` busy and barred to a second request, `q` quiet, `n` quiet but serving no
/// whole-machine lease, `k` quiet but cordoned, and `z` not registered.
#[test]
fn whole_marked_worker_holder_and_room_read_the_last_round() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    let action = f.lease_on("a", secs(MAX_WALL));
    f.worker("c");
    f.lease_on("c", secs(2 * MAX_WALL));
    let op = ask(&mut f);
    let barred = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(f.s.bar_whole(barred, w("c"), Bar::ForLife), Ok(()));
    f.worker("q");
    ordinary_worker(&mut f, "n");
    f.worker("k");
    f.feed(Event::Cordon { worker: w("k") });
    let before = snapshot(&f);

    assert_eq!(
        f.s.whole_room(&w("a")),
        Room {
            quiet: false,
            barred_for: BTreeSet::new(),
            marked_for: Some(op),
        }
    );
    let Some(WholeState::Holding { worker, .. }) = f.s.whole_state(op) else {
        panic!("holds");
    };
    assert_eq!(worker, w("a"));
    assert_eq!(
        f.s.whole_room(&w("c")),
        Room {
            quiet: false,
            barred_for: BTreeSet::from([barred]),
            marked_for: None,
        }
    );
    assert_eq!(
        f.s.whole_room(&w("q")),
        Room {
            quiet: true,
            ..Room::default()
        }
    );
    assert_eq!(f.s.whole_room(&w("n")), Room::default());
    assert_eq!(f.s.whole_room(&w("k")), Room::default());
    assert_eq!(f.s.whole_room(&w("z")), Room::default());
    assert_eq!(f.s.whole_marked_worker(op), Some(&w("a")));
    assert_eq!(f.s.whole_marked_worker(barred), None);
    assert_eq!(f.s.whole_marked_worker(action.operation), None);
    assert_eq!(f.s.whole_marked_worker(OperationId(999)), None);
    assert_eq!(f.s.whole_holder(&w("a")), None);
    assert_eq!(snapshot(&f), before);
}

/// Catches: a mark whose tail end moves after it is made, and a restored mark whose tail
/// end is not the one backfill is judged against (a backfilled job could then outlast
/// the tail the worker was chosen on). `a`'s lease may run 600 s when the request marks
/// it; its bound then becomes an hour. The mark keeps its tail end of 600 s, and at 10 s
/// a job of 1000 s (inside the hour, not inside 600 s) is not placed while one of 300 s
/// is. Restored with a tail end of 400 s, at 20 s a job of 350 s (in time for the 600 s
/// the last round held by, not for 400 s) is not placed and one of 300 s is.
#[test]
fn the_mark_keeps_the_tail_it_was_chosen_on() {
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .with_backfill(Backfill::enforced(DAY));
    let mut f = Farm::new(s);
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    let whole = f.whole();
    let mark = f.s.whole_mark(whole).cloned().expect("marked");
    assert_eq!((&mark.worker, mark.tail_end), (&w("a"), Some(t(600_000))));
    f.s.set_max_wall(lease.operation, secs(3_600));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, secs(1_000)), None);
    assert_eq!(job(&mut f, secs(300)), Some(w("a")));
    assert_eq!(f.s.whole_mark(whole), Some(&mark));

    f.at(20_000);
    f.beat();
    let state = RestoredWhole {
        mark: Some(RestoredMark {
            worker: w("a"),
            made: mark.made,
            deadline: mark.deadline,
            tail_end: Some(t(400_000)),
        }),
        ..restored(whole, None)
    };
    assert_eq!(f.s.restore_whole(state), Ok(()));
    assert_eq!(job(&mut f, secs(350)), None);
    assert_eq!(job(&mut f, secs(300)), Some(w("a")));
    let kept = f.s.whole_mark(whole).map(|m| m.tail_end);
    assert_eq!(kept, Some(Some(t(400_000))));
}

/// Catches: a restore or a bar that is taken in part, or at all, when it names what the
/// scheduler cannot hold, and one refused when it should be taken: each refusal is the
/// named error, says what it is in full, and leaves the scheduler as it was.
#[test]
fn restore_refuses_what_it_cannot_hold() {
    let mut off = Farm::new(Scheduler::new(1).with_load_config(booked::booked()));
    off.worker("a");
    let op = off.submit_whole(None);
    let before = snapshot(&off);
    let refused = off.s.restore_whole(restored(op, None));
    assert_eq!(refused, Err(WholeError::Off));
    assert_eq!(
        off.s.bar_whole(op, w("a"), Bar::ForLife),
        Err(WholeError::Off)
    );
    assert_eq!(snapshot(&off), before);
    assert_eq!(WholeError::Off.to_string(), "whole-node settings are off");

    // `b` holds a whole-machine lease, `op1` marks `a`, `op2` is queued with no mark.
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    let action = f.lease_on("a", secs(MAX_WALL)).operation;
    f.worker("c");
    f.lease_on("c", secs(2 * MAX_WALL));
    f.worker("b");
    let leased = f.submit_whole(Some(secs(3 * MAX_WALL)));
    assert_eq!(placed(&f.grants()), [(leased, w("b"))]);
    let op1 = ask(&mut f);
    assert_eq!(f.s.whole_mark(op1).map(|m| m.worker.clone()), Some(w("a")));
    let op2 = f.submit_whole(Some(secs(MAX_WALL)));

    let unknown = OperationId(999);
    let barred_c = RestoredWhole {
        bars: BTreeMap::from([(w("c"), Bar::ForLife)]),
        ..restored(op2, Some(mark_on("c")))
    };
    let cases = [
        (
            restored(unknown, None),
            WholeError::UnknownOperation(unknown),
            "op-999 is not an operation this scheduler holds".to_owned(),
        ),
        (
            restored(action, None),
            WholeError::NotWholeMachine(action),
            format!("{action} is not a whole-machine request"),
        ),
        (
            restored(leased, Some(mark_on("b"))),
            WholeError::NotQueued(leased),
            format!("{leased} is not queued, so it holds no mark"),
        ),
        (
            restored(op2, Some(mark_on("z"))),
            WholeError::UnknownWorker {
                operation: op2,
                worker: w("z"),
            },
            format!("{op2} cannot mark z: the worker is not registered"),
        ),
        (
            restored(op2, Some(mark_on("a"))),
            WholeError::Marked {
                operation: op2,
                worker: w("a"),
                by: op1,
            },
            format!("{op2} cannot mark a: {op1} marks it"),
        ),
        (
            barred_c,
            WholeError::Barred {
                operation: op2,
                worker: w("c"),
            },
            format!("{op2} cannot mark c: the worker is barred to it"),
        ),
    ];
    for (state, error, text) in cases {
        let before = snapshot(&f);
        assert_eq!(f.s.restore_whole(state), Err(error.clone()));
        assert_eq!(snapshot(&f), before);
        assert_eq!(error.to_string(), text);
    }
    let before = snapshot(&f);
    let refused = f.s.bar_whole(unknown, w("a"), Bar::ForLife);
    assert_eq!(refused, Err(WholeError::UnknownOperation(unknown)));
    assert_eq!(snapshot(&f), before);

    // Taken: its own mark again, a leased operation's count with no mark, and a mark on
    // a worker whose bar has ended while another operation marks another worker.
    let own = RestoredWhole {
        give_ups: 1,
        ..restored(op1, Some(mark_on("a")))
    };
    assert_eq!(f.s.restore_whole(own), Ok(()));
    assert_eq!(f.s.whole_give_ups(op1), 1);
    let counted = RestoredWhole {
        give_ups: 1,
        ..restored(leased, None)
    };
    assert_eq!(f.s.restore_whole(counted), Ok(()));
    assert_eq!(f.s.whole_give_ups(leased), 1);
    let ended = RestoredWhole {
        bars: BTreeMap::from([(w("c"), Bar::Until(t(0)))]),
        ..restored(op2, Some(mark_on("c")))
    };
    assert_eq!(f.s.restore_whole(ended), Ok(()));
    assert_eq!(f.s.whole_marked_worker(op2), Some(&w("c")));
}
