//! Which worker a request that waits for a worker to empty holds. With whole-node
//! settings an action that owns its worker takes one hold per pool, by the least tail,
//! made again every round; a whole-machine hold is the drain order's, kept while it could
//! run the request. Without settings nothing is changed: the worker with the fewest leases, kept while it
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
use js_types::sched::{LeaseKind, OperationId};
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
