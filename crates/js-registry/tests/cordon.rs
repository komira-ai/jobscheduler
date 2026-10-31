//! The cordon of a host: it keeps new leases off a host that is live, renews and lifts,
//! lapses by itself, is kept in the bindings a caller persists, and is not forgotten with
//! a silent host.

mod support;

use std::time::Duration;

use js_lease::Timestamp;
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Binding, MIN_LOST_RETENTION_MS, NotHeld};
use support::*;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Lets `n` seconds pass in steps of five, host `a` (epoch 1) heartbeating at each, so
/// that it stays live.
fn live_for(fx: &Fixture, n: u64) {
    for _ in 0..n / 5 {
        fx.clock.advance(secs(5));
        fx.beat("a", 1);
    }
}

fn until(fx: &Fixture, name: &str) -> Option<Timestamp> {
    fx.registry.own_host(&host(name)).unwrap().cordon_until
}

/// Asserts what a cordon does to placement and how it ends: while it is in force a live
/// host takes no lease (`NoCapacity`), just before it lapses still none, and at the time it
/// lapses the host takes one with no further call. It catches a cordon that is not read
/// when a lease is placed, and one that never ends.
#[test]
fn a_cordoned_host_takes_no_lease_until_the_cordon_lapses() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    let placed = fx.registry.cordon(&host("a"), secs(60)).unwrap();
    assert!(placed.placed);
    assert_eq!(
        placed.host.cordon_until,
        Some(Timestamp::from_millis(1_060_000))
    );
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    live_for(&fx, 55);
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    live_for(&fx, 10);
    assert_eq!(until(&fx, "a"), None);
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
}

/// Asserts that a cordon cuts nothing and survives the end of the lease: a lease held when
/// the host is cordoned stays held and is read on the host, and once it is stopped the host
/// takes no new one. It catches a cordon that gives up a lease it finds.
#[test]
fn a_cordon_leaves_the_lease_it_finds_and_outlives_it() {
    let mut fx = fixture(2);
    fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    let cordoned = fx.registry.cordon(&host("a"), secs(600)).unwrap();
    assert_eq!(cordoned.host.host.lease, Some(lease(1)));
    fx.provider.stop(&lease(1));
    fx.beat("a", 1);
    assert_eq!(fx.registry.own_host(&host("a")).unwrap().host.lease, None);
    assert_eq!(
        fx.provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts the renewal: a repeat reports that it did not place the cordon, a longer one
/// moves the expiry out from the time of the repeat, a shorter one leaves it where it was,
/// and `uncordon` reports whether it lifted one and leaves the host unmarked, again and
/// again. It catches a renew that shortens, one that reports itself a placement, and an
/// uncordon that fails when there is nothing to lift.
#[test]
fn a_repeat_extends_and_never_shortens_and_an_uncordon_repeats() {
    let fx = fixture(2);
    fx.join("a", "t");
    let first = fx.registry.cordon(&host("a"), secs(100)).unwrap();
    assert!(first.placed);
    live_for(&fx, 10);
    let shorter = fx.registry.cordon(&host("a"), secs(20)).unwrap();
    assert!(!shorter.placed);
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_100_000)));
    fx.registry.cordon(&host("a"), secs(200)).unwrap();
    assert_eq!(until(&fx, "a"), Some(Timestamp::from_millis(1_210_000)));

    assert!(fx.registry.uncordon(&host("a")).unwrap().lifted);
    assert!(!fx.registry.uncordon(&host("a")).unwrap().lifted);
    assert_eq!(until(&fx, "a"), None);
    let again = fx.registry.cordon(&host("a"), secs(5)).unwrap();
    assert!(again.placed);
}

/// Asserts that a host the registry does not hold is `NotHeld` to all three calls, and
/// that the read has what the hello said: the runtime and the agent version. It catches a
/// call that makes up a host.
#[test]
fn an_unknown_host_is_not_held_and_the_read_has_the_hello() {
    let fx = fixture(2);
    assert_eq!(fx.registry.cordon(&host("a"), secs(5)), Err(NotHeld));
    assert_eq!(fx.registry.uncordon(&host("a")), Err(NotHeld));
    assert_eq!(fx.registry.own_host(&host("a")), None);

    let mut said = hello("a");
    said.capabilities
        .insert("runtime".into(), "container".into());
    fx.hello(&said, &cred("t")).unwrap();
    let read = fx.registry.own_host(&host("a")).unwrap();
    assert_eq!(read.runtime.as_deref(), Some("container"));
    assert_eq!(read.host.agent_version.as_deref(), Some("0.1.0"));
    assert_eq!(read.now, Timestamp::from_millis(1_000_000));
    fx.join("b", "t");
    assert_eq!(fx.registry.own_host(&host("b")).unwrap().runtime, None);
}

/// Asserts that a cordon is part of what is persisted: the bindings carry it while it is in
/// force and not after it lapsed or was lifted, a registry started over them puts a
/// cordoned host back awaiting with the same expiry, a binding with a lapsed cordon puts
/// nothing back, and the host keeps the cordon through its hello. It catches a restart that
/// lifts a cordon and one that puts back a cordon that is over.
#[test]
fn a_cordon_survives_what_is_persisted() {
    let config = config(3)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let before = fixture_of(config.clone());
    for name in ["a", "b", "c"] {
        before.join(name, "t");
    }
    before.registry.cordon(&host("a"), secs(100)).unwrap();
    before.registry.cordon(&host("b"), secs(100)).unwrap();
    before.registry.uncordon(&host("b")).unwrap();
    let saved = before.registry.bindings();
    assert_eq!(saved[&host("a")].cordon_until_ms(), Some(1_100_000));
    assert_eq!(saved[&host("b")].cordon_until_ms(), None);
    assert_eq!(saved[&host("c")].cordon_until_ms(), None);

    let after = fixture_of(config);
    after
        .registry
        .restore_tainted(&host("b"), saved[&host("b")].clone());
    assert!(after.registry.hosts().is_empty());
    let lapsed = Binding::new(cred("t"), None).with_cordon(Some(999_999));
    after.registry.restore_tainted(&host("c"), lapsed);
    assert!(after.registry.hosts().is_empty());
    after
        .registry
        .restore_tainted(&host("a"), saved[&host("a")].clone());
    let listed = after.registry.hosts();
    assert_eq!(listed.len(), 1);
    assert_eq!(until(&after, "a"), Some(Timestamp::from_millis(1_100_000)));

    after.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(until(&after, "a"), Some(Timestamp::from_millis(1_100_000)));
    assert_eq!(
        after.provider.clone().start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a cordoned host that falls silent is not forgotten while its cordon is in
/// force, however long the retention, and is forgotten like any lost host once the cordon
/// has lapsed and the retention has passed. It catches a cordon that a silent host loses
/// with its record.
#[test]
fn a_cordoned_silent_host_is_kept_until_the_cordon_lapses() {
    let config = config(2)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let fx = fixture_of(config);
    fx.join("a", "t");
    fx.registry.cordon(&host("a"), secs(3600)).unwrap();
    fx.clock.advance(Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS + 1,
    ));
    assert_eq!(fx.registry.hosts().len(), 1);

    fx.clock.advance(secs(3600));
    assert!(fx.registry.hosts().is_empty());
}
