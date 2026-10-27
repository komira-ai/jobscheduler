//! Expiry: time acts only in a tick, and a lease never outlives its maximum wall time.

use std::time::Duration;

use js_lease::RenewError;
use js_provider::Call;
use js_types::LeaseId;

use crate::support::{
    Scratch, T0, at, clock, fake_with_plain, fresh, open, plain_request, request, secs,
};

/// Asserts that a lease lapses only when the clock passes its expiry and a tick runs:
/// before the tick a renew is refused as expired, the tick stops it on the provider and
/// reports it, and then the table has nothing.
#[test]
fn expiry_by_advancing_the_clock() {
    let (mut table, clock, _scratch) = fresh("expiry");
    let lease = table.acquire(request(3600), None).unwrap();
    clock.advance(Duration::from_millis(59_999));
    let quiet = table.tick();
    assert!(!quiet.changed());
    assert_eq!(table.list().len(), 1);

    clock.advance(Duration::from_millis(1));
    let err = table.renew(&lease.id, None).unwrap_err();
    assert!(matches!(err, RenewError::Expired(id) if id == lease.id));
    let report = table.tick();
    assert_eq!(report.expired, vec![lease.id]);
    assert_eq!(report.freed, vec![lease.id]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let gone = table.renew(&lease.id, None).unwrap_err();
    assert!(matches!(gone, RenewError::Unknown(_)));
}

/// Asserts that a renew never extends past acquisition plus the maximum wall time, and
/// that at that moment the lease expires whatever was renewed.
#[test]
fn renew_is_capped_at_the_max_wall_time() {
    let (mut table, clock, _scratch) = fresh("max_wall");
    let lease = table.acquire(request(100), Some(secs(60))).unwrap();
    clock.advance(secs(50));
    assert_eq!(
        table.renew(&lease.id, Some(secs(60))).unwrap(),
        at(T0 + 100_000)
    );
    assert_eq!(
        table.renew(&lease.id, Some(secs(60))).unwrap(),
        at(T0 + 100_000)
    );
    assert_eq!(table.list()[0].ends_at, at(T0 + 100_000));
    clock.advance(secs(50));
    let err = table.renew(&lease.id, Some(secs(60))).unwrap_err();
    assert!(matches!(err, RenewError::Expired(_)));
    assert_eq!(table.tick().expired, vec![lease.id]);
}

/// Asserts that a pending lease whose expiry passes before it resolves is stopped on
/// the provider, so a start that landed late does not leave a booking behind. The class
/// is not whole-node, so the ttl bounds the wait (a whole-node lease waits for the
/// prepare timeout instead; see the whole module).
#[test]
fn pending_lease_past_expiry_is_stopped() {
    let scratch = Scratch::new("pending_expiry");
    let clock = clock();
    let mut table = open(fake_with_plain(), &clock, &scratch);
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(plain_request(3600), None).unwrap_err();
    assert_eq!(table.provider().booked().len(), 1);
    clock.advance(secs(60));
    let report = table.tick();
    assert_eq!(report.expired, vec![LeaseId::new(1, 1)]);
    assert_eq!(report.freed, report.expired);
    assert!(table.provider().booked().is_empty());
}
