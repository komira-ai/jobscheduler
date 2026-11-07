//! Cancel and the whole-node reads at their edges ([`Scheduler::cancel_whole`],
//! [`Scheduler::whole_holder`], [`Scheduler::whole_marked_worker`],
//! [`Scheduler::whole_room`]): a cancel before the grant is committed and the records that
//! come after it, a recovered lease cancelled before its worker registers, joined callers
//! and an action's booking, an unmarked request and finished operations; and the reads
//! across the lease grace, a bar's end, a phase change, a hold set outside, and with and
//! without whole-node settings. Every time is farm time fed to the scheduler; nothing
//! waits.

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
    Bar, Cancelled, Event, NodePhase, Recovered, RecoveredLease, Request, RestoredWhole, Room,
    Scheduler,
};
use js_types::sched::{
    ControlRecord, Effect, Failure, LeaseId, LeaseKind, OperationId, Outcome, WaiterId,
};

use crate::digest::{Digest, DigestFunction};
use crate::whole_api::{MAX_WALL, ask, busy, cfg, farm, placed, restored, snapshot, t, waiter};
use crate::whole_rig::{Farm, request, secs, w};

/// A completed outcome, as a worker reports it.
fn completed() -> Outcome {
    Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [200; 32], 200).into(),
    }
}

/// Catches: a cancel before the grant is committed that leaves the worker booked, or one
/// that a late record revives. The request is granted `b` and cancelled before the grant
/// comes back; the grant, a start and a result fed afterwards do nothing (no `Start`, no
/// operation), `b`'s booking is what it was, and the next request is granted `b`.
#[test]
fn a_cancel_before_the_grant_is_committed_survives_the_late_records() {
    let mut f = farm(cfg(0, 2));
    f.worker("b");
    let empty = f.s.booked(&w("b"));
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let by = waiter(&f);
    let grants = f.grants();
    assert_eq!(placed(&grants), [(op, w("b"))]);
    let lease = grants[0].lease;
    let held = Cancelled::Held {
        lease,
        worker: w("b"),
        waiters: vec![by],
    };
    assert_eq!(f.s.cancel_whole(op), held);
    let commit = f.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    assert!(commit.is_empty(), "{commit:?}");
    let started = f.feed(Event::Started {
        operation: op,
        lease,
    });
    assert!(started.is_empty(), "{started:?}");
    let report = f.feed(Event::Report {
        operation: op,
        lease,
        outcome: completed(),
    });
    assert!(report.is_empty(), "{report:?}");
    assert_eq!(f.s.state(op), None);
    assert_eq!(f.s.booked(&w("b")), empty);
    let next = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(placed(&f.grants()), [(next, w("b"))]);
}

/// Catches: a cancelled recovered lease booked on its worker when the worker registers,
/// or a cancel that takes another recovered lease with it. Two leases are recovered on
/// `x` before it registers; the whole-machine one is cancelled; when `x` registers and
/// lists the other, only the other is held there, and no whole-machine lease holds it.
#[test]
fn a_recovered_lease_cancelled_before_its_worker_registers_is_not_booked() {
    let recovered = |n: u8, kind, seq| Recovered {
        operation: OperationId(u64::from(n)),
        request: request(n, kind),
        waiter: WaiterId(u64::from(n)),
        lease: Some(RecoveredLease {
            lease: LeaseId::new(1, seq),
            worker: w("x"),
            cost: None,
        }),
        losses: 0,
        fate: None,
    };
    let mut s = Scheduler::new(2)
        .with_load_config(booked::booked())
        .with_whole_node(cfg(0, 2));
    let whole = recovered(3, LeaseKind::WholeMachine, 5);
    let action = recovered(4, LeaseKind::Action, 6);
    s.recover(2, OperationId(10), 0, vec![whole, action]);
    let mut f = Farm::new(s);
    let held = Cancelled::Held {
        lease: LeaseId::new(1, 5),
        worker: w("x"),
        waiters: vec![WaiterId(3)],
    };
    assert_eq!(f.s.cancel_whole(OperationId(3)), held);
    f.worker("x");
    f.running.push((w("x"), LeaseId::new(1, 6)));
    f.beat();
    assert_eq!(f.s.leases_on(&w("x")), [LeaseId::new(1, 6)]);
    assert_eq!(f.s.whole_holder(&w("x")), None);
    assert_eq!(f.s.state(OperationId(3)), None);
    assert!(f.s.state(OperationId(4)).is_some());
}

/// Catches: a cancel that answers only one of the callers, leaves an action's booking or
/// its dedup entry, or leaves a whole-machine request's count and bars. Without
/// whole-node settings, an action two callers share is cancelled while it runs: both
/// callers are returned, the worker's booking and leases are back to none, and the same
/// action asked again is new work. With them, a request with a restored count and bar is
/// cancelled and both are gone.
#[test]
fn a_cancel_returns_every_caller_and_clears_what_the_operation_held() {
    let mut f = Farm::new(Scheduler::new(1).with_load_config(booked::booked()));
    f.worker("a");
    let empty = f.s.booked(&w("a"));
    let op = f.s.next_operation();
    f.next_n = 1;
    f.submit(request(1, LeaseKind::Action));
    f.next_n = 2;
    f.submit(request(1, LeaseKind::Action));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(op, w("a"))]);
    f.start(&grants[0]);
    let held = Cancelled::Held {
        lease: grants[0].lease,
        worker: w("a"),
        waiters: vec![WaiterId(1), WaiterId(2)],
    };
    assert_eq!(f.s.cancel_whole(op), held);
    assert_eq!(f.s.booked(&w("a")), empty);
    assert!(f.s.leases_on(&w("a")).is_empty());
    let again = f.s.next_operation();
    assert_ne!(again, op);
    f.next_n = 3;
    f.submit(request(1, LeaseKind::Action));
    assert_eq!(placed(&f.grants()), [(again, w("a"))]);

    let mut f = busy(cfg(0, 2));
    let op = ask(&mut f);
    let state = RestoredWhole {
        bars: BTreeMap::from([(w("c"), Bar::ForLife)]),
        ..restored(op, None)
    };
    assert_eq!(f.s.restore_whole(state), Ok(()));
    let cancelled = f.s.cancel_whole(op);
    assert!(
        matches!(cancelled, Cancelled::Queued { .. }),
        "{cancelled:?}"
    );
    assert_eq!(f.s.whole_barred(op, t(0)), BTreeMap::new());
}

/// Catches: an unmarked request's cancel reporting a mark, a repeated cancel that does
/// anything, and a cancel that changes a failed or a refused operation. A request still
/// before `drain_after` is cancelled with no mark and again with nothing; a failed action
/// and a refused probe are left as they were, the whole scheduler unchanged.
#[test]
fn an_unmarked_request_is_cancelled_and_a_failed_or_refused_one_is_left() {
    let mut f = busy(cfg(30_000, 2));
    let early = ask(&mut f);
    let by = waiter(&f);
    let queued = Cancelled::Queued {
        waiters: vec![by],
        mark: None,
    };
    assert_eq!(f.s.cancel_whole(early), queued);
    assert_eq!(f.s.cancel_whole(early), Cancelled::Nothing);

    f.worker("b");
    f.next_n += 1;
    let failing = f.s.next_operation();
    f.submit(request(f.next_n, LeaseKind::Action));
    let grants = f.grants();
    assert_eq!(placed(&grants)[0].0, failing);
    f.start(&grants[0]);
    let proposed = f.feed(Event::Report {
        operation: failing,
        lease: grants[0].lease,
        outcome: Outcome::Failed(Failure::Invalid),
    });
    let [Effect::Commit(record)] = proposed.as_slice() else {
        panic!("{proposed:?}");
    };
    f.feed(Event::Committed(record.clone()));
    assert!(f.s.state(failing).is_some_and(|s| s.is_done()));
    let before = snapshot(&f);
    assert_eq!(f.s.cancel_whole(failing), Cancelled::Nothing);
    assert_eq!(snapshot(&f), before);

    f.next_n += 1;
    let refused = f.s.next_operation();
    f.submit(Request {
        pin: Some(w("z")),
        ..request(f.next_n, LeaseKind::Action)
    });
    let refusal = f
        .feed(Event::Tick)
        .into_iter()
        .find_map(|e| match e {
            Effect::Commit(record @ ControlRecord::Refusal(_)) => Some(record),
            _ => None,
        })
        .expect("a probe of an unknown worker is refused at once");
    f.feed(Event::Committed(refusal));
    assert!(f.s.state(refused).is_some_and(|s| s.is_done()));
    let before = snapshot(&f);
    assert_eq!(f.s.cancel_whole(refused), Cancelled::Nothing);
    assert_eq!(snapshot(&f), before);
}

/// Feeds a heartbeat of `worker` at `ms`: the scheduler's time moves on, and no round runs.
fn heard(f: &mut Farm, worker: &str, ms: u64) {
    f.at(ms);
    f.feed(Event::Heartbeat {
        worker: w(worker),
        running: Vec::new(),
        load: None,
    });
}

/// Catches: a read that holds to the last round where it reads the time now. With no
/// round run, `q` (barred to a request until 30 s) shows the request barred at 29.999 s
/// and not at 30 s; heard last at 0, it is quiet until a millisecond before the lease
/// grace (60 s) ends and not at it; `r`, quiet, is not once it reports draining. No read
/// changes the scheduler.
#[test]
fn the_reads_follow_the_clock_and_the_phase_between_rounds() {
    let mut f = farm(cfg(0, 2));
    f.worker("q");
    f.worker("r");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(f.s.bar_whole(op, w("q"), Bar::Until(t(30_000))), Ok(()));
    heard(&mut f, "r", 29_999);
    let before = snapshot(&f);
    let barred = Room {
        quiet: true,
        barred_for: BTreeSet::from([op]),
        marked_for: None,
    };
    assert_eq!(f.s.whole_room(&w("q")), barred);
    assert_eq!(snapshot(&f), before);
    heard(&mut f, "r", 30_000);
    assert_eq!(f.s.whole_barred(op, t(30_000)), BTreeMap::new());
    let quiet = Room {
        quiet: true,
        ..Room::default()
    };
    assert_eq!(f.s.whole_room(&w("q")), quiet);
    heard(&mut f, "r", 59_999);
    assert_eq!(f.s.whole_room(&w("q")), quiet);
    heard(&mut f, "r", 60_000);
    assert_eq!(f.s.whole_room(&w("q")), Room::default());
    assert_eq!(f.s.whole_room(&w("r")), quiet);
    f.feed(Event::Phase {
        worker: w("r"),
        phase: NodePhase::Draining,
    });
    let before = snapshot(&f);
    assert_eq!(f.s.whole_room(&w("r")), Room::default());
    assert_eq!(snapshot(&f), before);
}

/// Catches: the holder read and the worker read disagreeing in a state, a marked worker
/// that empties shown as unmarked before the next round, reads that need whole-node
/// settings for a lease, and an action that owns its worker taken for a whole-machine
/// holder. Leased and then running on `b`, the request is `b`'s holder and `b` is its
/// worker; `a`'s lease ends under a mark and `a` shows quiet and still marked; without
/// settings a whole-machine lease still has its holder and worker and no mark, and an
/// owner action is no holder.
#[test]
fn the_holder_and_the_worker_reads_agree_in_every_state() {
    let mut f = farm(cfg(0, 2));
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    f.worker("b");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(placed(&grants), [(op, w("b"))]);
    assert_eq!(f.s.whole_holder(&w("b")), Some(op));
    assert_eq!(f.s.whole_marked_worker(op), Some(&w("b")));
    // `a` and `b` have the same tail (600 s) and one lease each: `a` is first by name.
    let marked = ask(&mut f);
    assert_eq!(f.s.whole_marked_worker(marked), Some(&w("a")));
    f.start(&grants[0]);
    assert_eq!(f.s.whole_holder(&w("b")), Some(op));
    assert_eq!(f.s.whole_marked_worker(op), Some(&w("b")));
    f.finish(&lease);
    let emptied = Room {
        quiet: true,
        barred_for: BTreeSet::new(),
        marked_for: Some(marked),
    };
    assert_eq!(f.s.whole_room(&w("a")), emptied);
    assert_eq!(f.s.whole_marked_worker(marked), Some(&w("a")));
    assert_eq!(f.s.whole_holder(&w("a")), None);

    let mut f = Farm::new(Scheduler::new(1).with_load_config(booked::booked()));
    f.worker("a");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert_eq!(placed(&f.grants()), [(op, w("a"))]);
    assert_eq!(f.s.whole_holder(&w("a")), Some(op));
    assert_eq!(f.s.whole_marked_worker(op), Some(&w("a")));
    assert_eq!(f.s.whole_room(&w("a")), Room::default());
    f.worker("o");
    f.next_n += 1;
    let owner = f.s.next_operation();
    f.submit(Request {
        whole_worker: true,
        ..request(f.next_n, LeaseKind::Action)
    });
    assert_eq!(placed(&f.grants()), [(owner, w("o"))]);
    assert_eq!(f.s.whole_holder(&w("o")), None);
    assert_eq!(f.s.whole_marked_worker(owner), None);
}

/// Catches: a reader taking `quiet` and no mark for leave to place, on a worker held with
/// [`Scheduler::reserve_outside`]: `q` is quiet and unmarked by the read, and still no
/// whole-machine request of this scheduler is granted it.
#[test]
fn the_room_does_not_read_a_hold_set_outside() {
    let mut f = farm(cfg(0, 2));
    f.worker("q");
    f.s.reserve_outside(w("q"), None);
    let quiet = Room {
        quiet: true,
        ..Room::default()
    };
    assert_eq!(f.s.whole_room(&w("q")), quiet);
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_room(&w("q")), quiet);
    assert_eq!(f.s.whole_mark(op), None);
}
