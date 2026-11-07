//! Hosts held whole by whole-node leases that no request waits on (started without a request
//! id), under both placements: `--placement registry` (`registry::NAME`) and `--placement
//! js-sched` (`js_sched::NAME`), with the same assertions. A waiting whole-node request
//! never sets such a host aside: it waits until one of them is free. Nothing sleeps; the
//! clock is the test's.

mod support;

use js_provider::{Pool, StartAck};
use support::world::*;
use support::*;

/// Starts whole-node lease `sequence` without a request id, bound to an hour, which is
/// committed on a quiet host, and has that host report it ready: the host it holds.
fn hold(w: &mut World, sequence: u64) -> String {
    let started = w.provider.start(World::whole_spec(sequence, None, HOUR_S));
    assert_eq!(
        started,
        StartAck::Unknown,
        "lease {sequence} waits for ready"
    );
    let booked = w.provider.booked();
    let on = booked.iter().find(|(_, held)| **held == lease(sequence));
    let on = on.map(|(host, _)| host.to_string()).unwrap();
    w.ready(&on, &lease(sequence));
    let ready = js_provider::Observed::Present { host: host(&on) };
    assert_eq!(w.provider.observe(&lease(sequence)), ready);
    on
}

/// Asserts that a whole-node request waits behind whole-node leases started without a
/// request id rather than set aside a host one of them holds: with both hosts held, the
/// request is on the wait list with no host, however long it waits; once one lease stops,
/// that host, and only it, is set aside for the request. It catches a host held whole by a
/// lease of no request taken for a host that may be drained.
fn a_host_a_whole_node_lease_holds_is_not_set_aside(p: Placement) {
    let mut w = World::new_in(p, &["a", "b"], |settings| settings);
    let first = hold(&mut w, 1);
    let second = hold(&mut w, 2);
    assert_ne!(first, second);
    w.ask(3, "q");
    w.step(10 * 60_000);
    assert_eq!(w.reservations(), []);
    assert_eq!(w.fx.registry.queue_position(&id("q")), Some(0));
    w.stop(1);
    w.step(POLL_MS);
    assert_eq!(w.reserved_host("q"), Some(first));
}

both!(a_host_a_whole_node_lease_holds_is_not_set_aside);
