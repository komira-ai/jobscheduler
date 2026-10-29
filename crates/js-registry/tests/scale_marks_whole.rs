//! How the scale-down marks meet whole-node requests, and the two things the registry
//! tells a scaler about demand: `waiting` and `Launchable`. The registry is driven
//! directly on a manual clock with the reservation tests' world.

mod support;

use std::sync::{Arc, Mutex};

use js_elastic::Demand;
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Busy, Launchable, Waiting, WholeNodeConfig};
use support::world::*;
use support::*;

/// A pool whose requests reserve a busy host only after ten minutes, so that a host that
/// turns quiet first is assigned to the request.
fn slow_drain(w: WholeNodeConfig) -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: 600_000,
        ..w
    }
}

/// Asserts that a draining host is not given to a waiting request and that undraining it
/// hands it over: the request waits (the start answers `Unknown`, no lease is committed, no
/// host is set aside), and after `undrain` the next poll commits it. It catches a
/// whole-node placement that does not read the mark.
#[test]
fn a_draining_host_is_not_committed_to_a_waiting_request() {
    let mut w = World::new(&["a"], plain);
    w.fx.registry.drain(&host("a")).unwrap();
    assert_eq!(w.ask(1, "r1"), StartAck::Unknown);
    w.step(3 * POLL_MS);
    assert_eq!(w.lease_on("a"), None);
    assert!(w.reservations().is_empty());
    assert_eq!(w.fx.registry.queue_position(&id("r1")), Some(0));
    w.fx.registry.undrain(&host("a")).unwrap();
    w.step(POLL_MS);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    assert!(w.fx.registry.waiting().is_empty());
}

/// Asserts that a request that has to reserve a busy host never reserves a marked one,
/// even though the marked host is idle and so the cheapest to empty: with `a` busy and `b`
/// draining, the request's reservation is `a`'s; once `b` is releasing it is still `a`'s.
/// It catches a reservation pass that counts a host being scaled away as a candidate.
#[test]
fn a_marked_host_is_not_reserved_for_a_request() {
    for release in [false, true] {
        let mut w = World::new(&["a", "b"], plain);
        w.share(1, HOUR_S);
        assert_eq!(w.lease_on("a"), Some(lease(1)));
        w.fx.registry.drain(&host("b")).unwrap();
        if release {
            w.fx.registry.release_host(&host("b")).unwrap();
        }
        w.ask(2, "r1");
        assert_eq!(w.reserved_host("r1"), Some("a".to_string()), "{release}");
        w.step(POLL_MS);
        assert_eq!(w.reserved_host("r1"), Some("a".to_string()), "{release}");
    }
}

/// Asserts that `drain` refuses a host a waiting request holds: `a` turns quiet while
/// `r1` waits and is assigned to it, and the drain answers `Busy::Whole` and leaves the
/// reservation. It catches a drain that takes a host out from under the request it was set
/// aside for.
#[test]
fn drain_refuses_a_host_set_aside_for_a_request() {
    let mut w = World::new(&["a"], slow_drain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.stop(1);
    w.provider.observe(&lease(1));
    let mark = w.reservation("r1").unwrap();
    assert!(mark.assigned);
    assert_eq!(w.fx.registry.drain(&host("a")), Err(Busy::Whole));
    assert_eq!(w.reservation("r1"), Some(mark));
}

/// Asserts that `drain` refuses a host that is restoring after a whole-node lease, though
/// no lease is booked and the host lists no run, and takes it after the second heartbeat
/// that ends the restore. It catches a quiet check that reads only leases and runs.
#[test]
fn drain_refuses_a_host_restoring_from_a_whole_node_lease() {
    let mut w = World::new(&["a"], plain);
    assert_eq!(w.ask(1, "r1"), StartAck::Unknown);
    w.cancel("r1");
    w.stop(1);
    assert_eq!(w.lease_on("a"), None);
    assert_eq!(w.fx.registry.drain(&host("a")), Err(Busy::Whole));
    w.step(POLL_MS);
    assert_eq!(w.fx.registry.drain(&host("a")), Err(Busy::Whole));
    w.step(POLL_MS);
    assert_eq!(w.fx.registry.drain(&host("a")), Ok(()));
}

/// Asserts what `waiting` reports: the requests in the order they were first presented,
/// each with its demand (the request's resources and its `os`), the time it was first
/// presented and, once a host is set aside for it, that host; a request that has no host
/// has `None`; a cancelled request is gone. It catches a list in the wrong order, a demand
/// that loses the capabilities, a `since` that moves with each poll, and a `reserved`
/// that names a host another request holds.
#[test]
fn waiting_lists_requests_with_their_demand_and_host() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    let first = w.now();
    w.ask(2, "r1");
    w.step(POLL_MS);
    let second = w.now();
    w.ask(3, "r2");
    w.step(POLL_MS);
    let demand = Demand::from_request(&request());
    assert_eq!(demand.os.as_deref(), Some("macos"));
    let expected = vec![
        Waiting {
            id: id("r1"),
            demand: demand.clone(),
            since: first,
            reserved: Some(host("a")),
        },
        Waiting {
            id: id("r2"),
            demand,
            since: second,
            reserved: None,
        },
    ];
    assert_eq!(w.fx.registry.waiting(), expected);
    w.cancel("r1");
    assert_eq!(w.fx.registry.waiting(), expected[1..].to_vec());
}

/// Asserts that a quiet host assigned to a waiting request is reported as the request's
/// `reserved` host: the mark of a free host counts, not only one that drains a busy host.
#[test]
fn an_assigned_host_is_reported_as_reserved() {
    let mut w = World::new(&["a"], slow_drain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.stop(1);
    w.provider.observe(&lease(1));
    let waiting = w.fx.registry.waiting();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].reserved, Some(host("a")));
}

/// A launchable that answers `answer` and records the demands it was asked about.
fn launchable(answer: bool) -> (Launchable, Arc<Mutex<Vec<Demand>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let launchable = Launchable::new(move |demand| {
        log.lock().unwrap().push(demand.clone());
        answer
    });
    (launchable, seen)
}

/// Asserts the refusal a request meets with and without a launchable: with no host in the
/// pool and no launchable it is refused `no-capable-host` and not kept; with one that says
/// yes it waits (the start answers `Unknown`, the request is on the list and `waiting`
/// shows it); with one that says no, or after the launchable is removed, it is refused
/// again. The launchable is asked about the request's own demand. It catches a pool that
/// cannot wait for a machine it could launch, and one that waits on a dead cloud.
#[test]
fn a_launchable_decides_whether_a_request_with_no_host_waits() {
    let mut w = World::new(&[], plain);
    let none = StartAck::Refused(RefusalCode::NoCapableHost);
    assert_eq!(w.ask(1, "r1"), none);
    assert_eq!(w.fx.registry.queue_position(&id("r1")), None);
    w.cancel("r1");

    let (yes, seen) = launchable(true);
    w.fx.registry.set_launchable(Some(yes));
    assert_eq!(w.ask(2, "r2"), StartAck::Unknown);
    assert_eq!(w.fx.registry.queue_position(&id("r2")), Some(0));
    let waiting = w.fx.registry.waiting();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].reserved, None);
    let asked = seen.lock().unwrap().clone();
    assert!(!asked.is_empty());
    assert!(asked.iter().all(|demand| *demand == waiting[0].demand));

    let (no, _) = launchable(false);
    w.fx.registry.set_launchable(Some(no));
    assert_eq!(w.poll(3, "r3", 600), none);
    w.fx.registry.set_launchable(None);
    assert_eq!(w.poll(4, "r4", 600), none);
}

/// Asserts that a start with no request id, which cannot wait, meets the launchable too:
/// it is answered `no-capacity` (a host could come) and not `no-capable-host`, and nothing
/// is kept for it. It catches a launchable that is read only for requests that queue.
#[test]
fn a_launchable_changes_the_refusal_of_a_request_that_cannot_wait() {
    let mut w = World::new(&[], plain);
    assert_eq!(
        w.provider.start(World::whole_spec(1, None, 600)),
        StartAck::Refused(RefusalCode::NoCapableHost)
    );
    w.fx.registry.set_launchable(Some(launchable(true).0));
    assert_eq!(
        w.provider.start(World::whole_spec(2, None, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert!(w.fx.registry.waiting().is_empty());
}

/// A `Launchable` prints without its closure, so a registry or a handle holding one can be
/// printed.
#[test]
fn a_launchable_prints_as_a_name() {
    assert_eq!(format!("{:?}", launchable(true).0), "Launchable(..)");
}
