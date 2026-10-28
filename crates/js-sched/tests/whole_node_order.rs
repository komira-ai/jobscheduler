//! Which worker a blocked whole-machine request holds: by leases and booking alone, as a
//! scheduler built with [`Scheduler::new`] does, or by drain cost once it is given
//! whole-node settings ([`Scheduler::with_whole_node`]). Every time here is farm time fed
//! to the scheduler; the longest a lease may run is a value handed to it, never waited for.

#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use crate::whole_rig::{Farm, request, secs, w};
use js_sched::{Recovered, RecoveredLease, Scheduler};
use js_types::sched::{LeaseId, LeaseKind, OperationId, WaiterId};
use js_whole::WholeNodeConfig;

fn whole_node() -> WholeNodeConfig {
    WholeNodeConfig::dedicated(4)
}

/// `a` holds two leases that may run a minute each, `b` one that may run an hour.
fn one_long_two_short(s: Scheduler) -> Farm {
    let mut f = Farm::new(s);
    f.worker("a");
    f.lease(secs(60));
    f.lease(secs(60));
    f.worker("b");
    f.lease(secs(3_600));
    assert_eq!(f.s.leases_held(), 3);
    f
}

/// Catches: the choice of the held worker changed for a scheduler without whole-node
/// settings: it holds the one with the fewest leases (`b`, one lease), whatever its
/// leases are still to run.
#[test]
fn without_settings_the_worker_with_fewest_leases_is_held() {
    let mut f = one_long_two_short(Scheduler::new(1).with_load_config(booked::booked()));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: the reservation ignoring how long a worker takes to empty: `b` has one lease
/// but it runs an hour, `a` has two that end within a minute, and the drain-cost order
/// holds `a`.
#[test]
fn with_settings_the_worker_cheapest_to_drain_is_held() {
    let mut f = one_long_two_short(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: leases whose operation was given no wall time costed as ending at once: they
/// are taken to end last, so the worker with only known short leases is held; and with
/// every wall unknown the order falls back to the fewest leases.
#[test]
fn a_lease_with_no_wall_time_drains_last() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("a");
    f.lease(secs(60));
    f.worker("b");
    f.next_n += 1;
    f.submit(request(f.next_n, LeaseKind::Action));
    let unbounded = f.grants().pop().expect("placed");
    f.start(&unbounded);
    assert_eq!(unbounded.worker, w("b"));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the stickiness of a hold lost under the drain-cost order: `b`'s hour-long
/// lease ends and what is left on it ends in 30 s, cheaper to drain than the held `a`
/// (60 s), and the hold stays on `a`.
#[test]
fn the_held_worker_is_kept_while_another_becomes_cheaper() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("a");
    f.lease(secs(60));
    f.lease(secs(60));
    f.worker("b");
    let long = f.lease(secs(3_600));
    let short = f.lease(secs(30));
    assert_eq!(short.worker, w("b"));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.finish(&long);
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: a worker held for one whole request offered to the next: the second request
/// holds the next worker in drain order.
#[test]
fn a_second_request_holds_the_next_cheapest_worker() {
    let mut f = one_long_two_short(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    let first = f.whole();
    let second = f.whole();
    assert_eq!(f.held_for(first), Some(&w("a")));
    assert_eq!(f.held_for(second), Some(&w("b")));
}

/// Catches: an end bound taken from the grant or from the wall alone instead of from when
/// the lease started: `b`'s lease started first and ends at 100 s, `a`'s started four
/// seconds later and ends at 102 s though its wall time is shorter; the wall alone would
/// hold `a`.
#[test]
fn the_end_bound_is_the_start_plus_the_wall_time() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("b");
    f.lease(secs(100));
    f.at(4_000);
    f.worker("a");
    f.lease(secs(98));
    f.at(5_000);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: an end bound counted from the grant instead of from the report that the run
/// started: `b`'s lease was granted at once but started three seconds on, so it ends at
/// 103 s, after `a`'s at 102 s; counted from its grant it would end first.
#[test]
fn the_bound_counts_from_the_start_report_not_the_grant() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("b");
    f.lease_after(secs(100), 3_000);
    f.at(4_000);
    f.worker("a");
    f.lease(secs(98));
    f.at(5_000);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the lease count left out of the order when two workers end together: `a`
/// holds two leases and `b` one, all ending at 60 s, and the one with fewer is held.
#[test]
fn equal_bounds_hold_the_worker_with_fewer_leases() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("a");
    f.lease(secs(60));
    f.lease(secs(60));
    f.worker("b");
    f.lease(secs(60));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: the bound of a lease that has ended still counted: `a`'s long lease ends, so
/// `a` (one lease of a minute) is cheaper than `b` (two of two minutes); with the bound
/// kept `a` would cost an hour and `b` would be held.
#[test]
fn an_ended_lease_no_longer_counts() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("a");
    f.lease(secs(60));
    let long = f.lease(secs(3_600));
    f.worker("b");
    f.lease(secs(120));
    f.lease(secs(120));
    f.finish(&long);
    f.at(1_000);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: a lease granted in the same round as the reservation left out of the drain
/// cost: `a` holds two leases of 90 s and `b` one of 60 s; an action that may run an hour
/// is placed on `b` in the round that holds a worker for the whole-machine request, so
/// `b` is the dearer and `a` is held.
#[test]
fn a_lease_placed_in_the_round_counts() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.worker("a");
    f.lease(secs(90));
    f.lease(secs(90));
    f.worker("b");
    f.lease(secs(60));
    f.next_n += 1;
    let action = f.s.next_operation();
    f.s.set_max_wall(action, secs(3_600));
    f.submit(request(f.next_n, LeaseKind::Action));
    f.next_n += 1;
    let whole = f.s.next_operation();
    f.submit(request(f.next_n, LeaseKind::WholeMachine));
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, action);
    assert_eq!(grants[0].worker, w("b"));
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

fn recovered(operation: u64, worker: &str, seq: u64) -> Recovered {
    Recovered {
        operation: OperationId(operation),
        request: request(u8::try_from(operation).unwrap(), LeaseKind::Action),
        waiter: WaiterId(operation),
        lease: Some(RecoveredLease {
            lease: LeaseId::new(7, seq),
            worker: w(worker),
            cost: None,
        }),
        losses: 0,
        fate: None,
    }
}

/// Catches: end bounds lost on recovery, or counted from before it: the leases come back
/// started at the recovery, so with the wall times set again `a` (two leases of a minute)
/// is cheaper to drain than `b` (one of an hour), where the fewest-leases rule holds `b`.
#[test]
fn recovered_leases_end_a_wall_time_after_the_recovery() {
    let mut s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(whole_node());
    s.recover(
        7,
        OperationId(10),
        20,
        vec![
            recovered(3, "a", 5),
            recovered(4, "a", 6),
            recovered(5, "b", 7),
        ],
    );
    s.set_max_wall(OperationId(3), secs(60));
    s.set_max_wall(OperationId(4), secs(60));
    s.set_max_wall(OperationId(5), secs(3_600));
    let mut f = Farm::new(s);
    f.now = 1_000;
    f.worker("a");
    f.worker("b");
    f.next_n = 50;
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: an end bound that overflows: a lease whose wall time is more than a
/// millisecond count can hold, started after farm time zero, ends at the latest time there
/// is, so the worker holding it drains last, and no sum wraps or panics.
#[test]
fn an_unrepresentable_wall_time_saturates_to_the_latest_end() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(whole_node()),
    );
    f.at(5_000);
    f.worker("a");
    f.lease(std::time::Duration::MAX);
    f.worker("b");
    f.lease(secs(86_400));
    f.lease(secs(86_400));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}
