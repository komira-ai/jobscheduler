//! Whole-node leases: pending until the provider is seen to have them ready, and a maximum
//! wall time that counts from that moment, not from the commit.

use std::fs;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, OpenError, Settings, SettingsError};
use js_provider::{Call, Pool, StartSpec};
use js_types::LeaseId;

use crate::support::{
    Scratch, T0, at, clock, fake, fake_with_plain, gpu_host_request, open_err, plain_class,
    plain_request, request, secs, settings, states,
};

fn id() -> LeaseId {
    LeaseId::new(1, 1)
}

type PlainTable = LeaseTable<js_provider_fake::FakePool, ManualClock>;

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
    table.acquire(request(max_wall_secs)).unwrap_err();
    assert_eq!(states(table), vec![(id(), LeaseState::Pending)]);
}

fn info(table: &PlainTable) -> js_lease::LeaseInfo {
    table.list().remove(0)
}

/// Asserts that a pending whole lease waits for its prepare deadline and nothing sooner:
/// 560 s pass (in four ticks) with the provider still answering Unknown and the lease stays
/// pending, with the prepare deadline (the acquire plus 600 s) in its expiry row of the
/// file. A deadline counted from a time to live of 60 s would have ended it.
#[test]
fn a_pending_whole_lease_stays_pending_until_its_prepare_deadline() {
    let (mut table, clock, scratch) = table("whole_pending", settings());
    pending_whole(&mut table, 3600);
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("expires_at_ms = 1600000\n"), "{text}");
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
/// T, the lease is held and its end is exactly T + max wall, which its expiry row in the
/// file repeats: the clock starts at the token. Acquisition time stays the commit.
#[test]
fn the_clock_starts_when_the_lease_is_observed_ready() {
    let (mut table, clock, scratch) = table("whole_token", settings());
    pending_whole(&mut table, 3600);
    clock.advance(secs(300));
    let report = table.tick();
    assert_eq!(report.confirmed, vec![id()]);
    let held = info(&table);
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(held.acquired_at, at(T0));
    assert_eq!(held.ends_at, at(T0 + 300_000 + 3_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(
        text.contains("expires_at_ms = 4900000\nends_at_ms = 4900000\n"),
        "{text}"
    );
}

/// Asserts that the wall time counts from the token: a lease with a wall time of 400 s that
/// is observed ready at 300 s ends at 700 s, not at 400 s, is still held a second before
/// that and is expired by the tick at it. A clock from the commit would have ended the
/// lease at 400 s.
#[test]
fn the_wall_end_is_the_token_time_plus_max_wall() {
    let (mut table, clock, _scratch) = table("whole_wall_end", settings());
    pending_whole(&mut table, 400);
    clock.advance(secs(300));
    assert_eq!(table.tick().confirmed, vec![id()]);
    assert_eq!(info(&table).ends_at, at(T0 + 700_000));
    clock.advance(secs(399));
    let report = table.tick();
    assert!(report.expired.is_empty(), "{report:?}");
    assert_eq!(states(&table), vec![(id(), LeaseState::Held)]);
    clock.advance(secs(1));
    assert_eq!(table.tick().expired, vec![id()]);
}

/// Asserts that a pending whole lease whose provider never answers still ends at its
/// prepare deadline, observe first: a tick before the deadline observes it and keeps it
/// pending, the tick at the deadline observes it once and does not expire it, and the
/// next tick expires and stops it, reporting it expired and freed. It catches a table that
/// stops the lease at its deadline without asking the provider, one that never ends a
/// lease the provider is silent about, and one that ends it before the deadline.
#[test]
fn a_pending_whole_lease_still_ends_at_its_prepare_deadline() {
    let (mut table, clock, _scratch) = table("whole_never", settings());
    pending_whole(&mut table, 3600);
    clock.advance(secs(599));
    table.provider_mut().script_unknown(Call::Observe, false);
    let early = table.tick();
    assert!(!early.changed());
    assert_eq!(early.pending, 1);
    assert_eq!(states(&table), vec![(id(), LeaseState::Pending)]);
    clock.advance(secs(1));
    table.provider_mut().script_unknown(Call::Observe, false);
    let report = table.tick();
    assert!(report.expired.is_empty(), "{report:?}");
    assert_eq!(states(&table), vec![(id(), LeaseState::Pending)]);
    clock.advance(secs(5));
    let report = table.tick();
    assert_eq!(report.expired, vec![id()]);
    assert_eq!(report.freed, vec![id()]);
    assert!(table.list().is_empty());
    assert!(table.provider().booked().is_empty());
}

/// Asserts that at the deadline the provider's answer decides: an observation that finds
/// the lease gone drops it with no stop from the table, which is how a provider that
/// ended the lease as a failed prepare is heard. It catches an expiry that runs before
/// the observation and so stops a lease the provider has already ended.
#[test]
fn a_pending_whole_lease_the_provider_ended_at_its_deadline_is_dropped_not_expired() {
    let (mut table, clock, _scratch) = table("whole_ended", settings());
    pending_whole(&mut table, 3600);
    clock.advance(secs(600));
    table.provider_mut().stop(&id());
    let report = table.tick();
    assert_eq!(report.dropped, vec![id()]);
    assert!(report.expired.is_empty(), "{report:?}");
    assert!(table.list().is_empty());
}

/// Asserts that the prepare timeout is a setting, and that a pending whole lease whose
/// maximum wall time is shorter than the timeout stays pending past that wall time; once
/// it is ready its end is the ready time plus the wall time.
#[test]
fn the_prepare_timeout_is_a_setting_and_the_wall_time_does_not_clamp_a_pending_lease() {
    let longer = settings().with_prepare_timeout(secs(1000)).unwrap();
    assert_eq!(longer.prepare_timeout(), secs(1000));
    let (mut table, clock, scratch) = table("whole_prepare", longer);
    pending_whole(&mut table, 40);
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("expires_at_ms = 2000000\n"), "{text}");
    clock.advance(secs(150));
    table.provider_mut().script_unknown(Call::Observe, false);
    let report = table.tick();
    assert!(report.expired.is_empty());
    assert_eq!(states(&table), vec![(id(), LeaseState::Pending)]);
    clock.advance(secs(50));
    assert_eq!(table.tick().confirmed, vec![id()]);
    let held = info(&table);
    assert_eq!(held.state, LeaseState::Held);
    assert_eq!(held.ends_at, at(T0 + 200_000 + 40_000));
    let shorter = settings().with_prepare_timeout(secs(30)).unwrap();
    let (mut table, _clock, scratch) = self::table("whole_prepare_short", shorter);
    pending_whole(&mut table, 3600);
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("expires_at_ms = 1030000\n"), "{text}");
}

/// Asserts the default prepare timeout is ten minutes and a zero one is refused.
#[test]
fn the_default_prepare_timeout_is_ten_minutes_and_zero_is_refused() {
    assert_eq!(settings().prepare_timeout(), secs(600));
    let zero = settings().with_prepare_timeout(secs(0));
    assert_eq!(zero.unwrap_err(), SettingsError::PrepareNotPositive);
}

/// Asserts that a whole lease whose start answers Started at once has the timestamps a
/// lease always had: its maximum wall time from the commit, which is the token.
#[test]
fn a_whole_lease_started_at_once_has_todays_timestamps() {
    let (mut table, _clock, _scratch) = table("whole_started", settings());
    let lease = table.acquire(request(3600)).unwrap();
    assert_eq!(lease.ends_at, at(T0 + 3_600_000));
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
}

/// Asserts that a `gpu-host` lease is admitted `BookAll`, so it is not whole-node: its
/// start is granted with a token at once, and one that answers Unknown is not given the
/// whole-node waiting rules. It fails if `gpu-host` were `Whole`.
#[test]
fn a_gpu_host_lease_is_not_pending_when_none_exists() {
    let (mut table, _clock, scratch) = table("gpu_host", settings());
    let lease = table.acquire(gpu_host_request(3600)).unwrap();
    assert_eq!(states(&table), vec![(lease.id, LeaseState::Held)]);
    assert_eq!(lease.ends_at, at(T0 + 3_600_000));
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
    table.release(&lease.id).unwrap();

    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(gpu_host_request(3600)).unwrap_err();
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("whole"));
}

/// Regression pin: a lease of a class that is not whole-node has its wall time counted
/// from the commit whether it starts at once or pending: its end is the commit plus the wall
/// time, an observation that confirms it moves nothing, and the tick at that end expires it.
#[test]
fn a_plain_lease_keeps_todays_timestamps() {
    let (mut table, clock, _scratch) = table("plain", settings());
    let lease = table.acquire(plain_request(3600)).unwrap();
    assert_eq!(lease.ends_at, at(T0 + 3_600_000));
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
    table.release(&lease.id).unwrap();

    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(plain_request(3600)).unwrap_err();
    let second = LeaseId::new(1, 2);
    assert_eq!(info(&table).ends_at, at(T0 + 3_600_000));
    clock.advance(secs(30));
    table.provider_mut().script_unknown(Call::Observe, false);
    table.tick();
    clock.advance(secs(10));
    assert_eq!(table.tick().confirmed, vec![second]);
    let held = info(&table);
    assert_eq!(held.ends_at, at(T0 + 3_600_000));
    clock.advance(secs(3_559));
    assert!(table.tick().expired.is_empty());
    clock.advance(secs(1));
    assert_eq!(table.tick().expired, vec![second]);
}

/// Asserts that a pending whole lease restored after a restart keeps its prepare
/// deadline and still starts its clock at the token, with the wall time it was asked
/// for; and that once held, a restart that observes it does not start the clock again.
/// The file shows the additive fields and never a `ttl_ms`.
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
    first.acquire(request(3600)).unwrap_err();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("whole = true"));
    assert!(!text.contains("ttl_ms"), "{text}");
    assert!(text.contains("wall_ms = 3600000"));
    assert!(text.contains("expires_at_ms = 1600000\n"), "{text}");
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
    assert_eq!(states(&second), vec![(id(), LeaseState::Pending)]);
    clock.advance(secs(100));
    assert_eq!(second.tick().confirmed, vec![id()]);
    let held = info(&second);
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
/// pending lease in it keeps its stored end when it is confirmed, and its stored expiry,
/// which is no longer read, is rewritten as that end.
#[test]
fn an_old_file_loads_as_not_whole() {
    let scratch = Scratch::new("whole_old");
    fs::write(scratch.file(), OLD_PENDING).unwrap();
    let mut provider = fake();
    let spec = StartSpec::new(
        id(),
        request(3600),
        &[js_types::ResourceClass::whole_node()],
    );
    provider.start(spec.unwrap());
    let clock = clock();
    let boot = Boot::Resume;
    let mut table =
        LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot).unwrap();
    clock.advance(secs(30));
    assert_eq!(table.tick().confirmed, vec![LeaseId::new(1, 1)]);
    let held = info_of(&table);
    assert_eq!(held.ends_at, at(4_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("whole"));
    assert!(text.contains("expires_at_ms = 4600000\n"), "{text}");
}

fn info_of(table: &LeaseTable<js_provider_fake::FakePool, ManualClock>) -> js_lease::LeaseInfo {
    table.list().remove(0)
}

/// Asserts that a pending whole lease with no wall time in its file is refused as corrupt,
/// naming the lease, rather than loaded with a clock that could not start (with or without
/// the `ttl_ms` a previous release kept), and that one with only its wall time, as this
/// release writes it, loads pending with its prepare deadline kept and starts its clock
/// when it is ready. It fails if the old `ttl_ms` is still required, or if a missing wall
/// time is accepted.
#[test]
fn a_pending_whole_lease_needs_its_wall_time_and_not_a_ttl() {
    let scratch = Scratch::new("whole_corrupt");
    for tail in ["whole = true\nttl_ms = 60000\n", "whole = true\n"] {
        fs::write(scratch.file(), format!("{OLD_PENDING}{tail}")).unwrap();
        match open_err(&scratch.file()) {
            OpenError::Corrupt { reason, .. } => assert!(reason.contains("lacks wall_ms")),
            other => panic!("unexpected {other}"),
        }
    }
    let wall_only = format!("{OLD_PENDING}whole = true\nwall_ms = 60000\n");
    fs::write(scratch.file(), wall_only).unwrap();
    let mut provider = fake();
    let classes = [js_types::ResourceClass::whole_node()];
    provider.start(StartSpec::new(id(), request(60), &classes).unwrap());
    let clock = clock();
    let boot = Boot::Resume;
    let mut table =
        LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot).unwrap();
    assert_eq!(states(&table), vec![(id(), LeaseState::Pending)]);
    clock.advance(secs(5));
    assert_eq!(table.tick().confirmed, vec![id()]);
    assert_eq!(info(&table).ends_at, at(T0 + 5_000 + 60_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("ttl_ms"), "{text}");
}

/// Asserts that fields of a later version in a pending whole lease are written back
/// with the whole fields beside them, and that its `ttl_ms` is read and not written back.
#[test]
fn unknown_fields_survive_beside_the_whole_fields() {
    let scratch = Scratch::new("whole_extra");
    let text =
        format!("{OLD_PENDING}whole = true\nttl_ms = 60000\nwall_ms = 3600000\npriority = 3\n");
    fs::write(scratch.file(), text).unwrap();
    let mut provider = fake();
    let classes = [plain_class(), js_types::ResourceClass::whole_node()];
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
    assert!(!rewritten.contains("ttl_ms"), "{rewritten}");
}
