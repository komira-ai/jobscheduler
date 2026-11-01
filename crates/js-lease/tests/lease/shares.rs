//! What a lease that shares its host leaves in the file: the resources it books, so that a
//! restart can put it back as the lease it was.

use std::fs;

use js_lease::{Booking, LeaseState, read_bookings};
use js_types::{Admission, ClassId, Request, ResourceClass, Resources};

use crate::support::{Scratch, clock, fake, host, open, request};

fn sharing() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("share").unwrap(),
        Admission::Book(Default::default()),
        0,
    )
}

fn asking(resources: Resources) -> Request {
    Request {
        class: sharing().id().clone(),
        resources,
        ..request(3600)
    }
}

/// Half a host's cores and memory and its GPU: every axis of the booking is set.
fn half() -> Resources {
    Resources::new(4000, 4, 1, Some("g".into())).unwrap()
}

/// Asserts that the bookings of a file name what a lease that shares its host books, and
/// none for a lease that holds the whole host, and that a reopened table writes the field
/// back unchanged. A table that does not keep the resources reads `None` for both.
#[test]
fn a_lease_that_shares_a_host_records_what_it_books() {
    let scratch = Scratch::new("shares_recorded");
    let clock = clock();
    let mut table = open(fake().with_class(sharing()), &clock, &scratch);
    let shared = table.acquire(asking(half()), None).unwrap();
    let whole = table.acquire(request(3600), None).unwrap();

    let expected = vec![
        Booking {
            lease: shared.id,
            host: shared.host.clone(),
            state: LeaseState::Held,
            shares: Some(half()),
        },
        Booking {
            lease: whole.id,
            host: whole.host.clone(),
            state: LeaseState::Held,
            shares: None,
        },
    ];
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
    let before = fs::read_to_string(scratch.file()).unwrap();
    let row = "[lease.shares]\ncpu_millis = 4000\nmemory_bytes = 4\ngpus = 1\ngpu_class = \"g\"\n";
    assert_eq!(before.matches("[lease.shares]").count(), 1, "{before}");
    assert!(before.contains(row), "{before}");

    let provider = table.into_provider();
    let table = open(provider, &clock, &scratch);
    assert_eq!(table.list().len(), 2);
    let after = fs::read_to_string(scratch.file()).unwrap();
    assert_eq!(after.matches("[lease.shares]").count(), 1, "{after}");
    assert!(after.contains(row), "{after}");
    assert_eq!(read_bookings(scratch.file()).unwrap(), expected);
}

/// Asserts that a file written before the field reads as a lease that books the whole
/// host, one with a malformed field (GPUs without a class) reads the same instead of
/// refusing the file, and a field this build does not know inside it is ignored. The
/// booking is of a lease the file names, so the server can still put it back.
#[test]
fn the_field_is_read_leniently_and_kept_as_read() {
    let scratch = Scratch::new("shares_lenient");
    let head = "version = 1\nterm = 3\nnext_sequence = 4\nlease_count = 3\n";
    let lease = |n: u64, tail: &str| {
        format!(
            "\n[[lease]]\nterm = 1\nsequence = {n}\nstate = \"held\"\nhost = \"h{n}\"\n\
             acquired_at_ms = 1\nexpires_at_ms = 2\nends_at_ms = 3\n{tail}"
        )
    };
    let old = lease(1, "");
    let odd = lease(
        2,
        "\n[lease.shares]\ncpu_millis = 1000\nmemory_bytes = 1\ngpus = 1\n",
    );
    let newer = lease(
        3,
        "\n[lease.shares]\ncpu_millis = 1000\nmemory_bytes = 1\nswap_bytes = 9\n",
    );
    fs::write(scratch.file(), format!("{head}{old}{odd}{newer}")).unwrap();
    let got = read_bookings(scratch.file()).unwrap();
    let shares: Vec<_> = got.iter().map(|booking| booking.shares.clone()).collect();
    let one = Resources::new(1000, 1, 0, None).unwrap();
    assert_eq!(shares, vec![None, None, Some(one)]);
    assert_eq!(got[2].host, host("h3"));
}

/// Asserts that a row that cannot be read as resources does not fail the file: a row
/// missing `cpu_millis`, a `shares` that is not a table, and a row with a text count each
/// read as a lease that books the whole host, and the leases around them are read as
/// before. The server must start on such a file.
#[test]
fn a_malformed_row_reads_as_no_shares_and_does_not_fail_the_file() {
    let scratch = Scratch::new("shares_malformed");
    let head = "version = 1\nterm = 3\nnext_sequence = 5\nlease_count = 4\n";
    let lease = |n: u64, tail: &str| {
        format!(
            "\n[[lease]]\nterm = 1\nsequence = {n}\nstate = \"held\"\nhost = \"h{n}\"\n\
             acquired_at_ms = 1\nexpires_at_ms = 2\nends_at_ms = 3\n{tail}"
        )
    };
    let missing = lease(1, "\n[lease.shares]\nmemory_bytes = 1\n");
    let scalar = lease(2, "shares = 3\n");
    let text = lease(
        3,
        "\n[lease.shares]\ncpu_millis = \"many\"\nmemory_bytes = 1\n",
    );
    let fine = lease(4, "\n[lease.shares]\ncpu_millis = 500\nmemory_bytes = 1\n");
    fs::write(
        scratch.file(),
        format!("{head}{missing}{scalar}{text}{fine}"),
    )
    .unwrap();
    let got = read_bookings(scratch.file()).unwrap();
    let shares: Vec<_> = got.iter().map(|booking| booking.shares.clone()).collect();
    let half = Resources::new(500, 1, 0, None).unwrap();
    assert_eq!(shares, vec![None, None, None, Some(half)]);
}
