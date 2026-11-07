//! Whole holders ([`Scheduler::set_whole_holder`]): an operation of another kind than
//! whole-machine the caller names as holding its worker whole, as a whole-node lease placed
//! as an action that owns its worker is. Its worker is whole already while the lease holds
//! it: it is never marked for a whole-machine request, and the caps count it once, as held.
//! Every time is farm time fed to the scheduler; nothing waits.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_sched::{Scheduler, WholeWait};
use js_types::sched::{LeaseGrant, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::Blocked;

use crate::whole_rig::{Farm, secs};

/// Whole-node settings with room everywhere, `max_whole_node` as given.
fn cfg(max_whole_node: usize) -> WholeNodeConfig {
    WholeNodeConfig {
        max_draining: 9,
        drain_deadline_ms: Some(3_600_000),
        max_whole_node,
        ..WholeNodeConfig::dedicated(9)
    }
}

/// A farm of `a` and `b`, each running one action that may run ten minutes; the action on
/// `holder`, if one is named, a whole holder.
fn busy(max_whole_node: usize, holder: Option<&str>) -> (Farm, Vec<LeaseGrant>) {
    let s = Scheduler::new(1)
        .with_load_config(booked::current())
        .with_whole_node(cfg(max_whole_node));
    let mut f = Farm::new(s);
    let mut grants = Vec::new();
    for name in ["a", "b"] {
        f.worker(name);
        let grant = f.lease_on(name, secs(600));
        if holder == Some(name) {
            f.s.set_whole_holder(grant.operation);
        }
        grants.push(grant);
    }
    (f, grants)
}

/// Asks for a whole machine that may run ten minutes, ten seconds on, and runs a round.
fn ask(f: &mut Farm) -> OperationId {
    let now = f.now;
    f.advance(now + 10_000);
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty(), "no worker is empty");
    op
}

fn marked(f: &Farm, op: OperationId) -> Option<WorkerId> {
    f.s.whole_mark(op).map(|mark| mark.worker.clone())
}

both_rules! {
/// Catches: a worker a whole holder holds taken for a worker a whole-machine request may
/// drain. With the two leases alike the request marks `a`, the first by name; once `a`'s
/// lease is named a whole holder it marks `b`, and with both named it marks neither and
/// holds no worker.
fn a_worker_a_whole_holder_holds_is_never_marked() {
    let (mut plain, _) = busy(9, None);
    let op = ask(&mut plain);
    assert_eq!(marked(&plain, op), Some(WorkerId::new("a")));
    let (mut held, _) = busy(9, Some("a"));
    let op = ask(&mut held);
    assert_eq!(marked(&held, op), Some(WorkerId::new("b")));
    let (mut both, grants) = busy(9, Some("a"));
    both.s.set_whole_holder(grants[1].operation);
    let op = ask(&mut both);
    assert_eq!(marked(&both, op), None);
    assert_eq!(both.held_for(op), None);
}
}

both_rules! {
/// Catches: a whole holder not counted by the caps: with one whole worker allowed and `a`
/// held by a whole holder, the request is stopped by the ceiling though `b` could be
/// drained.
fn a_whole_holder_counts_toward_max_whole_node() {
    let (mut f, _) = busy(1, Some("a"));
    let op = ask(&mut f);
    assert_eq!(f.s.whole_wait(op), Some(WholeWait::Cap(Blocked::Ceiling)));
    assert_eq!(marked(&f, op), None);
}
}

both_rules! {
/// Catches: the standing outliving the lease (the worker would stay whole with nothing on
/// it). Both workers are held by whole holders; once `a`'s lease ends, `a` is empty and is
/// granted to the request.
fn a_whole_holders_worker_is_free_once_its_lease_ends() {
    let (mut f, grants) = busy(9, Some("a"));
    f.s.set_whole_holder(grants[1].operation);
    let op = ask(&mut f);
    f.finish(&grants[0]);
    let now = f.now + 1_000;
    let granted = f.advance(now);
    assert_eq!(granted.len(), 1);
    assert_eq!((granted[0].operation, &granted[0].worker), (op, &WorkerId::new("a")));
}
}
