//! The reservation ledger in the lease file: round trip, atomicity with the leases,
//! the restart grace, corrupt rows, old and newer files. The table stores records and
//! decides nothing about them, so no policy is tested here.

use std::fs;
use std::time::Duration;

use js_lease::{
    Boot, Exclusion, ExclusionEnd, Held, LeaseTable, OpenError, Queued, ReservationRecord,
};

use crate::support::{
    Scratch, T0, Table, at, clock, fake, fresh, host, open, open_err, request, secs, settings,
};

fn held(name: &str, reserved: u64, deadline: u64) -> Held {
    Held {
        host: host(name),
        reserved_at: at(reserved),
        deadline: at(deadline),
        assigned: false,
        tail_end: None,
    }
}

fn record(id: &str) -> ReservationRecord {
    ReservationRecord {
        request_id: id.to_string(),
        held: None,
        queued: None,
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
/// bad host, an exclusion with neither or both ends, a request twice, a queue place
/// without the instant it was first presented (and the reverse), and `assigned` or
/// `tail_end_ms` on a row that holds no host.
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
        head(1) + row + "queue_place = 0\n",
        head(1) + row + "queued_since_ms = 5\n",
        head(1) + row + "assigned = true\n",
        head(1) + row + "tail_end_ms = 5\n",
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

/// A request that waits in third place and holds a quiet host: the two fields a record
/// needs to be put back in its wait-list place and as an assigned mark.
fn waiting_ledger() -> Vec<ReservationRecord> {
    let third = ReservationRecord {
        held: Some(Held {
            assigned: true,
            ..held("h1", T0 + 5, T0 + 600_000)
        }),
        queued: Some(Queued {
            place: 2,
            since: at(T0 - 7),
        }),
        give_ups: 1,
        ..record("req-c")
    };
    let first = ReservationRecord {
        queued: Some(Queued {
            place: 0,
            since: at(T0 - 90),
        }),
        ..record("req-a")
    };
    vec![third, first]
}

/// Asserts that the wait-list place, the instant the request was first presented and the
/// assigned flag are in the file and come back field for field: a record with a place
/// and an assigned host, one with a place alone, and the file carries the three keys. It
/// catches a ledger that cannot put a request back in its place after a restart, and an
/// assigned mark that returns as a draining one.
#[test]
fn the_wait_place_and_the_assigned_flag_survive_a_reopen() {
    let (mut table, clock, scratch) = fresh("res_queue");
    table.set_reservations(waiting_ledger());
    assert!(table.tick().save_error.is_none());
    let text = fs::read_to_string(scratch.file()).unwrap();
    for key in [
        "queue_place = 2",
        "queued_since_ms = 999993",
        "assigned = true",
    ] {
        assert!(text.contains(key), "{key}\n{text}");
    }

    let table = reopen(table, &clock, &scratch);
    let mut expected = waiting_ledger();
    expected.sort_by(|a, b| a.request_id.cmp(&b.request_id));
    assert_eq!(unclaimed(table.reservations()), expected);
}

/// Asserts that a row without the new keys, as every file written before them has, loads
/// as a record that is not queued and holds its host unassigned, and that rewriting it
/// does not add the keys. It is the fixture of a file an older build wrote.
#[test]
fn a_row_from_before_the_queue_fields_loads_unqueued_and_unassigned() {
    let scratch = Scratch::new("res_old_row");
    let text = "version = 1\nterm = 1\nnext_sequence = 1\nlease_count = 0\nreservation_count = 1\n\n\
        [[reservation]]\nrequest_id = \"old\"\nhost = \"h1\"\nreserved_at_ms = 1\ndeadline_ms = 2\ngive_ups = 3\n";
    fs::write(scratch.file(), text).unwrap();
    let clock = clock();
    let mut table = open(fake(), &clock, &scratch);
    let restored = table.reservations();
    assert_eq!(restored[0].queued, None);
    assert_eq!(restored[0].held.as_ref().map(|h| h.assigned), Some(false));
    assert_eq!(restored[0].held.as_ref().map(|h| h.tail_end), Some(None));
    assert_eq!(restored[0].give_ups, 3);
    table.set_reservations(restored);
    assert!(table.tick().save_error.is_none());
    let rewritten = fs::read_to_string(scratch.file()).unwrap();
    for key in ["queue_place", "queued_since_ms", "assigned", "tail_end_ms"] {
        assert!(!rewritten.contains(key), "{key}\n{rewritten}");
    }
}

/// Asserts that the end of the tail a mark was made with is in the file as `tail_end_ms`
/// and comes back field for field, for a mark that has one, and that a mark without one
/// writes no key. It catches a tail lost on a restart, and an unknown tail written as zero.
#[test]
fn the_tail_end_of_a_mark_survives_a_reopen() {
    let (mut table, clock, scratch) = fresh("res_tail");
    let with = ReservationRecord {
        held: Some(Held {
            tail_end: Some(T0 + 300_000),
            ..held("h1", T0, T0 + 600_000)
        }),
        ..record("req-a")
    };
    let without = ReservationRecord {
        held: Some(held("h2", T0, T0 + 600_000)),
        ..record("req-b")
    };
    table.set_reservations(vec![with.clone(), without.clone()]);
    assert!(table.tick().save_error.is_none());
    let text = fs::read_to_string(scratch.file()).unwrap();
    assert_eq!(text.matches("tail_end_ms").count(), 1, "{text}");
    assert!(
        text.contains(&format!("tail_end_ms = {}", T0 + 300_000)),
        "{text}"
    );
    let table = reopen(table, &clock, &scratch);
    let back = unclaimed(table.reservations());
    assert_eq!(
        back.iter()
            .map(|r| r.held.as_ref().map(|h| h.tail_end))
            .collect::<Vec<_>>(),
        [Some(Some(T0 + 300_000)), Some(None)]
    );
}
