//! Which worker a request that waits for a worker to empty holds. With whole-node
//! settings the pick is the least tail: an action that owns its worker takes one hold per
//! pool, picked again every round, and a whole-machine mark stays on the worker it was
//! made on until its request is placed, cancelled or gives it up, however cheap another
//! worker becomes, and no other request's hold, mark or whole-machine placement takes
//! that worker (ordinary work is still placed there).
//! Without settings a whole-machine request holds as an action that owns its worker does:
//! the least tail, picked again every round, one hold per pool. Every time is farm time
//! fed to the scheduler.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use crate::whole_rig::{Farm, request, secs, w};
use js_sched::{Request, Scheduler};
use js_types::sched::{LeaseGrant, LeaseKind, OperationId, Qos};
use js_whole::WholeNodeConfig;

fn plain() -> Farm {
    Farm::new(Scheduler::new(1).with_load_config(booked::booked()))
}

fn with_settings(max_draining: usize) -> Farm {
    let cfg = WholeNodeConfig {
        max_draining,
        ..WholeNodeConfig::dedicated(4)
    };
    Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(cfg),
    )
}

/// Submits an action that owns its worker, and ticks: the operation.
fn owner(f: &mut Farm) -> OperationId {
    f.next_n += 1;
    let operation = f.s.next_operation();
    let request = Request {
        whole_worker: true,
        pin: None,
        ..request(f.next_n, LeaseKind::Action)
    };
    f.submit(request);
    assert!(f.grants().is_empty(), "no worker is empty");
    operation
}

/// `a` holds two leases that end within a minute, `b` one that runs an hour.
fn one_long_two_short(f: &mut Farm) {
    f.worker("a");
    f.lease_on("a", secs(60));
    f.lease_on("a", secs(60));
    f.worker("b");
    f.lease_on("b", secs(3_600));
}

/// Catches: the pick without settings by the fewest leases: `b` has one lease that runs
/// an hour, `a` two that end within a minute, and `a`, which empties first, is held.
#[test]
fn without_settings_the_worker_that_empties_first_is_held() {
    let mut f = plain();
    one_long_two_short(&mut f);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the fewest-leases order lost when no lease has a wall time: no worker's tail
/// is known, so the one with the fewest leases is held.
#[test]
fn with_no_wall_times_known_the_fewest_leases_is_held() {
    let mut f = plain();
    f.worker("a");
    f.place_on("a", None);
    f.place_on("a", None);
    f.worker("b");
    f.place_on("b", None);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: an action that owns its worker picked by the fewest leases without settings:
/// it holds the worker that empties first, `a`.
#[test]
fn without_settings_an_owner_action_holds_the_worker_that_empties_first() {
    let mut f = plain();
    one_long_two_short(&mut f);
    let op = owner(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
}

/// Catches: a hold kept on its worker without settings: `a` holds a lease of a minute and
/// one of an hour, `b` one of ten minutes, so `b` is held; when the hour-long lease ends,
/// `a` empties first and the hold moves there.
#[test]
fn without_settings_the_hold_moves_to_the_worker_that_empties_first() {
    let mut f = plain();
    f.worker("a");
    f.lease_on("a", secs(60));
    let long = f.lease_on("a", secs(3_600));
    f.worker("b");
    f.lease_on("b", secs(600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.finish(&long);
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: no limit on holds per pool without settings: of four owner actions in one
/// pool only the first in queue order holds a worker.
#[test]
fn without_settings_four_owner_actions_in_one_pool_hold_one_worker() {
    let mut f = plain();
    for name in ["a", "b", "c", "d"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let ops: Vec<OperationId> = (0..4).map(|_| owner(&mut f)).collect();
    let held: Vec<bool> = ops.iter().map(|op| f.held_for(*op).is_some()).collect();
    assert_eq!(held, [true, false, false, false]);
}

/// Catches: no limit on holds per pool without settings: of two whole-machine requests
/// the first holds `a` and the second holds nothing; once the first is placed, the second
/// holds the worker left.
#[test]
fn without_settings_two_whole_machine_requests_in_one_pool_hold_one_worker() {
    let mut f = plain();
    f.worker("a");
    let short = f.lease_on("a", secs(300));
    f.worker("b");
    f.lease_on("b", secs(600));
    let first = f.whole();
    let second = f.whole();
    assert_eq!(f.held_for(first), Some(&w("a")));
    assert_eq!(f.held_for(second), None);
    f.finish(&short);
    let grants = f.grants();
    let granted: Vec<_> = grants.iter().map(|g| (g.operation, &g.worker)).collect();
    assert_eq!(granted, [(first, &w("a"))]);
    assert_eq!(f.held_for(second), Some(&w("b")));
}

/// Catches: with settings, an action that owns its worker still picked by the fewest
/// leases: it holds the worker that empties first, `a`.
#[test]
fn with_settings_an_owner_action_holds_the_worker_that_empties_first() {
    let mut f = with_settings(4);
    one_long_two_short(&mut f);
    let op = owner(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
}

/// Catches: with settings, an owner action held on a worker whose lease is past its bound
/// as though it were empty. At 100 s `a`'s lease of 60 s still runs and reads as ending a
/// minute from now; `b`'s ends 30 s from now, so `b` is held.
#[test]
fn with_settings_an_owner_action_does_not_take_a_worker_whose_lease_is_late() {
    let mut f = with_settings(4);
    f.worker("a");
    f.lease_on("a", secs(60));
    f.worker("b");
    assert!(f.advance(100_000).is_empty());
    f.lease_on("b", secs(30));
    let op = owner(&mut f);
    assert_eq!(f.held_for(op), Some(&w("b")));
}

/// Catches: with settings, four owner actions in one pool each holding a worker: one
/// hold per pool, the first in queue order.
#[test]
fn with_settings_four_owner_actions_in_one_pool_hold_one_worker() {
    let mut f = with_settings(4);
    for name in ["a", "b", "c", "d"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let ops: Vec<OperationId> = (0..4).map(|_| owner(&mut f)).collect();
    let held: Vec<bool> = ops.iter().map(|op| f.held_for(*op).is_some()).collect();
    assert_eq!(held, [true, false, false, false]);
}

/// Catches: an action that owns its worker counted by the whole-node caps or dropped as
/// a mark over the ceiling: at `max_whole_node = 1` a whole-machine request marks `a`,
/// and an owner action after it still holds `b`, round after round, while the mark stays
/// as it was made.
#[test]
fn an_owner_action_is_outside_the_whole_node_caps() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(1)),
    );
    for name in ["a", "b"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let whole = f.whole();
    let made = f.s.whole_mark(whole).cloned().expect("marked");
    assert_eq!(made.worker, w("a"));
    let op = owner(&mut f);
    for ms in [10_000, 20_000, 30_000] {
        f.at(ms);
        f.beat();
        assert!(f.grants().is_empty());
        assert_eq!(f.held_for(op), Some(&w("b")));
        assert_eq!(f.s.whole_mark(whole), Some(&made));
    }
}

/// Catches: a mark picked again when another worker becomes cheaper: `a` holds two leases
/// of a minute, `b` an hour-long one and a lease of 30 s; the request marks `a`, and when
/// the hour-long lease ends `b` is the cheaper to drain, yet the mark stays on `a` exactly
/// as it was made (its time, its deadline and its tail end).
#[test]
fn the_mark_stays_on_its_worker_when_another_becomes_cheaper() {
    let mut f = with_settings(1);
    f.worker("a");
    f.lease_on("a", secs(60));
    f.lease_on("a", secs(60));
    f.worker("b");
    let long = f.lease_on("b", secs(3_600));
    f.lease_on("b", secs(30));
    f.at(5_000);
    f.beat();
    let whole = f.whole();
    let made = f.s.whole_mark(whole).cloned().expect("marked");
    assert_eq!(made.worker, w("a"));
    f.finish(&long);
    f.at(10_000);
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(f.s.whole_mark(whole), Some(&made));
}

/// Submits a whole-machine request at `qos`, ahead of every `Ci` request in queue order,
/// and ticks: the operation.
fn urgent_whole(f: &mut Farm) -> OperationId {
    f.next_n += 1;
    let operation = f.s.next_operation();
    let request = Request {
        qos: Qos::Interactive,
        ..request(f.next_n, LeaseKind::WholeMachine)
    };
    f.submit(request);
    assert!(f.grants().is_empty(), "no worker is empty");
    operation
}

/// `a` holds a lease of a minute, `b` one of ten.
fn short_and_long(f: &mut Farm) -> LeaseGrant {
    f.worker("a");
    let short = f.lease_on("a", secs(60));
    f.worker("b");
    f.lease_on("b", secs(600));
    short
}

/// Catches: a mark taken by a more urgent request: the first request marks `a`, the
/// cheaper to drain; a more urgent one, reached first in every later round, marks `b`
/// and leaves `a` to the first, which keeps its mark as it was made.
#[test]
fn a_more_urgent_request_does_not_take_a_mark() {
    let mut f = with_settings(2);
    short_and_long(&mut f);
    let first = f.whole();
    let made = f.s.whole_mark(first).cloned().expect("marked");
    let urgent = urgent_whole(&mut f);
    assert_eq!(f.held_for(urgent), Some(&w("b")));
    assert_eq!(f.held_for(first), Some(&w("a")));
    assert_eq!(f.s.whole_mark(first), Some(&made));
}

/// Catches: a marked worker that empties granted to a more urgent request: the first
/// request marks `a`; `a` empties as a more urgent request arrives, and the round that
/// sees both grants `a` to the first, while the more urgent one marks `b`.
#[test]
fn a_marked_worker_that_empties_goes_to_its_own_request() {
    let mut f = with_settings(2);
    let short = short_and_long(&mut f);
    let first = f.whole();
    assert_eq!(f.held_for(first), Some(&w("a")));
    f.finish(&short);
    f.next_n += 1;
    let urgent = f.s.next_operation();
    f.submit(Request {
        qos: Qos::Interactive,
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    let grants = f.grants();
    let granted: Vec<_> = grants.iter().map(|g| (g.operation, &g.worker)).collect();
    assert_eq!(granted, [(first, &w("a"))]);
    assert_eq!(f.held_for(urgent), Some(&w("b")));
}

/// Catches: a mark taken by the hold of a more urgent action that owns its worker: the
/// request marks `a`, the cheaper to drain; the action, reached first, holds `b`.
#[test]
fn an_owner_action_does_not_take_a_mark() {
    let mut f = with_settings(2);
    short_and_long(&mut f);
    let whole = f.whole();
    f.next_n += 1;
    let action = f.s.next_operation();
    f.submit(Request {
        qos: Qos::Interactive,
        whole_worker: true,
        pin: None,
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty(), "no worker is empty");
    assert_eq!(f.held_for(action), Some(&w("b")));
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: a tie flapping: two workers end together and the hold stays on the first by
/// name across rounds.
#[test]
fn a_tie_stays_on_the_lower_name() {
    let mut f = with_settings(1);
    f.worker("a");
    f.lease_on("a", secs(600));
    f.worker("b");
    f.lease_on("b", secs(600));
    let whole = f.whole();
    for ms in [10_000, 20_000, 30_000] {
        f.at(ms);
        f.beat();
        assert!(f.grants().is_empty());
        assert_eq!(f.held_for(whole), Some(&w("a")));
    }
}

/// Catches: a hold lost at the cap by a pick made every round: at `max_draining = 1` the
/// request keeps its hold on the same worker across rounds, and holds it still.
#[test]
fn at_max_draining_one_the_hold_survives_every_round() {
    let mut f = with_settings(1);
    f.worker("a");
    f.lease_on("a", secs(600));
    let whole = f.whole();
    for ms in [10_000, 20_000, 30_000, 40_000] {
        f.at(ms);
        f.beat();
        assert!(f.grants().is_empty());
        assert_eq!(f.held_for(whole), Some(&w("a")));
        assert!(f.s.whole_mark(whole).is_some());
    }
}

/// Catches: a mark made on a worker a whole-machine lease holds (the request would wait
/// behind that lease, which is never touched, and the caps would count the worker twice,
/// held and marked). `a` holds a whole-machine lease that ends in a minute, `b` an
/// ordinary lease of an hour: the next whole-machine request marks `b`, though `a`'s
/// tail is the shorter.
#[test]
fn a_worker_a_whole_machine_lease_holds_is_never_marked() {
    let mut f = with_settings(2);
    f.worker("a");
    let first = f.submit_whole(Some(secs(60)));
    let grants = f.grants();
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!((grants[0].operation, &grants[0].worker), (first, &w("a")));
    f.start(&grants[0]);
    f.worker("b");
    f.lease_on("b", secs(3_600));
    let second = f.whole();
    assert_eq!(f.held_for(second), Some(&w("b")));
}

/// Catches: a mark made on a worker a whole-machine request was placed on earlier in the
/// same round (the two would hold one worker). `a` is empty and `b` holds an ordinary
/// lease of an hour; two whole-machine requests are submitted before one round: the first
/// is placed on `a`, and the second marks `b`, though `a`'s tail (the first's minute) is
/// the shorter.
#[test]
fn a_worker_a_whole_machine_request_was_placed_on_this_round_is_not_marked() {
    let mut f = with_settings(2);
    f.worker("a");
    f.worker("b");
    f.lease_on("b", secs(3_600));
    let first = f.submit_whole(Some(secs(60)));
    let second = f.submit_whole(Some(secs(60)));
    let grants = f.grants();
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!((grants[0].operation, &grants[0].worker), (first, &w("a")));
    assert_eq!(f.held_for(second), Some(&w("b")));
}
