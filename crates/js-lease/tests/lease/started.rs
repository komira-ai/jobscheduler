//! When a lease started: the moment its ttl and maximum wall time started counting, which
//! the file keeps so that a provider restored from it ([`read_bookings`]) can count a
//! lease's expected end from its start. A lease started at its acquire writes nothing new;
//! a whole-node lease observed ready later writes `started_at_ms`.

use std::fs;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, read_bookings};
use js_provider::Call;
use js_types::LeaseId;

use crate::support::{Scratch, T0, at, clock, fake_with_plain, request, secs, settings, states};

type PlainTable = LeaseTable<js_provider_fake::FakeProvider, ManualClock>;

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
