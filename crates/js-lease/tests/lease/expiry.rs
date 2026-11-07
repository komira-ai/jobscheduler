//! Expiry: time acts only in a tick, and a lease ends at its maximum wall time and at no
//! other time.

use std::time::Duration;

use js_lease::LeaseState;
use js_provider::Call;
use js_types::LeaseId;

use crate::support::{
    Scratch, T0, at, clock, fake_with_plain, fresh, host, open, plain_request, request, secs,
    states,
};

/// Asserts that a lease ends only when the clock reaches its maximum wall time and a tick
/// runs: a tick one millisecond before changes nothing, the tick at the wall time stops it
/// on the provider and reports it, and then the table has nothing.
#[test]
fn expiry_by_advancing_the_clock() {
    let (mut table, clock, _scratch) = fresh("expiry");
    let lease = table.acquire(request(60)).unwrap();
    assert_eq!(lease.ends_at, at(T0 + 60_000));
    clock.advance(Duration::from_millis(59_999));
    let quiet = table.tick();
    assert!(!quiet.changed());
    assert_eq!(table.list().len(), 1);

    clock.advance(Duration::from_millis(1));
    let report = table.tick();
    assert_eq!(report.expired, vec![lease.id]);
    assert_eq!(report.freed, vec![lease.id]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that no clock but the maximum wall time ends a held lease: with a wall time of
/// two hours, a tick after 600 s (the old default ttl), after 3600 s (the old longest ttl)
/// and one millisecond before the wall end each leave it held, listed and unreported; the
/// tick at exactly its end reports it expired and freed, drops it from the list and
/// releases the provider's booking, so the host is granted again. It fails if a 600 s or
/// 3600 s time to live still ends the lease, or if the wall end stops ending it.
#[test]
fn a_lease_is_no_longer_expired_by_a_clock() {
    let scratch = Scratch::new("no_clock_expiry");
    let clock = clock();
    let mut table = open(fake_with_plain(), &clock, &scratch);
    let lease = table.acquire(plain_request(7200)).unwrap();
    assert_eq!(lease.host, host("h1"));
    assert_eq!(lease.ends_at, at(T0 + 7_200_000));

    let mut elapsed = 0;
    for target in [601_000, 3_601_000, 7_200_000 - 1] {
        clock.advance(Duration::from_millis(target - elapsed));
        elapsed = target;
        let report = table.tick();
        assert!(report.expired.is_empty(), "at {target} ms: {report:?}");
        assert!(report.freed.is_empty(), "at {target} ms: {report:?}");
        assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);
        assert_eq!(table.provider().booked().len(), 1);
    }

    clock.advance(Duration::from_millis(1));
    let report = table.tick();
    assert_eq!(report.expired, vec![lease.id]);
    assert_eq!(report.freed, vec![lease.id]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let next = table.acquire(plain_request(60)).unwrap();
    assert_eq!(next.host, host("h1"));
}

/// Asserts that a pending lease whose maximum wall time ends before it resolves is stopped
/// on the provider, so a start that landed late does not leave a booking behind. The class
/// is not whole-node, so the wall time bounds the wait (a whole-node lease waits for the
/// prepare timeout instead; see the whole module).
#[test]
fn pending_lease_past_its_wall_time_is_stopped() {
    let scratch = Scratch::new("pending_expiry");
    let clock = clock();
    let mut table = open(fake_with_plain(), &clock, &scratch);
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(plain_request(60)).unwrap_err();
    assert_eq!(table.provider().booked().len(), 1);
    clock.advance(secs(60));
    let report = table.tick();
    assert_eq!(report.expired, vec![LeaseId::new(1, 1)]);
    assert_eq!(report.freed, report.expired);
    assert!(table.provider().booked().is_empty());
}
