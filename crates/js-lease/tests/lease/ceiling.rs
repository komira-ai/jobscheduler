//! The ceiling on a request's maximum wall time: what it refuses, that a refusal leaves
//! nothing behind, that a lease ends at it, and that a restart keeps both.

use std::time::Duration;

use js_lease::{AcquireError, Boot, LeaseTable, Settings, SettingsError};
use js_types::LeaseId;

use crate::support::{Scratch, T0, Table, at, clock, fake, request, secs, settings};

fn ceiling_settings() -> Settings {
    settings().with_max_wall(secs(600)).unwrap()
}

fn table(name: &str) -> (Table, js_lease::ManualClock, Scratch) {
    let scratch = Scratch::new(name);
    let clock = clock();
    let opened = LeaseTable::open(
        fake(),
        clock.clone(),
        ceiling_settings(),
        scratch.file(),
        Boot::Initialise,
    );
    (opened.unwrap(), clock, scratch)
}

/// Asserts that a request one second above the ceiling is refused with the ceiling named,
/// that the refusal records nothing, uses no lease id and books no host, and that a request
/// at the ceiling is granted as the first lease. It catches a ceiling checked after the id
/// is drawn or the lease written, and one that is off by one.
#[test]
fn a_request_above_the_ceiling_is_refused_and_leaves_nothing() {
    let (mut table, _clock, _scratch) = table("ceiling_refuse");
    let err = table.acquire(request(601)).unwrap_err();
    assert!(matches!(
        err,
        AcquireError::WallAboveCeiling { ceiling } if ceiling == secs(600)
    ));
    assert_eq!(
        err.to_string(),
        "the maximum wall time is above the ceiling of 600 s"
    );
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
    let granted = table.acquire(request(600)).unwrap();
    assert_eq!(granted.id, LeaseId::new(1, 1));
}

/// Asserts that a lease granted at the ceiling ends at the ceiling after it began and not
/// later: it is still held one millisecond before and a tick at that moment expires it. It
/// proves a draining host empties within the ceiling.
#[test]
fn a_lease_at_the_ceiling_ends_at_the_ceiling() {
    let (mut table, clock, _scratch) = table("ceiling_ends");
    let granted = table.acquire(request(600)).unwrap();
    assert_eq!(granted.ends_at, at(T0 + 600_000));
    clock.advance(Duration::from_millis(599_999));
    assert!(table.tick().expired.is_empty());
    clock.advance(Duration::from_millis(1));
    assert_eq!(table.tick().expired, vec![granted.id]);
}

/// Asserts that with no ceiling set a long wall time is still granted (the default is
/// unchanged), and that a ceiling of zero is refused when the settings are built.
#[test]
fn no_ceiling_grants_as_before_and_a_zero_ceiling_is_refused() {
    let scratch = Scratch::new("ceiling_none");
    let mut table = LeaseTable::open(
        fake(),
        clock(),
        settings(),
        scratch.file(),
        Boot::Initialise,
    )
    .unwrap();
    assert_eq!(settings().max_wall(), None);
    assert!(table.acquire(request(10 * 24 * 3600)).is_ok());
    let zero = settings().with_max_wall(secs(0));
    assert_eq!(zero.unwrap_err(), SettingsError::MaxWallNotPositive);
}

/// Asserts that across a restart a lease granted at the ceiling keeps its end (it still
/// expires at it after the restart), and the reopened table, given the same
/// settings, refuses the same request. It catches a ceiling that lives only in memory of
/// the first process, or a restart that lengthens a lease.
#[test]
fn the_ceiling_and_the_lease_end_survive_a_restart() {
    let (mut table, clock, scratch) = table("ceiling_restart");
    let granted = table.acquire(request(600)).unwrap();
    let before = table.list();
    let provider = table.into_provider();
    let mut table = LeaseTable::open(
        provider,
        clock.clone(),
        ceiling_settings(),
        scratch.file(),
        Boot::Initialise,
    )
    .unwrap();
    assert_eq!(table.list(), before);
    assert!(matches!(
        table.acquire(request(601)),
        Err(AcquireError::WallAboveCeiling { .. })
    ));
    assert_eq!(granted.ends_at, at(T0 + 600_000));
    clock.advance(Duration::from_millis(599_999));
    assert!(table.tick().expired.is_empty());
    clock.advance(Duration::from_millis(1));
    assert_eq!(table.tick().expired, vec![granted.id]);
}
