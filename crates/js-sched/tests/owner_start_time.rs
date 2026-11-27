//! When a request that needs a whole worker can start there: once the worker holds no
//! lease, so its start time on a worker is the latest end of the worker's leases, the
//! drain order's tail. That holds for an action that owns its worker as for a
//! whole-machine request: neither needs only part of a worker, so neither is held by when
//! enough of a worker's cores free up, the start time of a blocked head
//! (`head_start_time.rs`). Every time is farm time fed to the scheduler.

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
use js_types::sched::{LeaseGrant, LeaseKind, OperationId, WorkerId};
use js_whole::WholeNodeConfig;

fn with_settings() -> Farm {
    Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(WholeNodeConfig::dedicated(4)),
    )
}

/// Two 8-core workers. `a` runs seven one-core leases that all end at 200 s, so it is
/// nearly full but empties first. `b` runs two: one ends at 100 s, the other at 900 s, so
/// it has the cores now and frees more at 100 s, but empties last. The grants of `a`'s
/// leases, then `b`'s short one.
fn full_but_short_and_free_but_long(f: &mut Farm) -> (Vec<LeaseGrant>, LeaseGrant) {
    f.worker("a");
    f.worker("b");
    let on_a = (0..7).map(|_| f.lease_on("a", secs(200))).collect();
    let short = f.lease_on("b", secs(100));
    f.lease_on("b", secs(900));
    (on_a, short)
}

/// Runs the farm to 100 s, when `b`'s short lease ends, then to 200 s, when `a`'s end:
/// whether `operation` was granted at 100 s, and the worker it is granted at 200 s.
fn run_to_a_empty(
    f: &mut Farm,
    (on_a, short): &(Vec<LeaseGrant>, LeaseGrant),
    operation: OperationId,
) -> (bool, Option<WorkerId>) {
    let mut early = f.advance(100_000);
    f.finish(short);
    early.extend(f.grants());
    let granted_early = early.iter().any(|g| g.operation == operation);
    let mut late = f.advance(200_000);
    for grant in on_a {
        f.finish(grant);
    }
    late.extend(f.grants());
    let granted = late
        .iter()
        .find(|g| g.operation == operation)
        .map(|g| g.worker.clone());
    (granted_early, granted)
}

/// Catches: an action that owns its worker held, as a blocked head is, by when its share
/// of cores fits: `b` has the cores free now and more at 100 s, but still runs a lease
/// until 900 s. The action runs only on a worker with no lease, so it holds `a`, which
/// empties first; it is not started on `b` at 100 s, and it starts on `a` at 200 s.
#[test]
fn an_owner_action_holds_the_worker_that_empties_first_not_the_one_with_free_cores() {
    let mut f = with_settings();
    let layout = full_but_short_and_free_but_long(&mut f);
    f.next_n += 1;
    let owner = f.s.next_operation();
    f.submit(Request {
        whole_worker: true,
        pin: None,
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty(), "no worker is empty");
    assert_eq!(f.held_for(owner), Some(&w("a")));
    let (granted_early, granted) = run_to_a_empty(&mut f, &layout, owner);
    assert!(!granted_early, "b still runs a lease at 100 s");
    assert_eq!(granted, Some(w("a")));
}

/// Catches: a whole-machine request's start time taken as anything but the latest end of
/// the worker's leases: in the same layout it marks `a`, is not started at 100 s, and
/// starts on `a` when it empties at 200 s.
#[test]
fn a_whole_machine_request_starts_when_the_worker_empties() {
    let mut f = with_settings();
    let layout = full_but_short_and_free_but_long(&mut f);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    let (granted_early, granted) = run_to_a_empty(&mut f, &layout, whole);
    assert!(!granted_early, "no worker is empty at 100 s");
    assert_eq!(granted, Some(w("a")));
}
