//! The held and capacity refusals of a job row (tags 59 and 63): the hosts round-trip in order
//! beside a later field, a row from before them reads as having none and a job with none writes
//! no field, the layout is pinned byte for byte, and every way a value can fail to parse is
//! refused by tag.

use js_types::HostId;

use super::job_whole_tag;
use crate::codec::tests::{bare_job, framed, full_job, job_row, with, without};
use crate::codec::{DecodeError, decode_job, field_of};
use crate::record::JobRecord;

fn host(text: &str) -> HostId {
    HostId::new(text).expect("a host")
}

/// A job a held host refused twice and another host once, between them.
fn refused() -> JobRecord {
    JobRecord {
        held_refusals: vec![host("host-b"), host("host-c"), host("host-b")],
        ..bare_job()
    }
}

// Catches the held refusals written under another tag, in another shape, sorted or with a
// repeat dropped (the count is the list's length), or dropped by a rewrite: the bytes are
// pinned with the literal tag, the record reads back as written, and a later build's field
// beside it is kept when the row is written again.
#[test]
fn a_job_row_round_trips_its_held_refusals_in_order_with_a_later_field_beside_them() {
    let job = refused();
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 59),
        Some(framed(&[b"host-b", b"host-c", b"host-b"])),
        "the refusals are a list of host text, in order"
    );
    assert_eq!(decode_job(&row), Ok(job.clone()));
    let read = decode_job(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.held_refusals, job.held_refusals);
    let rewritten = job_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 59), field_of(&row, 59));
}

// Catches a default that is written, which would change the bytes of every row written before
// the field, and a row from before it that is refused or read as having refusals.
#[test]
fn a_job_with_no_held_refusal_writes_no_field_and_an_older_row_reads_as_having_none() {
    assert_eq!(field_of(&job_row(&bare_job()), 59), None);
    let older = without(&job_row(&full_job()), job_whole_tag::HELD_REFUSALS);
    assert_eq!(
        decode_job(&older),
        Ok(JobRecord {
            held_refusals: Vec::new(),
            ..full_job()
        })
    );
}

// Catches a list read from a value that is not one: a list that does not frame, an item that
// is not text and an item that is not a host id are each refused with the tag.
#[test]
fn a_held_refusal_field_that_is_not_valid_is_refused_by_tag() {
    let row = job_row(&full_job());
    let cases: [(Vec<u8>, DecodeError); 3] = [
        (vec![0, 0], DecodeError::BadValue { tag: 59 }),
        (
            framed(&[b"host-b", &[0xff]]),
            DecodeError::NotUtf8 { tag: 59 },
        ),
        (framed(&[b"Host B"]), DecodeError::BadValue { tag: 59 }),
    ];
    for (value, why) in cases {
        assert_eq!(decode_job(&with(&row, 59, &value)), Err(why), "{value:?}");
    }
}

/// A job a host refused for capacity on two hosts, one of them twice.
fn crowded() -> JobRecord {
    JobRecord {
        capacity_refusals: vec![host("host-b"), host("host-c"), host("host-b")],
        ..bare_job()
    }
}

// Catches the capacity refusals written under another tag, sorted or with a repeat dropped, or
// mixed with the held refusals: the bytes are pinned with the literal tag, the record reads
// back as written, and a later build's field beside it is kept when the row is written again.
#[test]
fn a_job_row_round_trips_its_capacity_refusals_in_order_apart_from_its_held_refusals() {
    let job = JobRecord {
        held_refusals: vec![host("host-d")],
        ..crowded()
    };
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 63),
        Some(framed(&[b"host-b", b"host-c", b"host-b"])),
        "the refusals are a list of host text, in order"
    );
    assert_eq!(field_of(&row, 59), Some(framed(&[b"host-d"])));
    assert_eq!(decode_job(&row), Ok(job.clone()));
    let read = decode_job(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.capacity_refusals, job.capacity_refusals);
    let rewritten = job_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 63), field_of(&row, 63));
}

// Catches a default that is written, which would change the bytes of every row written before
// the field, and a row from before it that is refused or read as having refusals.
#[test]
fn a_job_with_no_capacity_refusal_writes_no_field_and_an_older_row_reads_as_having_none() {
    assert_eq!(field_of(&job_row(&bare_job()), 63), None);
    let older = without(&job_row(&full_job()), job_whole_tag::CAPACITY_REFUSALS);
    assert_eq!(
        decode_job(&older),
        Ok(JobRecord {
            capacity_refusals: Vec::new(),
            ..full_job()
        })
    );
}

// Catches a list read from a value that is not one: a list that does not frame, an item that
// is not text and an item that is not a host id are each refused with the tag.
#[test]
fn a_capacity_refusal_field_that_is_not_valid_is_refused_by_tag() {
    let row = job_row(&full_job());
    let cases: [(Vec<u8>, DecodeError); 3] = [
        (vec![0, 0], DecodeError::BadValue { tag: 63 }),
        (
            framed(&[b"host-b", &[0xff]]),
            DecodeError::NotUtf8 { tag: 63 },
        ),
        (framed(&[b"Host B"]), DecodeError::BadValue { tag: 63 }),
    ];
    for (value, why) in cases {
        assert_eq!(decode_job(&with(&row, 63, &value)), Err(why), "{value:?}");
    }
}
