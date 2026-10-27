//! How long the registry keeps a whole-node request's record: removed once a terminal
//! refusal has been delivered, dropped after the idle horizon, bounded across many
//! requests.
//!
//! The registry has no request id yet, so a request is its content: the tests tell
//! requests apart by `max_wall`.

mod support;

use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, RefusalReason};
use js_provider::{Observed, Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, WholeNodeConfig};
use js_types::{ProviderId, Request, ResourceClass};
use support::*;

const BACKOFF: u64 = 60_000;
const TIMEOUT: u64 = 120_000;
/// `BACKOFF` for each of the three tries, which is more than `TIMEOUT`.
const HORIZON: u64 = 3 * BACKOFF;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn config() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    let settings = WholeNodeConfig {
        prepare_timeout_ms: TIMEOUT,
        reserve_backoff_ms: BACKOFF,
        max_replace: 2,
        ..WholeNodeConfig::dedicated(4)
    };
    PoolConfig::new(name, whole(), pool_shape(), 4)
        .with_whole_node(settings)
        .unwrap()
}

fn request_of(seconds: u64) -> Request {
    Request {
        class: whole().id().clone(),
        max_wall: Duration::from_secs(seconds),
        ..request()
    }
}

fn spec_of(sequence: u64, seconds: u64) -> StartSpec {
    StartSpec::new(lease(sequence), request_of(seconds), &[whole()]).unwrap()
}

fn no_capacity() -> StartAck {
    StartAck::Refused(RefusalCode::NoCapacity)
}

fn pool(names: &[&'static str]) -> (Fixture, Vec<(&'static str, u64)>) {
    let fx = fixture_of(config());
    let hosts = names
        .iter()
        .map(|name| (*name, fx.join(name, "t")))
        .collect();
    (fx, hosts)
}

fn refuse(fx: &Fixture, host: (&str, u64), lease_id: String, code: RefusalReason) {
    let mut message = fx.heartbeat(host.0, host.1);
    message.exclusive_refused = Some(ExclusiveRefusal {
        lease_id,
        code,
        detail: String::new(),
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
}

/// Moves the clock on by `ms` in steps shorter than the grace period, every host sending
/// a serving heartbeat at each, so none is lost.
fn wait(fx: &Fixture, hosts: &[(&str, u64)], ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(5_000);
        fx.clock.advance(Duration::from_millis(step));
        hosts.iter().for_each(|(name, epoch)| fx.beat(name, *epoch));
        left -= step;
    }
}

/// Two serving heartbeats from every host: the restore after a lease has ended is done.
fn restore(fx: &Fixture, hosts: &[(&str, u64)]) {
    for _ in 0..2 {
        hosts.iter().for_each(|(name, epoch)| fx.beat(name, *epoch));
    }
}

fn recorded(fx: &Fixture, seconds: u64) -> bool {
    fx.registry.request_record(&request_of(seconds)).is_some()
}

/// Fails the request of 600 s on hosts `a` and `b` in turn (leases 1 and 2), with `code`.
fn use_up_the_tries(fx: &Fixture, hosts: &[(&str, u64)], code: RefusalReason) {
    let mut provider = fx.provider.clone();
    for (n, host) in hosts.iter().enumerate() {
        let sequence = n as u64 + 1;
        assert_eq!(provider.start(spec_of(sequence, 600)), StartAck::Unknown);
        refuse(fx, *host, lease(sequence).to_string(), code);
    }
    assert!(fx.registry.not_ready(&request_of(600)).is_some());
}

/// The terminal refusal is delivered by the next start; the record goes with it, and the
/// start after is a fresh request that is placed, though the back-off of the failed
/// hosts has not run out.
#[test]
fn a_later_identical_request_after_the_terminal_refusal_is_placed() {
    let (fx, hosts) = pool(&["a", "b"]);
    let mut provider = fx.provider.clone();
    use_up_the_tries(&fx, &hosts, RefusalReason::NotQuiet);
    restore(&fx, &hosts);
    assert_eq!(provider.start(spec_of(3, 600)), no_capacity());
    assert!(!recorded(&fx, 600));
    assert_eq!(fx.registry.not_ready(&request_of(600)), None);
    assert_eq!(provider.start(spec_of(4, 600)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(4)));
}

/// A host left out for the life of a request is left out of that request only: the
/// exclusion goes with the record.
#[test]
fn a_for_life_exclusion_dies_with_the_record() {
    let (fx, hosts) = pool(&["a", "b"]);
    let mut provider = fx.provider.clone();
    use_up_the_tries(&fx, &hosts, RefusalReason::Unsupported);
    restore(&fx, &hosts);
    assert_eq!(provider.start(spec_of(3, 600)), no_capacity());
    assert_eq!(provider.start(spec_of(4, 600)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(4)));
}

/// A terminal request is refused once, not for ever, and not a second time while
/// another start of it is placed.
#[test]
fn only_one_start_gets_the_terminal_refusal() {
    let (fx, hosts) = pool(&["a", "b"]);
    let mut provider = fx.provider.clone();
    use_up_the_tries(&fx, &hosts, RefusalReason::NotQuiet);
    restore(&fx, &hosts);
    assert_eq!(provider.start(spec_of(3, 600)), no_capacity());
    assert_ne!(provider.start(spec_of(4, 600)), no_capacity());
    assert!(fx.registry.not_ready(&request_of(600)).is_none());
}

/// A record whose request is not started for the horizon is dropped at the horizon,
/// not before: `reserve_backoff` for each of `max_replace + 1` tries.
#[test]
fn an_idle_record_is_dropped_exactly_at_the_horizon() {
    let (fx, hosts) = pool(&["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(spec_of(1, 600)), StartAck::Unknown);
    refuse(&fx, hosts[0], lease(1).to_string(), RefusalReason::NotQuiet);
    assert!(recorded(&fx, 600));
    wait(&fx, &hosts, HORIZON - 5_000);
    assert!(recorded(&fx, 600));
    fx.clock.advance(Duration::from_millis(4_999));
    assert!(recorded(&fx, 600));
    fx.clock.advance(Duration::from_millis(1));
    assert!(!recorded(&fx, 600));
}

/// A start of the request restarts the idle time.
#[test]
fn a_start_of_the_request_restarts_the_idle_time() {
    let (fx, hosts) = pool(&["a"]);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(spec_of(1, 600)), StartAck::Unknown);
    refuse(&fx, hosts[0], lease(1).to_string(), RefusalReason::NotQuiet);
    wait(&fx, &hosts, BACKOFF - 5_000);
    // Inside the back-off, so the only host is turned away.
    assert_eq!(provider.start(spec_of(2, 600)), no_capacity());
    wait(&fx, &hosts, HORIZON - 5_000);
    assert!(recorded(&fx, 600));
    fx.clock.advance(Duration::from_millis(5_000));
    assert!(!recorded(&fx, 600));
}

/// The record of a request with a lease committed is kept while the lease waits for its
/// agent, even when the wait is as long as the horizon (a short back-off leaves the
/// prepare timeout as the horizon): the timeout then counts as the second failure.
#[test]
fn a_record_with_a_committed_lease_is_kept() {
    let name = ProviderId::new("builders").unwrap();
    let settings = WholeNodeConfig {
        prepare_timeout_ms: TIMEOUT,
        reserve_backoff_ms: 10_000,
        max_replace: 2,
        ..WholeNodeConfig::dedicated(4)
    };
    let short = PoolConfig::new(name, whole(), pool_shape(), 4)
        .with_whole_node(settings)
        .unwrap();
    let fx = fixture_of(short);
    let hosts = vec![("a", fx.join("a", "t")), ("b", fx.join("b", "t"))];
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(spec_of(1, 600)), StartAck::Unknown);
    refuse(&fx, hosts[0], lease(1).to_string(), RefusalReason::NotQuiet);
    assert_eq!(provider.start(spec_of(2, 600)), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    wait(&fx, &hosts, TIMEOUT);
    assert_eq!(provider.observe(&lease(2)), Observed::Absent);
    let kept = fx.registry.request_record(&request_of(600)).unwrap();
    assert_eq!(kept.prepare_failures, 2);
    assert!(kept.terminal);
}

/// Many requests that each fail once leave only the recent ones behind.
#[test]
fn the_records_stay_bounded_across_many_failed_requests() {
    let (fx, hosts) = pool(&["a"]);
    let mut provider = fx.provider.clone();
    let step = 10_000;
    let total = 60;
    for n in 1..=total {
        assert_eq!(provider.start(spec_of(n, n)), StartAck::Unknown);
        refuse(&fx, hosts[0], lease(n).to_string(), RefusalReason::NotQuiet);
        restore(&fx, &hosts);
        wait(&fx, &hosts, step);
    }
    let kept: Vec<u64> = (1..=total).filter(|n| recorded(&fx, *n)).collect();
    // Request `n` failed `total - n + 1` steps ago; it is kept while that is under the
    // horizon.
    let expected: Vec<u64> = (1..=total)
        .filter(|n| (total - n + 1) * step < HORIZON)
        .collect();
    assert_eq!(kept, expected);
    assert!(kept.len() < 20);
}
