//! Which worker a request that waits for a worker to empty holds. With whole-node
//! settings the pick is the least tail: an action that owns its worker takes one hold per
//! pool, picked again every round, and a whole-machine mark stays on the worker it was
//! made on until its request is placed, cancelled or gives it up, however cheap another
//! worker becomes, and no other request's hold, mark or whole-machine placement takes
//! that worker (ordinary work is still placed there).
//! Without settings nothing is changed: the worker with the fewest leases, kept while it
//! could run the request, and no limit on how many requests hold a worker. Every time is
//! farm time fed to the scheduler.

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

/// Catches (off is main): the pick without settings changed from the fewest leases: `b`
/// has one lease that runs an hour, `a` two that end within a minute, and `b` is held.
#[test]
fn without_settings_the_fewest_leases_worker_is_held() {
    let mut f = plain();
    one_long_two_short(&mut f);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches (off is main): the fewest-leases fallback lost when no lease has a wall time.
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

/// Catches (off is main): an action that owns its worker picked by tail without settings:
/// it holds the worker with the fewest leases, `b`.
#[test]
fn without_settings_an_owner_action_holds_the_fewest_leases_worker() {
    let mut f = plain();
    one_long_two_short(&mut f);
    let op = owner(&mut f);
    assert_eq!(f.held_for(op), Some(&w("b")));
}

/// Catches (off is main): a hold that moves without settings: `b` is held for having the
/// fewest leases; when `a` is down to one lease too, `b` is still held (kept), though `a`
/// would now empty first.
#[test]
fn without_settings_the_hold_is_kept() {
    let mut f = plain();
    f.worker("a");
    let first = f.lease_on("a", secs(60));
    f.lease_on("a", secs(60));
    f.worker("b");
    f.lease_on("b", secs(600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.finish(&first);
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches (off is main): a limit of one hold per pool without settings: four owner
/// actions in one pool each hold a worker.
#[test]
fn without_settings_four_owner_actions_each_hold_a_worker() {
    let mut f = plain();
    for name in ["a", "b", "c", "d"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let ops: Vec<OperationId> = (0..4).map(|_| owner(&mut f)).collect();
    let held: Vec<bool> = ops.iter().map(|op| f.held_for(*op).is_some()).collect();
    assert_eq!(held, [true, true, true, true]);
}

/// Catches (off is main): a limit of one hold per pool without settings: a second
/// whole-machine request holds the next worker.
#[test]
fn without_settings_two_whole_machine_requests_each_hold_a_worker() {
    let mut f = plain();
    for name in ["a", "b"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let first = f.whole();
    let second = f.whole();
    assert_eq!(f.held_for(first), Some(&w("a")));
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
