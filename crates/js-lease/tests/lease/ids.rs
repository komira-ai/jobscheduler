//! The id limits: the file holds integers up to `i64::MAX`, so a term or a sequence
//! whose successor does not fit is the last one that can be issued.

use std::fs;

use js_lease::{AcquireError, Boot, LeaseState, LeaseTable, OpenError, ReleaseOutcome};
use js_types::LeaseId;

use crate::support::{Scratch, clock, fake, open_err, open_with, request, settings, states};

const LARGEST: u64 = i64::MAX.unsigned_abs();

fn head(term: u64, next_sequence: u64) -> String {
    format!("version = 1\nterm = {term}\nnext_sequence = {next_sequence}\nlease_count = 0\n")
}

/// Asserts the sequence limit: the file can hold integers up to `i64::MAX`, so a
/// sequence whose successor does not fit is the last. With `next_sequence` one below the
/// limit the acquire still works (and writes the limit); the one after it is refused as
/// exhausted, records nothing, does not call the provider and leaves the file as it was.
/// A hand-edited file with the limit, a value above it or the largest u64 opens and
/// refuses every acquire the same way, with no overflow.
#[test]
fn the_last_sequence_is_issued_and_the_next_is_refused() {
    let scratch = Scratch::new("sequence_limit");
    fs::write(scratch.file(), head(1, LARGEST - 1)).unwrap();
    let mut table = open_with(fake(), &scratch);

    let last = table.acquire(request(3600), None).unwrap();
    assert_eq!(last.id, LeaseId::new(2, LARGEST - 1));
    let written = fs::read_to_string(scratch.file()).unwrap();
    assert!(written.contains(&format!("next_sequence = {LARGEST}\n")));

    let err = table.acquire(request(3600), None).unwrap_err();
    assert!(matches!(err, AcquireError::IdsExhausted), "{err}");
    assert_eq!(states(&table), vec![(last.id, LeaseState::Held)]);
    assert_eq!(table.provider().booked().len(), 1);
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), written);
    assert_eq!(table.release(&last.id).unwrap(), ReleaseOutcome::Freed);
    drop(table);

    // A hand-edited file past the limit, up to the largest u64, opens and issues nothing.
    for edited in [LARGEST, LARGEST + 1, u64::MAX] {
        fs::write(scratch.file(), head(1, edited)).unwrap();
        let mut table = open_with(fake(), &scratch);
        let err = table.acquire(request(3600), None).unwrap_err();
        assert!(matches!(err, AcquireError::IdsExhausted), "{err}");
        assert!(table.list().is_empty());
        assert!(table.provider().booked().is_empty());
        drop(table);
        let after = fs::read_to_string(scratch.file()).unwrap();
        assert!(
            after.contains(&format!("next_sequence = {edited}\n")),
            "{after}"
        );
    }
}

/// Asserts that the table reports the term it issues ids in: 1 on the first boot of a
/// file and one more on every boot after, the number a caller puts into ids of its own so
/// that none repeats after a restart.
#[test]
fn the_table_reports_the_term_of_its_boot() {
    let scratch = Scratch::new("term_reported");
    let first = open_with(fake(), &scratch);
    assert_eq!(first.term(), 1);
    drop(first);
    let boot = Boot::Resume;
    let second = LeaseTable::open(fake(), clock(), settings(), scratch.file(), boot).unwrap();
    assert_eq!(second.term(), 2);
}

/// Asserts the term limit: a file whose term is the largest it can hold cannot be opened
/// for another boot, because the next term could not be written, and the file is not
/// touched; one term below it opens and takes the limit.
#[test]
fn the_last_term_is_taken_and_the_next_boot_is_refused() {
    let scratch = Scratch::new("term_limit");
    fs::write(scratch.file(), head(LARGEST - 1, 1)).unwrap();
    let mut table = open_with(fake(), &scratch);
    let lease = table.acquire(request(3600), None).unwrap();
    assert_eq!(lease.id, LeaseId::new(LARGEST, 1));
    let provider = table.into_provider();
    let written = fs::read_to_string(scratch.file()).unwrap();

    let boot = Boot::Resume;
    let err = LeaseTable::open(provider, clock(), settings(), scratch.file(), boot);
    let err = err.map(|_| ()).unwrap_err();
    assert!(matches!(&err, OpenError::TermExhausted { path } if *path == scratch.file()));
    assert!(err.to_string().starts_with(&format!(
        "lease file {}: the term cannot advance",
        scratch.file().display()
    )));
    assert_eq!(fs::read_to_string(scratch.file()).unwrap(), written);

    for edited in [LARGEST, u64::MAX] {
        fs::write(scratch.file(), head(edited, 1)).unwrap();
        let err = open_err(&scratch.file());
        assert!(matches!(err, OpenError::TermExhausted { .. }), "{err}");
    }
}
