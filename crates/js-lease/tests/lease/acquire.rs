//! Acquiring: the grant, the ttl bounds, refusals and the answers a start can give; and the plain value types.

use std::fs;
use std::time::Duration;

use js_lease::{
    AcquireError, Boot, Clock, LeaseState, LeaseTable, ManualClock, ReleaseError, ReleaseOutcome,
    RenewError, Settings, SettingsError, SystemClock, Timestamp,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::{Call, HostCapacity, Provider, RefusalCode};
use js_types::{ClassId, LeaseId, ProviderId, Request, Resources};

use crate::support::{
    Scratch, T0, at, clock, fresh, gpu_host_request, host, request, secs, settings, states,
};

/// Asserts that the same flow works on the static pool, that two acquires never get the
/// same host, and that a third is refused when every host is booked.
#[test]
fn two_acquires_never_share_a_host_on_the_pool() {
    let scratch = Scratch::new("pool");
    let pool = StaticPool::from_toml(ProviderId::new("pool").unwrap(), EXAMPLE_CONFIG).unwrap();
    let mut table =
        LeaseTable::open(pool, clock(), settings(), scratch.file(), Boot::Initialise).unwrap();
    let wanted = || Request {
        resources: Resources::new(1000, 1, 1, Some("nvidia-a100".into())).unwrap(),
        ..gpu_host_request(3600)
    };
    let a = table.acquire(wanted(), None).unwrap();
    let b = table.acquire(wanted(), None).unwrap();
    assert_eq!((a.host.as_str(), b.host.as_str()), ("gpu-01", "gpu-02"));
    let third = table.acquire(wanted(), None).unwrap_err();
    assert!(matches!(
        third,
        AcquireError::Refused(RefusalCode::NoCapacity)
    ));
    assert_eq!(table.provider().booked().len(), 2);

    assert_eq!(table.release(&a.id).unwrap(), ReleaseOutcome::Freed);
    let again = table.acquire(wanted(), None).unwrap();
    assert_eq!(again.host.as_str(), "gpu-01");
    assert_eq!(again.id, LeaseId::new(1, 4));
    let capacity: Vec<HostCapacity> = table.provider().capacity();
    assert!(capacity.iter().all(|c| c.leased_to.is_some()));
}

/// Asserts that a short max wall time caps the first expiry too, that a ttl above the
/// maximum is clamped to it, and that a zero ttl is refused on acquire and on renew.
#[test]
fn ttl_is_clamped_and_must_be_positive() {
    let (mut table, _clock, _scratch) = fresh("ttl");
    let short = table.acquire(request(10), None).unwrap();
    assert_eq!(short.expires_at, at(T0 + 10_000));
    let long = table.acquire(request(3600), Some(secs(1000))).unwrap();
    assert_eq!(long.expires_at, at(T0 + 120_000));
    let zero = table
        .acquire(request(3600), Some(Duration::ZERO))
        .unwrap_err();
    assert!(matches!(zero, AcquireError::TtlNotPositive));
    let zero = table.renew(&long.id, Some(Duration::ZERO)).unwrap_err();
    assert!(matches!(zero, RenewError::TtlNotPositive));
    assert_eq!(table.list().len(), 2);
}

/// Asserts that a refusal, by the start contract or by the provider, records nothing
/// and that a renew or release of an id the table never held is an unknown-id error.
#[test]
fn refusal_records_nothing() {
    let (mut table, _clock, scratch) = fresh("refusal");
    let mut bad = request(3600);
    bad.class = ClassId::new("other").unwrap();
    let err = table.acquire(bad, None).unwrap_err();
    assert!(matches!(
        err,
        AcquireError::Refused(RefusalCode::UnknownClass)
    ));
    let mut wide = request(3600);
    wide.resources = Resources::new(99_000, 1, 1, Some("g".into())).unwrap();
    let err = table.acquire(wide, None).unwrap_err();
    assert!(matches!(err, AcquireError::Refused(RefusalCode::NoFit)));
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("[[lease]]"));

    let nobody = LeaseId::new(7, 7);
    assert!(matches!(
        table.renew(&nobody, None),
        Err(RenewError::Unknown(_))
    ));
    assert!(matches!(
        table.release(&nobody),
        Err(ReleaseError::Unknown(_))
    ));
}

/// Asserts that a start that answers Unknown and landed is recorded pending, not
/// returned as held, and that a tick resolves it to held on the host the provider names.
#[test]
fn unknown_start_that_landed_resolves_to_held() {
    let (mut table, _clock, _scratch) = fresh("unknown_landed");
    table.provider_mut().script_unknown(Call::Start, true);
    let err = table.acquire(request(3600), None).unwrap_err();
    let id = LeaseId::new(1, 1);
    assert!(matches!(err, AcquireError::Unconfirmed(got) if got == id));
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);
    assert!(matches!(
        table.renew(&id, None),
        Err(RenewError::NotHeld { .. })
    ));

    let report = table.tick();
    assert_eq!(report.confirmed, vec![id]);
    assert_eq!(report.pending, 0);
    let info = &table.list()[0];
    assert_eq!(
        (info.state, info.host.clone()),
        (LeaseState::Held, Some(host("h1")))
    );
}

/// Asserts that a start that answered Unknown and did not land is dropped when the
/// provider answers Absent, and that an Unknown observation keeps the lease pending
/// and changes nothing.
#[test]
fn unknown_start_that_did_not_land_is_dropped() {
    let (mut table, _clock, _scratch) = fresh("unknown_lost");
    table.provider_mut().script_unknown(Call::Start, false);
    table.acquire(request(3600), None).unwrap_err();
    table.provider_mut().script_unknown(Call::Observe, false);
    let unsure = table.tick();
    assert_eq!((unsure.pending, unsure.changed()), (1, false));
    assert_eq!(table.list().len(), 1);

    let report = table.tick();
    assert_eq!(report.dropped, vec![LeaseId::new(1, 1)]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that settings refuse a zero default and a default above the maximum, and
/// that the bounds read back.
#[test]
fn settings_are_checked() {
    assert_eq!(
        Settings::new(Duration::ZERO, secs(1)),
        Err(SettingsError::DefaultNotPositive)
    );
    assert_eq!(
        Settings::new(secs(2), secs(1)),
        Err(SettingsError::DefaultAboveMax)
    );
    let ok = Settings::new(secs(1), secs(1)).unwrap();
    assert_eq!((ok.default_ttl(), ok.max_ttl()), (secs(1), secs(1)));
    assert_eq!(
        SettingsError::DefaultNotPositive.to_string(),
        "the default ttl must be positive"
    );
    assert_eq!(
        SettingsError::DefaultAboveMax.to_string(),
        "the default ttl must not exceed the maximum ttl"
    );
}

/// Asserts that timestamps saturate instead of wrapping, that the manual clock moves
/// only when told to and clones share one reading, and that the system clock reads a
/// time after the epoch.
#[test]
fn clocks_and_timestamps() {
    let top = Timestamp::from_millis(u64::MAX);
    assert_eq!(top.millis(), i64::MAX as u64);
    assert_eq!(top.plus(Duration::MAX), top);
    assert_eq!(at(5).plus(Duration::from_millis(7)), at(12));

    let clock = ManualClock::new(at(10));
    let other = clock.clone();
    other.advance(Duration::from_millis(5));
    assert_eq!(clock.now(), at(15));
    clock.set(at(3));
    assert_eq!(other.now(), at(3));
    assert_eq!(ManualClock::default().now(), at(0));
    assert!(SystemClock.now().millis() > 0);
}

/// Asserts that each error and state prints a stable message.
#[test]
fn messages_are_stable() {
    let id = LeaseId::new(2, 3);
    let held = RenewError::NotHeld {
        id,
        state: LeaseState::Pending,
    };
    assert_eq!(held.to_string(), "lease 2-3 is pending, not held");
    assert_eq!(
        RenewError::Unknown(id).to_string(),
        "lease 2-3 is not known"
    );
    assert_eq!(RenewError::Expired(id).to_string(), "lease 2-3 has expired");
    assert_eq!(
        RenewError::TtlNotPositive.to_string(),
        "the ttl must be positive"
    );
    assert_eq!(
        AcquireError::TtlNotPositive.to_string(),
        "the ttl must be positive"
    );
    assert_eq!(
        AcquireError::Refused(RefusalCode::NoFit).to_string(),
        "the start was refused: no-fit"
    );
    assert_eq!(
        AcquireError::Unconfirmed(id).to_string(),
        "lease 2-3 is pending: the provider did not answer the start"
    );
    assert_eq!(
        AcquireError::IdsExhausted.to_string(),
        "lease ids are used up: the next sequence cannot be stored"
    );
    assert_eq!(
        ReleaseError::Unknown(id).to_string(),
        "lease 2-3 is not known"
    );
    let all = [LeaseState::Pending, LeaseState::Held, LeaseState::Releasing];
    let names: Vec<String> = all.iter().map(|s| s.to_string()).collect();
    assert_eq!(names, ["pending", "held", "releasing"]);
}
