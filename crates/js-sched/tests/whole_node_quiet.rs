//! The caps on a whole-machine request that takes a quiet worker at once
//! ([`Scheduler::with_whole_node`]): the take adds one whole worker and no draining one,
//! so the ceiling and `keep_for_ordinary` apply to it and `max_draining` does not. A
//! request the caps stop stays queued and [`Scheduler::whole_wait`] says why. Also how workers of every
//! platform count toward one pool. Every time is farm time fed to the scheduler.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Request, Scheduler, WholeWait};
use js_types::sched::{LeaseKind, NodePhase, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::Blocked;

use crate::whole_rig::{CAPACITY, Farm, request, secs, w};

fn cfg(edit: impl FnOnce(&mut WholeNodeConfig)) -> WholeNodeConfig {
    let mut c = WholeNodeConfig {
        max_draining: 9,
        drain_deadline_ms: Some(3_600_000),
        ..WholeNodeConfig::dedicated(9)
    };
    edit(&mut c);
    c
}

/// A farm of quiet workers `names` (no lease on any).
fn quiet(config: WholeNodeConfig, names: &[&str]) -> Farm {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(config));
    for name in names {
        f.worker(name);
    }
    f
}

/// Submits a whole-machine request and runs a round: the worker it is granted, if any.
fn ask(f: &mut Farm) -> Option<WorkerId> {
    let op = f.submit_whole(Some(secs(600)));
    let grants = f.grants();
    let grant = grants.into_iter().find(|g| g.operation == op)?;
    f.start(&grant);
    Some(grant.worker)
}

/// The operation of the last request `ask` made.
fn last(f: &Farm) -> OperationId {
    OperationId(f.s.next_operation().0 - 1)
}

/// Catches: a quiet worker taken past `max_whole_node`: with one as the cap, the first
/// request is granted `a`, and the second, though `b` is quiet, is not granted and says
/// the ceiling stopped it.
#[test]
fn a_quiet_worker_is_not_taken_at_max_whole_node() {
    let mut f = quiet(cfg(|c| c.max_whole_node = 1), &["a", "b"]);
    assert!(ask(&mut f).is_some());
    assert_eq!(ask(&mut f), None);
    let second = last(&f);
    assert_eq!(
        f.s.whole_wait(second),
        Some(WholeWait::Cap(Blocked::Ceiling))
    );
    assert_eq!(f.held_for(second), None);
    assert_eq!(f.s.whole_mark(second), None);
}

/// Catches: the cap one too low for a quiet take: with two as the cap both requests are
/// granted a worker, and the third is not.
#[test]
fn one_below_max_whole_node_a_quiet_worker_is_taken() {
    let mut f = quiet(cfg(|c| c.max_whole_node = 2), &["a", "b", "c"]);
    let a = ask(&mut f).unwrap();
    let b = ask(&mut f).unwrap();
    assert_ne!(a, b);
    assert_eq!(ask(&mut f), None);
}

/// Catches: a request refused for the ceiling forgotten when room returns: it stays
/// queued, and once the first lease is released it is granted the worker.
#[test]
fn a_refused_quiet_take_is_granted_when_a_whole_lease_ends() {
    let mut f = quiet(cfg(|c| c.max_whole_node = 1), &["a", "b"]);
    let first = f.submit_whole(Some(secs(600)));
    let grant = f.grants().pop().unwrap();
    assert_eq!(grant.operation, first);
    f.start(&grant);
    assert_eq!(ask(&mut f), None);
    f.finish(&grant);
    let at = f.now + 1_000;
    let grants = f.advance(at);
    assert_eq!(grants.len(), 1);
}

/// Catches: a request that holds a mark refused the quiet worker for the ceiling its own
/// mark fills: with one as the cap, the request holds `a` (busy), `b` comes up quiet, and
/// it takes `b` (its mark becomes a lease, still one whole worker).
#[test]
fn a_marked_request_takes_a_quiet_worker_at_the_ceiling() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.max_whole_node = 1)));
    f.worker("a");
    f.lease(secs(600));
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty());
    assert!(f.s.whole_mark(op).is_some());
    f.worker("b");
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].worker, w("b"));
}

/// Catches: `keep_for_ordinary` at or above the capable workers (ceiling zero) leaving a
/// whole request placed or marked: with two workers and two kept, a request is not
/// granted a quiet worker and holds nothing; on busy workers it holds nothing either, and
/// both say the floor stopped it.
#[test]
fn nothing_is_placed_or_marked_when_the_floor_leaves_no_ceiling() {
    for kept in [2, 3] {
        let mut f = quiet(cfg(|c| c.keep_for_ordinary = kept), &["a", "b"]);
        assert_eq!(ask(&mut f), None);
        let op = last(&f);
        assert_eq!(
            f.s.whole_wait(op),
            Some(WholeWait::Cap(Blocked::KeepForOrdinary))
        );
        assert_eq!((f.held_for(op), f.s.whole_mark(op)), (None, None));
    }
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.keep_for_ordinary = 2)));
    for name in ["a", "b"] {
        f.worker(name);
        f.lease(secs(600));
    }
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty());
    assert_eq!(
        f.s.whole_wait(op),
        Some(WholeWait::Cap(Blocked::KeepForOrdinary))
    );
    assert_eq!((f.held_for(op), f.s.whole_mark(op)), (None, None));
}

/// Catches: the ceiling taken from the capable workers when `max_whole_node` is lower:
/// with five workers and two as the cap, the first two requests hold a worker each; two
/// of the three workers they do not hold are quarantined (capable falls to three, the
/// ceiling stays two because the cap binds), both marks stay, and a third request is
/// stopped by the ceiling, not by the floor.
#[test]
fn a_falling_capable_count_does_not_lower_a_ceiling_the_cap_binds() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.max_whole_node = 2)));
    for name in ["a", "b", "c", "d", "e"] {
        f.worker(name);
        f.lease(secs(600));
    }
    let first = f.submit_whole(Some(secs(600)));
    f.advance(10_000);
    let second = f.submit_whole(Some(secs(600)));
    f.advance(20_000);
    let held = [
        f.held_for(first).unwrap().clone(),
        f.held_for(second).unwrap().clone(),
    ];
    let spare = ["a", "b", "c", "d", "e"]
        .map(w)
        .into_iter()
        .filter(|name| !held.contains(name));
    for name in spare.take(2) {
        {
            f.feed(Event::Phase {
                worker: name,
                phase: NodePhase::Quarantined("scan dirty".to_owned()),
            });
        }
    }
    let third = f.submit_whole(Some(secs(600)));
    f.advance(30_000);
    assert!(f.s.whole_mark(first).is_some());
    assert!(f.s.whole_mark(second).is_some());
    assert_eq!(
        f.s.whole_wait(third),
        Some(WholeWait::Cap(Blocked::Ceiling))
    );
}

/// A mac worker, which serves whole machines like the linux ones.
fn mac(f: &mut Farm, name: &str) {
    f.feed(Event::WorkerUp {
        worker: w(name),
        instance: DaemonInstance::new(name),
        predecessor_stopped: false,
        capacity: CAPACITY,
        caps: NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
            .unwrap()
            .with_drivers(["container", "native-whole-machine"]),
    });
    f.workers.push(w(name));
}

/// Catches: the ceiling kept per platform: a pool of one linux and one mac worker, both
/// quiet, with one as the cap: the first request is granted one worker and the second
/// (which only the other platform's worker could run) is stopped by the ceiling.
#[test]
fn workers_of_every_platform_count_toward_one_ceiling() {
    let mut f = quiet(cfg(|c| c.max_whole_node = 1), &["linux"]);
    mac(&mut f, "mac");
    assert_eq!(ask(&mut f), Some(w("linux")));
    f.next_n += 1;
    let on_mac = f.s.next_operation();
    f.submit(Request {
        needs: js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap(),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    assert!(f.grants().is_empty());
    assert_eq!(
        f.s.whole_wait(on_mac),
        Some(WholeWait::Cap(Blocked::Ceiling))
    );
}

/// Catches: `max_draining` kept per platform: with one draining slot, a request for the
/// busy linux worker holds it, and a request for the busy mac worker is stopped by
/// `max_draining`.
#[test]
fn workers_of_every_platform_count_toward_max_draining() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.max_draining = 1)));
    f.worker("linux");
    f.lease(secs(600));
    mac(&mut f, "mac");
    f.next_n += 1;
    f.submit(Request {
        needs: js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap(),
        ..request(f.next_n, LeaseKind::Action)
    });
    let grants = f.grants();
    assert_eq!(grants.len(), 1, "the action is placed on the mac");
    f.start(&grants[0]);
    let on_linux = f.submit_whole(Some(secs(600)));
    f.advance(10_000);
    assert_eq!(f.held_for(on_linux), Some(&w("linux")));
    f.next_n += 1;
    let on_mac = f.s.next_operation();
    f.submit(Request {
        needs: js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap(),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    f.advance(20_000);
    assert_eq!(
        f.s.whole_wait(on_mac),
        Some(WholeWait::Cap(Blocked::MaxDraining))
    );
}

/// Catches: `max_draining` applied to a quiet take, which drains nothing: with the one
/// draining slot taken by a linux request that holds the busy linux worker, a mac request
/// is still granted the quiet mac worker.
#[test]
fn max_draining_does_not_stop_a_quiet_take() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.max_draining = 1)));
    f.worker("linux");
    f.lease(secs(600));
    let draining = f.submit_whole(Some(secs(600)));
    f.advance(10_000);
    assert_eq!(f.held_for(draining), Some(&w("linux")));
    mac(&mut f, "mac");
    f.next_n += 1;
    f.submit(Request {
        needs: js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap(),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].worker, w("mac"));
}
