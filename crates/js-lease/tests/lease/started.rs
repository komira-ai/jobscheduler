//! When a lease started: the moment its ttl and maximum wall time started counting, which
//! the file keeps so that a provider restored from it ([`read_bookings`]) can count a
//! lease's expected end from its start. A lease started at its acquire writes nothing new;
//! a whole-node lease observed ready later writes `started_at_ms`.

use std::fs;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, OpenError, Timestamp, read_bookings};
use js_provider::Call;
use js_types::LeaseId;

use crate::support::{
    Scratch, T0, at, break_dir, clock, fake_with_plain, open_err, request, secs, settings, states,
};

type PlainTable = LeaseTable<js_provider_fake::FakePool, ManualClock>;

fn table(scratch: &Scratch, clock: &ManualClock, boot: Boot) -> PlainTable {
    let provider = fake_with_plain();
    let opened = LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot);
    opened.unwrap()
}

/// How long after its acquire the whole lease of [`ready_later`] is observed ready.
pub(crate) const READY_AFTER_MS: u64 = 300_000;

/// A table over `scratch` with one whole-node lease of an hour, acquired at `T0` with a
/// start that answered Unknown but landed, and observed ready [`READY_AFTER_MS`] later.
pub(crate) fn ready_later(scratch: &Scratch, clock: &ManualClock) -> PlainTable {
    let mut table = table(scratch, clock, Boot::Initialise);
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(3600), None).unwrap_err();
    clock.advance(secs(READY_AFTER_MS / 1000));
    assert_eq!(table.tick().confirmed, vec![LeaseId::new(1, 1)]);
    table
}

/// Asserts that a whole-node lease observed ready after its acquire writes the moment it
/// was ready as `started_at_ms`, reads back with that start and the end one maximum wall
/// time after it, and keeps the start across a restart that observes it again and writes
/// the file anew. It catches a start that is not written, a start read as the acquire,
/// and a restart that drops the field or moves it to the time of the restart.
#[test]
fn a_lease_ready_after_its_acquire_keeps_its_start_through_the_file() {
    let scratch = Scratch::new("started_ready_later");
    let clock = clock();
    let first = ready_later(&scratch, &clock);
    let ready = T0 + READY_AFTER_MS;
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains(&format!("acquired_at_ms = {T0}\n")), "{text}");
    assert!(
        text.contains(&format!("started_at_ms = {ready}\n")),
        "{text}"
    );
    let bookings = read_bookings(scratch.file()).unwrap();
    assert_eq!(bookings.len(), 1);
    assert_eq!(bookings[0].started_at, at(ready));
    assert_eq!(bookings[0].ends_at, at(ready + 3_600_000));

    // Within the lease's 60 s ttl, so the restart finds it held and a renewal saves.
    let provider = first.into_provider();
    clock.advance(secs(30));
    let boot = Boot::Resume;
    let mut second = LeaseTable::open(provider, clock.clone(), settings(), scratch.file(), boot);
    let second = second.as_mut().unwrap();
    assert_eq!(states(second), vec![(LeaseId::new(1, 1), LeaseState::Held)]);
    second.renew(&LeaseId::new(1, 1), Some(secs(60))).unwrap();
    let again = fs::read_to_string(scratch.file()).unwrap();
    assert!(
        again.contains(&format!("started_at_ms = {ready}\n")),
        "{again}"
    );
    assert_eq!(read_bookings(scratch.file()).unwrap(), bookings);
}

/// Asserts that a lease whose start answers at once started at its acquire: the file gains
/// no key (so it is written as before the field existed) and the booking reads its acquire
/// as its start, its end one maximum wall time later. It catches a key written for every
/// lease, which would change the file of a release that never needs it.
#[test]
fn a_lease_started_at_its_acquire_writes_no_start_and_reads_its_acquire() {
    let scratch = Scratch::new("started_at_acquire");
    let clock = clock();
    let mut table = table(&scratch, &clock, Boot::Initialise);
    table.acquire(request(3600), None).unwrap();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("started_at_ms"), "{text}");
    let bookings = read_bookings(scratch.file()).unwrap();
    assert_eq!(bookings[0].started_at, at(T0));
    assert_eq!(bookings[0].ends_at, at(T0 + 3_600_000));
}

/// A held lease acquired at 1 000 000 with an end of 4 600 000, and `extra` lines.
fn held_file(extra: &str) -> String {
    format!(
        "version = 1\nterm = 1\nnext_sequence = 2\nlease_count = 1\n\n\
         [[lease]]\nterm = 1\nsequence = 1\nstate = \"held\"\nhost = \"h1\"\n\
         acquired_at_ms = 1000000\nexpires_at_ms = 1060000\nends_at_ms = 4600000\n{extra}"
    )
}

/// Asserts how a held lease's start is read from a file: absent (a file from before the
/// field) is its acquire, and present is the value written. It catches a missing key read
/// as zero or refused, and a present one ignored.
#[test]
fn the_start_is_the_key_when_written_and_the_acquire_otherwise() {
    let scratch = Scratch::new("started_read");
    fs::write(scratch.file(), held_file("")).unwrap();
    let old = read_bookings(scratch.file()).unwrap();
    assert_eq!(old[0].started_at, at(1_000_000));
    assert_eq!(old[0].ends_at, at(4_600_000));
    fs::write(scratch.file(), held_file("started_at_ms = 1000500\n")).unwrap();
    let new = read_bookings(scratch.file()).unwrap();
    assert_eq!(new[0].started_at, at(1_000_500));
}

/// A held lease whose `started_at_ms` is `value`, as a whole file.
fn with_start(value: &str) -> String {
    held_file(&format!("started_at_ms = {value}\n"))
}

/// Asserts that a `started_at_ms` that is not a millisecond count a timestamp holds (a
/// string, a negative number, a key given twice, a number past what a TOML integer holds)
/// makes the file corrupt, through `read_bookings` and through opening the table, with a
/// reason that names the field, and that neither writes the file. It catches a bad value
/// read as the acquire or as zero, and a corrupt file rewritten.
#[test]
fn a_start_that_is_not_a_millisecond_count_is_a_corrupt_file() {
    for (name, text) in [
        ("started_text", with_start("\"soon\"")),
        ("started_negative", with_start("-1")),
        (
            "started_twice",
            with_start("1000500\nstarted_at_ms = 1000600"),
        ),
        ("started_huge", with_start("18446744073709551616")),
    ] {
        let scratch = Scratch::new(name);
        fs::write(scratch.file(), &text).unwrap();
        match read_bookings(scratch.file()) {
            Err(OpenError::Corrupt { reason, .. }) => {
                assert!(reason.contains("started_at_ms"), "{name}: {reason}");
            }
            other => panic!("{name}: {other:?}"),
        }
        match open_err(&scratch.file()) {
            OpenError::Corrupt { reason, .. } => {
                assert!(reason.contains("started_at_ms"), "{name}: {reason}");
            }
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(fs::read_to_string(scratch.file()).unwrap(), text, "{name}");
    }
}

/// Asserts that when the save made as a whole-node lease is seen ready fails, the start
/// recorded is still the moment it was ready: the tick reports the failed save, the clock
/// moves on, and the next tick, once the file can be written, writes the first ready time
/// and the end one maximum wall time after it. It catches a start taken at the save that
/// worked rather than at the observation.
#[test]
fn a_failed_save_at_readiness_keeps_the_ready_time_for_the_next_save() {
    let scratch = Scratch::new("started_save_fails");
    let clock = clock();
    let mut table = table(&scratch, &clock, Boot::Initialise);
    table.provider_mut().script_unknown(Call::Start, true);
    table.acquire(request(3600), None).unwrap_err();
    clock.advance(secs(READY_AFTER_MS / 1000));
    break_dir(&scratch);
    let report = table.tick();
    assert_eq!(report.confirmed, vec![LeaseId::new(1, 1)]);
    assert!(report.save_error.is_some());
    clock.advance(secs(20));
    fs::create_dir(&scratch.dir).unwrap();
    assert!(table.tick().save_error.is_none());
    let ready = T0 + READY_AFTER_MS;
    let bookings = read_bookings(scratch.file()).unwrap();
    assert_eq!(bookings[0].started_at, at(ready));
    assert_eq!(bookings[0].ends_at, at(ready + 3_600_000));
}

/// Asserts the start across restarts while a whole-node lease is pending and while it is
/// releasing. Pending at a restart, it starts when it is seen ready after it, and a second
/// restart that observes it again moves nothing; released with its stop unanswered, it is
/// put back releasing with the same start and end, and its rewrite keeps them. It catches a
/// start taken at a restart, a clock started twice, and a releasing lease that loses its
/// start.
#[test]
fn the_start_survives_restarts_while_pending_and_while_releasing() {
    let scratch = Scratch::new("started_restarts");
    let clock = clock();
    let mut first = table(&scratch, &clock, Boot::Initialise);
    first.provider_mut().script_unknown(Call::Start, true);
    first.acquire(request(3600), None).unwrap_err();
    let provider = first.into_provider();
    clock.advance(secs(100));
    let resume = |provider, clock: &ManualClock| {
        LeaseTable::open(
            provider,
            clock.clone(),
            settings(),
            scratch.file(),
            Boot::Resume,
        )
        .unwrap()
    };
    let mut second = resume(provider, &clock);
    clock.advance(secs(100));
    assert_eq!(second.tick().confirmed, vec![LeaseId::new(1, 1)]);
    let ready = T0 + 200_000;
    let bookings = read_bookings(scratch.file()).unwrap();
    assert_eq!(bookings[0].started_at, at(ready));
    assert_eq!(bookings[0].ends_at, at(ready + 3_600_000));
    let provider = second.into_provider();
    clock.advance(secs(10));
    let mut third = resume(provider, &clock);
    third.tick();
    assert_eq!(read_bookings(scratch.file()).unwrap(), bookings);

    third.provider_mut().script_unknown(Call::Stop, false);
    third.release(&LeaseId::new(1, 1)).unwrap();
    let provider = third.into_provider();
    clock.advance(secs(10));
    let mut fourth = resume(provider, &clock);
    fourth.provider_mut().script_unknown(Call::Stop, false);
    fourth.tick();
    let releasing = read_bookings(scratch.file()).unwrap();
    assert_eq!(releasing[0].state, LeaseState::Releasing);
    assert_eq!(releasing[0].started_at, at(ready));
    assert_eq!(releasing[0].ends_at, at(ready + 3_600_000));
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(
        text.contains(&format!("started_at_ms = {ready}\n")),
        "{text}"
    );
}

/// Asserts the start at the edges of the clock: a whole-node lease seen ready at the very
/// instant it was acquired started at its acquire (no key written); one seen ready after
/// the clock stepped back started before its acquire (the key written, read back as
/// given); and one whose maximum wall time is endless ends at the largest instant a
/// timestamp holds. It catches a start clamped to the acquire, a key compared the wrong way
/// round, and an end that overflows.
#[test]
fn the_start_at_the_edges_of_the_clock() {
    let ready_at = |name: &str, step: &dyn Fn(&ManualClock), wall_secs: u64| {
        let scratch = Scratch::new(name);
        let clock = clock();
        let mut table = table(&scratch, &clock, Boot::Initialise);
        table.provider_mut().script_unknown(Call::Start, true);
        table.acquire(request(wall_secs), None).unwrap_err();
        step(&clock);
        assert_eq!(table.tick().confirmed, vec![LeaseId::new(1, 1)]);
        let text = fs::read_to_string(scratch.file()).unwrap();
        (text, read_bookings(scratch.file()).unwrap().remove(0))
    };
    let (text, same) = ready_at("started_same_instant", &|_| {}, 3600);
    assert!(!text.contains("started_at_ms"), "{text}");
    assert_eq!(
        (same.started_at, same.ends_at),
        (at(T0), at(T0 + 3_600_000))
    );

    let back = |clock: &ManualClock| clock.set(at(T0 - 50_000));
    let (text, early) = ready_at("started_clock_back", &back, 3600);
    assert!(
        text.contains(&format!("started_at_ms = {}\n", T0 - 50_000)),
        "{text}"
    );
    let expected = (at(T0 - 50_000), at(T0 - 50_000 + 3_600_000));
    assert_eq!((early.started_at, early.ends_at), expected);

    let later = |clock: &ManualClock| clock.advance(secs(READY_AFTER_MS / 1000));
    let (text, endless) = ready_at("started_endless", &later, u64::MAX / 1000);
    let max = Timestamp::MAX.millis();
    assert!(text.contains(&format!("ends_at_ms = {max}\n")), "{text}");
    assert_eq!(endless.started_at, at(T0 + READY_AFTER_MS));
    assert_eq!(endless.ends_at, Timestamp::MAX);
}
