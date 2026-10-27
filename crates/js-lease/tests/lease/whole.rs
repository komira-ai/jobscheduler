//! Whole-node leases: pending until the provider is seen to have them ready, and a ttl
//! and maximum wall time that count from that moment, not from the commit.

use std::fs;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, OpenError, Settings, SettingsError};
use js_provider::{Call, Provider, StartSpec};
use js_types::LeaseId;

use crate::support::{
    Scratch, T0, at, clock, fake, fake_with_plain, open_err, plain_class, plain_request, request,
    secs, settings, states,
};

fn id() -> LeaseId {
    LeaseId::new(1, 1)
}

type PlainTable = LeaseTable<js_provider_fake::FakeProvider, ManualClock>;

fn table(name: &str, settings: Settings) -> (PlainTable, ManualClock, Scratch) {
    let scratch = Scratch::new(name);
    let clock = clock();
    let opened = LeaseTable::open(
        fake_with_plain(),
        clock.clone(),
        settings,
        scratch.file(),
        Boot::Initialise,
    );
    (opened.unwrap(), clock, scratch)
}

/// A whole lease whose start answered Unknown but landed: pending, booked.
fn pending_whole(table: &mut PlainTable, max_wall_secs: u64) {
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(max_wall_secs), None).unwrap_err();
    assert_eq!(states(table), vec![(id(), LeaseState::Pending)]);
}

fn info(table: &PlainTable) -> js_lease::LeaseInfo {
    table.list().remove(0)
}

/// Asserts that a pending whole lease is not expired by its own ttl: ten times the ttl
/// has passed with the provider still answering Unknown and the lease stays pending,
/// with the prepare deadline as its expiry. A ttl counted from the commit would expire
/// it at 60 s.
#[test]
fn a_pending_whole_lease_outlives_its_ttl() {
    let (mut table, clock, _scratch) = table("whole_pending", settings());
    pending_whole(&mut table, 3600);
    assert_eq!(info(&table).expires_at, at(T0 + 600_000));
    for _ in 0..4 {
        clock.advance(secs(140));
        table.provider_mut().script_unknown(Call::Observe, false);
        let report = table.tick();
        assert!(!report.changed());
        assert_eq!(report.pending, 1);
    }
    assert_eq!(states(&table), vec![(id(), LeaseState::Pending)]);
    assert_eq!(table.provider().booked().len(), 1);
}

/// Asserts that when the provider is first seen to have a pending whole lease ready, at
/// T, the lease is held and its expiry is exactly T + ttl and its end T + max wall: the
/// clock starts at the token. Acquisition time stays the commit.
#[test]
fn the_clock_starts_when_the_lease_is_observed_ready() {
    let (mut table, clock, _scratch) = table("whole_token", settings());
    pending_whole(&mut table, 3600);
    clock.advance(secs(300));
    let report = table.tick();
    assert_eq!(report.confirmed, vec![id()]);
    let held = info(&table);
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(held.acquired_at, at(T0));
    assert_eq!(held.expires_at, at(T0 + 300_000 + 60_000));
    assert_eq!(held.ends_at, at(T0 + 300_000 + 3_600_000));
}

/// Asserts that the granted ttl (not the default) is what the clock starts with, and
/// that renewals after the token stop at token time + max wall exactly. A clock from the
/// commit would have ended the lease at T0 + 400 s, long before.
#[test]
fn renew_is_capped_at_token_time_plus_max_wall() {
    let (mut table, clock, _scratch) = table("whole_renew", settings());
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(400), Some(secs(90))).unwrap_err();
    clock.advance(secs(300));
    assert_eq!(table.tick().confirmed, vec![id()]);
    let held = info(&table);
    assert_eq!(held.expires_at, at(T0 + 300_000 + 90_000));
    assert_eq!(held.ends_at, at(T0 + 300_000 + 400_000));
    let end = at(T0 + 700_000);
    let mut seen = Vec::new();
    for _ in 0..4 {
        clock.advance(secs(50));
        seen.push(table.renew(&id(), Some(secs(120))).unwrap());
        clock.advance(secs(50));
    }
    let want = [470, 570, 670, 700];
    let want: Vec<_> = want.iter().map(|s| at(T0 + s * 1000)).collect();
    assert_eq!(seen, want);
    assert_eq!(info(&table).ends_at, end);
}

/// Asserts that a whole lease the provider never reports ready is expired and stopped
/// at the prepare timeout and not before, and that the tick lists it.
#[test]
fn a_whole_lease_never_ready_expires_at_the_prepare_timeout() {
    let (mut table, clock, _scratch) = table("whole_never", settings());
    pending_whole(&mut table, 3600);
    clock.advance(secs(599));
    table.provider_mut().script_unknown(Call::Observe, false);
    assert!(!table.tick().changed());
    clock.advance(secs(1));
    let report = table.tick();
    assert_eq!(report.expired, vec![id()]);
    assert_eq!(report.freed, vec![id()]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that the prepare timeout is a setting, and that a pending lease never waits
/// past its maximum wall time even when the timeout is longer.
#[test]
fn the_prepare_timeout_is_a_setting_bounded_by_the_wall_time() {
    let longer = settings().with_prepare_timeout(secs(1000)).unwrap();
    assert_eq!(longer.prepare_timeout(), secs(1000));
    let (mut table, _clock, _scratch) = table("whole_prepare", longer);
    pending_whole(&mut table, 100);
    assert_eq!(info(&table).expires_at, at(T0 + 100_000));
    let shorter = settings().with_prepare_timeout(secs(30)).unwrap();
    let (mut table, _clock, _scratch) = self::table("whole_prepare_short", shorter);
    pending_whole(&mut table, 3600);
    assert_eq!(info(&table).expires_at, at(T0 + 30_000));
}

/// Asserts the default prepare timeout is ten minutes and a zero one is refused.
#[test]
fn the_default_prepare_timeout_is_ten_minutes_and_zero_is_refused() {
    assert_eq!(settings().prepare_timeout(), secs(600));
    let zero = settings().with_prepare_timeout(secs(0));
    assert_eq!(zero.unwrap_err(), SettingsError::PrepareNotPositive);
}

/// Asserts that a whole lease whose start answers Started at once has the timestamps a
/// lease always had: ttl and max wall from the commit, which is the token.
#[test]
fn a_whole_lease_started_at_once_has_todays_timestamps() {
    let (mut table, _clock, _scratch) = table("whole_started", settings());
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(lease.expires_at, at(T0 + 60_000));
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
}

/// Regression pin: a lease of a class that is not whole-node keeps its exact timestamps
/// whether it starts at once or pending: the ttl counts from the commit, a pending one
/// expires at the ttl, and an observation that confirms it moves nothing.
#[test]
fn a_plain_lease_keeps_todays_timestamps() {
    let (mut table, clock, _scratch) = table("plain", settings());
    let lease = table.acquire(plain_request(3600), None).unwrap();
    assert_eq!(lease.expires_at, at(T0 + 60_000));
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
    table.release(&lease.id).unwrap();

    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(plain_request(3600), None).unwrap_err();
    let second = LeaseId::new(1, 2);
    assert_eq!(info(&table).expires_at, at(T0 + 60_000));
    clock.advance(secs(30));
    table.provider_mut().script_unknown(Call::Observe, false);
    table.tick();
    clock.advance(secs(10));
    assert_eq!(table.tick().confirmed, vec![second]);
    let held = info(&table);
    assert_eq!(held.expires_at, at(T0 + 60_000));
    assert_eq!(held.ends_at, at(T0 + 3_600_000));
    clock.advance(secs(20));
    assert_eq!(table.tick().expired, vec![second]);
}

/// Asserts that a pending whole lease restored after a restart keeps its prepare
/// deadline and still starts its clock at the token, with the ttl and wall time it was
/// granted; and that once held, a restart that observes it does not start the clock
/// again. The file shows the additive fields.
#[test]
fn a_pending_whole_lease_survives_a_restart() {
    let scratch = Scratch::new("whole_restart");
    let clock = clock();
    let mut first = LeaseTable::open(
        fake_with_plain(),
        clock.clone(),
        settings(),
        scratch.file(),
        Boot::Initialise,
    )
    .unwrap();
    first.provider_mut().script_unknown(Call::Start, true);
    first.acquire(request(3600), Some(secs(90))).unwrap_err();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("whole = true"));
    assert!(text.contains("ttl_ms = 90000"));
    assert!(text.contains("wall_ms = 3600000"));
    let provider = first.into_provider();

    clock.advance(secs(100));
    let mut second = LeaseTable::open(
        provider,
        clock.clone(),
        settings(),
        scratch.file(),
        Boot::Resume,
    )
    .unwrap();
    assert_eq!(info(&second).expires_at, at(T0 + 600_000));
    clock.advance(secs(100));
    assert_eq!(second.tick().confirmed, vec![id()]);
    let held = info(&second);
    assert_eq!(held.expires_at, at(T0 + 200_000 + 90_000));
    assert_eq!(held.ends_at, at(T0 + 200_000 + 3_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("whole = true"));
    assert!(!text.contains("ttl_ms"));
    let provider = second.into_provider();

    clock.advance(secs(10));
    let third = LeaseTable::open(provider, clock, settings(), scratch.file(), Boot::Resume);
    assert_eq!(info(&third.unwrap()), held);
}

const OLD_PENDING: &str = "version = 1\nterm = 1\nnext_sequence = 2\nlease_count = 1\n\n\
    [[lease]]\nterm = 1\nsequence = 1\nstate = \"pending\"\n\
    acquired_at_ms = 1000000\nexpires_at_ms = 1060000\nends_at_ms = 4600000\n";

/// Asserts that a file written before the whole fields existed loads as not whole: a
/// pending lease in it keeps its stored expiry and end when it is confirmed.
#[test]
fn an_old_file_loads_as_not_whole() {
    let scratch = Scratch::new("whole_old");
    fs::write(scratch.file(), OLD_PENDING).unwrap();
    let mut provider = fake();
    let spec = StartSpec::new(id(), request(3600), &[js_types::ResourceClass::gpu_host()]);
    provider.start(spec.unwrap());
    let clock = clock();
    let boot = Boot::Resume;
    let mut table =
        LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot).unwrap();
    clock.advance(secs(30));
    assert_eq!(table.tick().confirmed, vec![LeaseId::new(1, 1)]);
    let held = info_of(&table);
    assert_eq!(held.expires_at, at(1_060_000));
    assert_eq!(held.ends_at, at(4_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("whole"));
}

fn info_of(table: &LeaseTable<js_provider_fake::FakeProvider, ManualClock>) -> js_lease::LeaseInfo {
    table.list().remove(0)
}

/// Asserts that a pending whole lease with no ttl or wall time in its file is refused
/// as corrupt, naming the lease, rather than loaded with a clock that could not start.
#[test]
fn a_pending_whole_lease_without_its_grant_is_corrupt() {
    let scratch = Scratch::new("whole_corrupt");
    let text = format!("{OLD_PENDING}whole = true\nttl_ms = 60000\n");
    fs::write(scratch.file(), text).unwrap();
    match open_err(&scratch.file()) {
        OpenError::Corrupt { reason, .. } => assert!(reason.contains("lacks ttl_ms or wall_ms")),
        other => panic!("unexpected {other}"),
    }
    let other = format!("{OLD_PENDING}whole = true\nwall_ms = 60000\n");
    fs::write(scratch.file(), other).unwrap();
    assert!(matches!(
        open_err(&scratch.file()),
        OpenError::Corrupt { .. }
    ));
}

/// Asserts that fields of a later version in a pending whole lease are written back
/// with the whole fields beside them.
#[test]
fn unknown_fields_survive_beside_the_whole_fields() {
    let scratch = Scratch::new("whole_extra");
    let text =
        format!("{OLD_PENDING}whole = true\nttl_ms = 60000\nwall_ms = 3600000\npriority = 3\n");
    fs::write(scratch.file(), text).unwrap();
    let mut provider = fake();
    let classes = [plain_class(), js_types::ResourceClass::gpu_host()];
    provider.start(StartSpec::new(id(), request(3600), &classes).unwrap());
    let clock = clock();
    let boot = Boot::Resume;
    let mut table =
        LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot).unwrap();
    clock.advance(secs(5));
    table.tick();
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    assert!(rewritten.contains("priority = 3"));
    assert!(rewritten.contains("whole = true"));
}
