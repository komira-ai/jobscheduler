//! The reservation ledger in the lease file: round trip, atomicity with the leases,
//! the restart grace, corrupt rows, old and newer files. The table stores records and
//! decides nothing about them, so no policy is tested here.

use std::fs;
use std::time::Duration;

use js_lease::{Boot, Exclusion, ExclusionEnd, Held, LeaseTable, OpenError, ReservationRecord};

use crate::support::{
    Scratch, T0, Table, at, clock, fake, fresh, host, open, open_err, request, secs, settings,
};

fn held(name: &str, reserved: u64, deadline: u64) -> Held {
    Held {
        host: host(name),
        reserved_at: at(reserved),
        deadline: at(deadline),
    }
}

fn record(id: &str) -> ReservationRecord {
    ReservationRecord {
        request_id: id.to_string(),
        held: None,
        give_ups: 0,
        prepare_failures: 0,
        exclusions: Vec::new(),
        claim_by: None,
    }
}

/// A held record with counters and both kinds of exclusion, and a counters-only one.
fn ledger() -> Vec<ReservationRecord> {
    let held_one = ReservationRecord {
        held: Some(held("h1", T0, T0 + 600_000)),
        give_ups: 2,
        prepare_failures: 1,
        exclusions: vec![
            Exclusion {
                host: host("h2"),
                until: ExclusionEnd::At(at(T0 + 5_000)),
            },
            Exclusion {
                host: host("h3"),
                until: ExclusionEnd::Forever,
            },
        ],
        ..record("req-a")
    };
    let between = ReservationRecord {
        give_ups: 4,
        ..record("req-b")
    };
    vec![held_one, between]
}

fn unclaimed(records: Vec<ReservationRecord>) -> Vec<ReservationRecord> {
    let strip = |mut r: ReservationRecord| {
        r.claim_by = None;
        r
    };
    records.into_iter().map(strip).collect()
}

fn reopen(table: Table, clock: &js_lease::ManualClock, scratch: &Scratch) -> Table {
    open(table.into_provider(), clock, scratch)
}

/// Asserts that records written and reopened are equal field for field (held part,
/// both counters, a timed and a forever exclusion, a counters-only record), that the
/// file carries the count, and that a restored record is stamped with `claim_by`.
#[test]
fn records_survive_a_reopen_field_for_field() {
    let (mut table, clock, scratch) = fresh("res_round");
    table.set_reservations(ledger());
    assert!(table.tick().save_error.is_none());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(text.contains("reservation_count = 2\n"), "{text}");

    let table = reopen(table, &clock, &scratch);
    let restored = table.reservations();
    assert_eq!(unclaimed(restored.clone()), ledger());
    assert!(restored.iter().all(|r| r.claim_by == Some(at(T0))));
}

/// Asserts that a record cannot reach the file without the leases written with it: one
/// acquire after `set_reservations` writes both in the same replace.
#[test]
fn leases_and_reservations_are_one_write() {
    let (mut table, _clock, scratch) = fresh("res_one_write");
    table.set_reservations(ledger());
    assert!(
        !fs::read_to_string(scratch.file())
            .unwrap()
            .contains("req-a")
    );
    table.acquire(request(3600), None).unwrap();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(
        text.contains("[[lease]]") && text.contains("req-a"),
        "{text}"
    );
}

/// Asserts that a file that counts two reservations but holds one is refused as corrupt
/// naming the path, and so are the rows that are not valid: a held part cut short, a
/// bad host, an exclusion with neither or both ends, a request twice.
#[test]
fn corrupt_reservation_rows_are_refused() {
    let scratch = Scratch::new("res_corrupt");
    let path = scratch.file();
    let head = |n: u32| {
        format!(
            "version = 1\nterm = 1\nnext_sequence = 1\nlease_count = 0\nreservation_count = {n}\n"
        )
    };
    let row = "\n[[reservation]]\nrequest_id = \"r\"\n";
    let held = "host = \"h1\"\nreserved_at_ms = 1\ndeadline_ms = 2\n";
    let excl = "\n[[reservation.exclusion]]\nhost = \"h2\"\n";
    let cases = [
        head(2) + row,
        head(0) + row,
        head(1) + row + "host = \"h1\"\n",
        head(1) + row + "deadline_ms = 2\nreserved_at_ms = 1\n",
        head(1) + row + &held.replace("h1", "BAD HOST"),
        head(1) + row + excl,
        head(1) + row + excl + "until_ms = 1\nforever = true\n",
        head(1) + row + &excl.replace("h2", "BAD HOST") + "forever = true\n",
        head(2) + row + row,
    ];
    for text in cases {
        fs::write(&path, &text).unwrap();
        let err = open_err(&path);
        assert!(matches!(err, OpenError::Corrupt { .. }), "{err}\n{text}");
        assert!(
            err.to_string()
                .starts_with(&format!("lease file {}: corrupt: ", path.display()))
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

/// Asserts that a file with no reservation rows (every file before this change) loads
/// an empty ledger, and that rewriting it adds no reservation text.
#[test]
fn a_file_without_rows_loads_empty() {
    let (mut table, clock, scratch) = fresh("res_old");
    table.acquire(request(3600), None).unwrap();
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("reservation"), "{text}");
    let table = reopen(table, &clock, &scratch);
    assert!(table.reservations().is_empty());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert!(!text.contains("reservation"), "{text}");
}

/// Asserts that fields a later version adds to a reservation row are kept when the
/// table rewrites the file, and that they go with the row when the row is dropped.
#[test]
fn unknown_reservation_fields_are_kept_on_rewrite() {
    let scratch = Scratch::new("res_newer");
    let text = "version = 2\nterm = 1\nnext_sequence = 1\nlease_count = 0\nreservation_count = 2\n\n\
        [[reservation]]\nrequest_id = \"keep\"\npriority = 7\n\n\
        [[reservation]]\nrequest_id = \"drop\"\nflavour = \"x\"\n";
    fs::write(scratch.file(), text).unwrap();
    let clock = clock();
    let mut table = open(fake(), &clock, &scratch);
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    assert!(rewritten.contains("priority = 7") && rewritten.contains("flavour = \"x\""));
    let mut records = table.reservations();
    records.retain(|r| r.request_id == "keep");
    table.set_reservations(records);
    table.tick();
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    assert!(rewritten.contains("reservation_count = 1\n"), "{rewritten}");
    assert!(rewritten.contains("priority = 7"));
    assert!(!rewritten.contains("flavour"));
}

/// Asserts the restart grace on a manual clock: after an open at the boot instant a held
/// deadline is exactly the boot plus the boot grace (the stored one is earlier), the
/// reserved instant and the counters are untouched, and `claim_by` is the boot plus the regain grace; a counters-only
/// record gets `claim_by` too.
#[test]
fn the_restart_grace_extends_deadlines_exactly() {
    let (mut table, clock, scratch) = fresh("res_grace");
    table.set_reservations(ledger());
    table.tick();
    drop(table);

    clock.advance(secs(1000));
    let graced = settings().with_restart_grace(secs(30), Duration::from_millis(90_500));
    let table =
        LeaseTable::open(fake(), clock.clone(), graced, scratch.file(), Boot::Resume).unwrap();
    let restored = table.reservations();
    let first = &restored[0];
    assert_eq!(first.held, Some(held("h1", T0, T0 + 1_030_000)));
    assert_eq!((first.give_ups, first.prepare_failures), (2, 1));
    assert_eq!(first.claim_by, Some(at(T0 + 1_090_500)));
    assert_eq!(restored[1].held, None);
    assert_eq!(restored[1].claim_by, Some(at(T0 + 1_090_500)));
}

/// Opens `scratch`'s file with `graced`, saves the restored ledger back and returns
/// the first record's deadline.
fn reopen_saving(
    clock: &js_lease::ManualClock,
    scratch: &Scratch,
    graced: js_lease::Settings,
) -> u64 {
    let mut table =
        LeaseTable::open(fake(), clock.clone(), graced, scratch.file(), Boot::Resume).unwrap();
    let deadline = table.reservations()[0].held.as_ref().unwrap().deadline;
    table.set_reservations(table.reservations());
    assert!(table.tick().save_error.is_none());
    deadline.millis()
}

/// Asserts that consecutive reopens, each saving the ledger back with the clock barely
/// moved, leave the deadline at the boot plus the grace without growing it, and that a
/// stored deadline already later than that is unchanged.
#[test]
fn repeated_reopens_do_not_accumulate_the_boot_grace() {
    let (mut table, clock, scratch) = fresh("res_loop");
    table.set_reservations(ledger());
    table.tick();
    drop(table);
    let graced = settings().with_restart_grace(secs(30), secs(0));

    clock.advance(secs(1000));
    assert_eq!(reopen_saving(&clock, &scratch, graced), T0 + 1_030_000);
    clock.advance(Duration::from_millis(10));
    assert_eq!(reopen_saving(&clock, &scratch, graced), T0 + 1_030_010);
    assert_eq!(reopen_saving(&clock, &scratch, graced), T0 + 1_030_010);
    let later = settings().with_restart_grace(secs(1), secs(0));
    assert_eq!(reopen_saving(&clock, &scratch, later), T0 + 1_030_010);
}

/// Asserts that a stored deadline later than the boot plus the grace is unchanged and
/// one exactly at it stays there.
#[test]
fn a_deadline_beyond_the_grace_is_unchanged() {
    let (mut table, clock, scratch) = fresh("res_late");
    table.set_reservations(ledger());
    table.tick();
    drop(table);
    clock.advance(secs(10));
    let graced = settings().with_restart_grace(secs(30), secs(0));
    assert_eq!(reopen_saving(&clock, &scratch, graced), T0 + 600_000);
    let exact = settings().with_restart_grace(secs(590), secs(0));
    assert_eq!(reopen_saving(&clock, &scratch, exact), T0 + 600_000);
}

/// Asserts that `set_reservations` marks the table unsaved: nothing is written until
/// the next tick, that tick writes and reports no error, and a later one writes
/// nothing more. The later of two records of one request wins.
#[test]
fn set_reservations_is_written_by_the_next_tick() {
    let (mut table, clock, scratch) = fresh("res_unsaved");
    let first = ReservationRecord {
        give_ups: 1,
        ..record("same")
    };
    let second = ReservationRecord {
        give_ups: 9,
        ..record("same")
    };
    table.set_reservations(vec![first, second]);
    assert!(!fs::read_to_string(scratch.file()).unwrap().contains("same"));
    assert!(table.tick().save_error.is_none());
    assert!(fs::read_to_string(scratch.file()).unwrap().contains("same"));
    fs::remove_file(scratch.file()).unwrap();
    table.tick();
    assert!(!scratch.file().exists());

    let table = reopen(table, &clock, &scratch);
    assert!(table.reservations().is_empty());
}
