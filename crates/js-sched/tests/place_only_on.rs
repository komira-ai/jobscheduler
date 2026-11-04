//! Holding an operation to a set of workers (`Scheduler::place_only_on`) and ending a
//! hold put on a worker from outside (`Scheduler::release_outside`): the two ways a
//! caller that decides which hosts a start may use (the lease server's registry) keeps a
//! long-lived scheduler's choice inside that decision.

#[path = "load_rig/mod.rs"]
mod load_rig;

use std::collections::BTreeSet;

use js_sched::OpState;
use js_types::sched::{OperationId, Resources, WorkerId};

use load_rig::{GIB, Rig, w};

/// A rig with the equal workers `names`, each of 8 cores and 16 GiB.
fn rig(names: &[&str]) -> Rig {
    let mut r = Rig::on();
    for name in names {
        r.up(name, Resources::new(8_000, 16 * GIB));
    }
    r
}

/// The workers `names`.
fn set(names: &[&str]) -> BTreeSet<WorkerId> {
    names.iter().map(|name| w(name)).collect()
}

/// The worker each lease the round started is on.
fn placed(r: &mut Rig) -> Vec<String> {
    let starts = r.tick();
    starts
        .iter()
        .map(|start| start.worker.as_str().to_owned())
        .collect()
}

/// Whether `op` holds a lease.
fn leased(r: &Rig, op: OperationId) -> bool {
    matches!(r.s.state(op), Some(OpState::Leased { .. }))
}

/// A one-core action of a key of its own.
fn action(r: &mut Rig) -> OperationId {
    r.action(1_000, GIB, None)
}

/// Catches: the set not read by placement, or read as the workers to avoid. Over three
/// equal workers, an operation held to one of them goes there, whichever it is; one held
/// to two goes to one of the two.
#[test]
fn an_operation_held_to_workers_is_placed_on_none_but_them() {
    for only in ["a", "b", "c"] {
        let mut r = rig(&["a", "b", "c"]);
        let op = action(&mut r);
        r.s.place_only_on(op, set(&[only]));
        assert_eq!(placed(&mut r), [only], "held to {only}");
    }
    let mut r = rig(&["a", "b", "c"]);
    let op = action(&mut r);
    r.s.place_only_on(op, set(&["a", "c"]));
    let on = placed(&mut r);
    assert_eq!(on.len(), 1);
    assert_ne!(on[0], "b");
}

/// Catches: an empty set read as no set (the operation placed anywhere), a second call
/// that adds to the first instead of replacing it, and a set that leaks to another
/// operation. Held to none, an operation stays queued round after round while one with
/// no set beside it is placed; held again to `b`, it goes to `b` and nowhere else.
#[test]
fn an_empty_set_places_nowhere_and_a_second_call_replaces_the_first() {
    let mut r = rig(&["a", "b"]);
    let held = action(&mut r);
    r.s.place_only_on(held, BTreeSet::new());
    assert!(placed(&mut r).is_empty());
    assert!(r.round(5_000).is_empty());
    assert_eq!(r.s.queued_len(), 1);
    let free = action(&mut r);
    assert_eq!(placed(&mut r).len(), 1);
    assert!(leased(&r, free));
    r.s.place_only_on(held, set(&["a"]));
    r.s.place_only_on(held, set(&["b"]));
    assert_eq!(placed(&mut r), ["b"]);
    assert_eq!(r.s.queued_len(), 0);
}

/// Catches: a set kept after its operation is forgotten as queued. The operation held to
/// none is forgotten, and the next one, which has no set, is placed.
#[test]
fn the_set_goes_with_the_operation() {
    let mut r = rig(&["a"]);
    let held = action(&mut r);
    r.s.place_only_on(held, BTreeSet::new());
    assert!(placed(&mut r).is_empty());
    assert!(r.s.forget_queued(held).is_some());
    let next = action(&mut r);
    assert_eq!(placed(&mut r), ["a"]);
    assert!(leased(&r, next));
}

/// Catches: a hold from outside that cannot be ended (a long-lived scheduler would keep
/// every worker a mark ever set aside) and a release that ends the holds of other
/// workers. With `a` held outside, work goes to `b`; with both held, nowhere; released,
/// `a` takes work again while `b` stays held. Releasing a worker never held changes
/// nothing.
#[test]
fn a_worker_held_outside_takes_work_again_once_released() {
    let mut r = rig(&["a", "b"]);
    r.s.reserve_outside(w("a"), None);
    let op = action(&mut r);
    r.s.place_only_on(op, set(&["a", "b"]));
    assert_eq!(placed(&mut r), ["b"]);
    let mut r = rig(&["a", "b"]);
    r.s.reserve_outside(w("a"), None);
    r.s.reserve_outside(w("b"), None);
    let op = action(&mut r);
    assert!(placed(&mut r).is_empty());
    r.s.release_outside(&w("c"));
    assert!(placed(&mut r).is_empty());
    r.s.release_outside(&w("a"));
    assert_eq!(placed(&mut r), ["a"]);
    assert!(leased(&r, op));
}
