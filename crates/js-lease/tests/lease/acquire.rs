//! Acquiring: the grant, refusals and the answers a start can give; and the plain value types.

use std::fs;
use std::time::Duration;

use js_lease::{
    AcquireError, Boot, Clock, LeaseState, LeaseTable, ManualClock, ReleaseError, ReleaseOutcome,
    Settings, SettingsError, SystemClock, Timestamp,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::{Call, HostCapacity, Pool, RefusalCode};
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
    let a = table.acquire(wanted()).unwrap();
    let b = table.acquire(wanted()).unwrap();
    assert_eq!((a.host.as_str(), b.host.as_str()), ("gpu-01", "gpu-02"));
    let third = table.acquire(wanted()).unwrap_err();
    assert!(matches!(
        third,
        AcquireError::Refused(RefusalCode::NoCapacity)
    ));
    assert_eq!(table.provider().booked().len(), 2);

    assert_eq!(table.release(&a.id).unwrap(), ReleaseOutcome::Freed);
    let again = table.acquire(wanted()).unwrap();
    assert_eq!(again.host.as_str(), "gpu-01");
    assert_eq!(again.id, LeaseId::new(1, 4));
    let capacity: Vec<HostCapacity> = table.provider().capacity();
    assert!(capacity.iter().all(|c| c.leased_to.is_some()));
}

/// Asserts that a lease ends at its maximum wall time, whatever that is: a short and a
/// long request each report `ends_at` as the acquire time plus their own wall time, with no
/// other bound applied. It catches a grant that still clamps the end to a default or
/// longest time to live.
#[test]
fn a_lease_ends_at_its_maximum_wall_time() {
    let (mut table, _clock, _scratch) = fresh("ends");
    let short = table.acquire(request(10)).unwrap();
    assert_eq!(short.ends_at, at(T0 + 10_000));
    let long = table.acquire(request(3600)).unwrap();
    assert_eq!(long.ends_at, at(T0 + 3_600_000));
    assert_eq!(table.list().len(), 2);
}

/// Asserts that a refusal, by the start contract or by the provider, records nothing
/// and that a release of an id the table never held is an unknown-id error.
#[test]
fn refusal_records_nothing() {
    let (mut table, _clock, scratch) = fresh("refusal");
    let mut bad = request(3600);
    bad.class = ClassId::new("other").unwrap();
    let err = table.acquire(bad).unwrap_err();
    assert!(matches!(
        err,
        AcquireError::Refused(RefusalCode::UnknownClass)
    ));
    let mut wide = request(3600);
    wide.resources = Resources::new(99_000, 1, 1, Some("g".into())).unwrap();
    let err = table.acquire(wide).unwrap_err();
    assert!(matches!(err, AcquireError::Refused(RefusalCode::NoFit)));
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("[[lease]]"));

    let nobody = LeaseId::new(7, 7);
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
    let err = table.acquire(request(3600)).unwrap_err();
    let id = LeaseId::new(1, 1);
    assert!(matches!(err, AcquireError::Unconfirmed(got) if got == id));
    assert_eq!(states(&table), vec![(id, LeaseState::Pending)]);

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
    table.acquire(request(3600)).unwrap_err();
    table.provider_mut().script_unknown(Call::Observe, false);
    let unsure = table.tick();
    assert_eq!((unsure.pending, unsure.changed()), (1, false));
    assert_eq!(table.list().len(), 1);

    let report = table.tick();
    assert_eq!(report.dropped, vec![LeaseId::new(1, 1)]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that the default settings have a prepare timeout of ten minutes, no ceiling on
/// the wall time and no restart grace, that `Default` is the same, that the `with_`
/// methods change one bound each, and that a zero prepare timeout or ceiling is refused
/// with a stable message.
#[test]
fn settings_are_checked() {
    let plain = Settings::new();
    assert_eq!(plain, Settings::default());
    assert_eq!(plain.prepare_timeout(), secs(600));
    assert_eq!(plain.max_wall(), None);
    assert_eq!(
        (plain.boot_grace(), plain.regain_grace()),
        (Duration::ZERO, Duration::ZERO)
    );
    let set = plain
        .with_prepare_timeout(secs(5))
        .unwrap()
        .with_max_wall(secs(7))
        .unwrap()
        .with_restart_grace(secs(1), secs(2));
    assert_eq!(set.prepare_timeout(), secs(5));
    assert_eq!(set.max_wall(), Some(secs(7)));
    assert_eq!((set.boot_grace(), set.regain_grace()), (secs(1), secs(2)));
    assert_eq!(
        Settings::new().with_prepare_timeout(Duration::ZERO),
        Err(SettingsError::PrepareNotPositive)
    );
    assert_eq!(
        Settings::new().with_max_wall(Duration::ZERO),
        Err(SettingsError::MaxWallNotPositive)
    );
    assert_eq!(
        SettingsError::PrepareNotPositive.to_string(),
        "the prepare timeout must be positive"
    );
    assert_eq!(
        SettingsError::MaxWallNotPositive.to_string(),
        "the ceiling on the maximum wall time must be positive"
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
