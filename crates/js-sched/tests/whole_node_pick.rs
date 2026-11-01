//! The one pick for a request that holds a worker while it waits: the least tail, made
//! again every round, for a whole-machine request with or without whole-node settings and
//! for an action that owns its worker; one such hold per pool where there are no
//! whole-node settings. Every time is farm time fed to the scheduler.

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

/// Catches: the pick without settings still by the fewest leases: `a` holds two leases
/// that end within a minute and `b` one that runs an hour, so `a` empties first and is
/// held, though `b` has fewer leases.
#[test]
fn without_settings_the_worker_that_empties_first_is_held() {
    let mut f = plain();
    f.worker("a");
    f.lease_on("a", secs(60));
    f.lease_on("a", secs(60));
    f.worker("b");
    f.lease_on("b", secs(3_600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the fallback lost when no lease has a wall time: every tail is unknown, so the
/// order is the fewest leases, then the name, as before.
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

/// Catches: an action that owns its worker still picked by the fewest leases: the same
/// layout holds `a`, the worker that empties first.
#[test]
fn an_action_that_owns_its_worker_holds_the_worker_that_empties_first() {
    let mut f = plain();
    f.worker("a");
    f.lease_on("a", secs(60));
    f.lease_on("a", secs(60));
    f.worker("b");
    f.lease_on("b", secs(3_600));
    let op = owner(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
}

/// Catches: a hold that stays where it was put: `a` holds two leases of a minute, `b` an
/// hour-long one and a lease of 30 s; the request holds `a`, and when the hour-long lease
/// ends `b` is the cheaper and the hold moves there, keeping the time it was made.
#[test]
fn the_hold_moves_to_the_cheaper_worker_and_keeps_its_made_time() {
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
    let made = f.s.whole_mark(whole).expect("marked").made;
    f.finish(&long);
    f.at(10_000);
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
    let mark = f.s.whole_mark(whole).expect("still marked");
    assert_eq!((mark.worker.clone(), mark.made), (w("b"), made));
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

/// Catches: four queued owner-worker actions each holding a worker: one hold per pool,
/// the first in queue order.
#[test]
fn four_owner_actions_in_one_pool_hold_one_worker() {
    let mut f = plain();
    for name in ["a", "b", "c", "d"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let ops: Vec<OperationId> = (0..4).map(|_| owner(&mut f)).collect();
    let held: Vec<bool> = ops.iter().map(|op| f.held_for(*op).is_some()).collect();
    assert_eq!(held, [true, false, false, false]);
}

/// Catches: two whole-machine requests without settings each holding a worker in one
/// pool: the first holds one, the second waits for the next round after the first is
/// placed.
#[test]
fn two_whole_machine_requests_without_settings_hold_one_worker() {
    let mut f = plain();
    for name in ["a", "b"] {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    let first = f.whole();
    let second = f.whole();
    assert_eq!(f.held_for(first), Some(&w("a")));
    assert_eq!(f.held_for(second), None);
}
