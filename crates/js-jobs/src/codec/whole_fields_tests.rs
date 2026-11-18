//! The whole-host fields of a job row (tags 45 and 46) and of an allocation row (tags 14 and
//! 15): each round-trips beside a later field, a row from before them reads with their defaults
//! and a default writes no field, the layout is pinned byte for byte, and every way a value can
//! fail to parse is refused by tag.

use std::collections::BTreeMap;

use js_types::HostId;

use super::{alloc_whole_tag, job_whole_tag};
use crate::codec::tests::{
    allocation, at, bare_job, framed, full_job, job_row, shape, with, without,
};
use crate::codec::{DecodeError, decode_allocation, decode_job, encode_allocation, field_of};
use crate::record::{AllocationRecord, Booked, Exclusion, JobRecord};

/// A time of whole seconds as a row holds it.
fn seconds(secs: u64) -> Vec<u8> {
    let mut out = secs.to_be_bytes().to_vec();
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// An exclusion's value: the kind byte and then `rest`.
fn kind(byte: u8, rest: &[u8]) -> Vec<u8> {
    let mut out = vec![byte];
    out.extend_from_slice(rest);
    out
}

fn host(text: &str) -> HostId {
    HostId::new(text).expect("a host")
}

/// A job that has failed twice, kept off a host by each kind of exclusion.
fn barred() -> JobRecord {
    JobRecord {
        failed_prepares: 2,
        prepare_bars: BTreeMap::from([
            (host("host-b"), Exclusion::Backoff(at(8))),
            (host("host-c"), Exclusion::Until(at(9))),
            (host("host-d"), Exclusion::ForLife),
        ]),
        ..bare_job()
    }
}

fn allocation_row(allocation: &AllocationRecord) -> Vec<u8> {
    encode_allocation(allocation).expect("encodes")
}

/// A whole-host allocation that waits for its agent, with a wall limit of its own.
fn waiting() -> AllocationRecord {
    AllocationRecord {
        prepare_deadline: Some(at(9)),
        wall_limit_secs: Some(1800),
        ..allocation(Booked::WholeHost)
    }
}

// Catches a count or a bar written under another tag, in another shape, or dropped by a
// rewrite: the bytes are pinned with literal tags, the record reads back as written, and a later
// build's field beside them is kept when the row is written again.
#[test]
fn a_job_row_round_trips_its_failed_prepares_with_a_later_field_beside_them() {
    let job = barred();
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 45),
        Some(2u32.to_be_bytes().to_vec()),
        "the count is a u32"
    );
    let bars = framed(&[
        b"host-b",
        &kind(0, &seconds(8)),
        b"host-c",
        &kind(1, &seconds(9)),
        b"host-d",
        &[2],
    ]);
    assert_eq!(
        field_of(&row, 46),
        Some(bars),
        "the bars are a map of host to exclusion"
    );
    assert_eq!(decode_job(&row), Ok(job.clone()));
    let read = decode_job(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.failed_prepares, job.failed_prepares);
    assert_eq!(read.prepare_bars, job.prepare_bars);
    let rewritten = job_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 45), field_of(&row, 45));
    assert_eq!(field_of(&rewritten, 46), field_of(&row, 46));
}

// Catches a default that is written: a job with no failed prepare and no bar writes neither
// field, so every row written before them keeps its bytes. A count without bars and bars
// without a count each write only their own field.
#[test]
fn a_job_at_the_default_writes_neither_field() {
    let row = job_row(&bare_job());
    assert_eq!(field_of(&row, 45), None);
    assert_eq!(field_of(&row, 46), None);

    let counted = JobRecord {
        failed_prepares: 1,
        ..bare_job()
    };
    let row = job_row(&counted);
    assert_eq!(field_of(&row, 45), Some(1u32.to_be_bytes().to_vec()));
    assert_eq!(field_of(&row, 46), None);
    assert_eq!(decode_job(&row), Ok(counted));

    let barred_only = JobRecord {
        prepare_bars: barred().prepare_bars,
        ..bare_job()
    };
    let row = job_row(&barred_only);
    assert_eq!(field_of(&row, 45), None);
    assert!(field_of(&row, 46).is_some());
    assert_eq!(decode_job(&row), Ok(barred_only));
}

// Catches a row from before failed prepares that is refused, or read as having some, when it
// should read as having none.
#[test]
fn a_job_row_from_before_failed_prepares_reads_as_having_none() {
    let row = job_row(&JobRecord {
        failed_prepares: 3,
        prepare_bars: barred().prepare_bars,
        ..full_job()
    });
    let older = without(&row, job_whole_tag::FAILED_PREPARES);
    let older = without(&older, job_whole_tag::PREPARE_BARS);
    assert_eq!(
        decode_job(&older),
        Ok(JobRecord {
            failed_prepares: 0,
            prepare_bars: BTreeMap::new(),
            ..full_job()
        })
    );
}

// Catches a count or a bar read from a value that is not one: a count of another width, a bar
// map that does not frame, that names a host twice, that holds a host or a kind or a time no
// writer produces, or that holds a kind this build does not know (a later build's).
#[test]
fn a_failed_prepare_field_that_is_not_valid_is_refused_by_tag() {
    let row = job_row(&full_job());
    for (value, why) in [
        (vec![0; 3], DecodeError::BadLength { tag: 45 }),
        (vec![0; 5], DecodeError::BadLength { tag: 45 }),
    ] {
        assert_eq!(decode_job(&with(&row, 45, &value)), Err(why), "{value:?}");
    }
    let bad = DecodeError::BadValue { tag: 46 };
    let length = DecodeError::BadLength { tag: 46 };
    let past_a_second = kind(1, &[0, 0, 0, 0, 0, 0, 0, 0, 59, 154, 202, 0]);
    let cases: [(&[&[u8]], DecodeError); 9] = [
        (&[b"host-b"], bad),
        (&[&[0xff], &[2]], DecodeError::NotUtf8 { tag: 46 }),
        (&[b"Host B", &[2]], bad),
        (&[b"host-b", &[]], length),
        (&[b"host-b", &[2, 0]], length),
        (&[b"host-b", &[1, 0, 0]], length),
        (&[b"host-b", &past_a_second], bad),
        (&[b"host-b", &[9]], DecodeError::Newer),
        (&[b"host-b", &[2], b"host-b", &[2]], bad),
    ];
    for (items, why) in cases {
        assert_eq!(
            decode_job(&with(&row, 46, &framed(items))),
            Err(why),
            "bars {items:?}"
        );
    }
    assert_eq!(decode_job(&with(&row, 46, &[0, 0])), Err(bad), "a bad list");
}

// Catches a deadline or a wall limit written under another tag, dropped by a rewrite, or
// written for an allocation that has none, which would change every row written before them.
#[test]
fn an_allocation_row_round_trips_its_prepare_deadline_and_wall_limit() {
    let record = waiting();
    let row = allocation_row(&record);
    assert_eq!(
        field_of(&row, 14),
        Some(seconds(9)),
        "the deadline is a time"
    );
    assert_eq!(
        field_of(&row, 15),
        Some(1800u64.to_be_bytes().to_vec()),
        "the wall limit is a u64"
    );
    assert_eq!(decode_allocation(&row), Ok(record.clone()));
    let read = decode_allocation(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.prepare_deadline, Some(at(9)));
    assert_eq!(read.wall_limit_secs, Some(1800));
    let rewritten = allocation_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 14), field_of(&row, 14));
    assert_eq!(field_of(&rewritten, 15), field_of(&row, 15));

    let plain = allocation_row(&allocation(shape()));
    assert_eq!(field_of(&plain, 14), None);
    assert_eq!(field_of(&plain, 15), None);

    let deadline_only = AllocationRecord {
        wall_limit_secs: None,
        ..record.clone()
    };
    let row = allocation_row(&deadline_only);
    assert!(field_of(&row, 14).is_some());
    assert_eq!(field_of(&row, 15), None);
    assert_eq!(decode_allocation(&row), Ok(deadline_only));

    let limit_only = AllocationRecord {
        prepare_deadline: None,
        ..record
    };
    let row = allocation_row(&limit_only);
    assert_eq!(field_of(&row, 14), None);
    assert!(field_of(&row, 15).is_some());
    assert_eq!(decode_allocation(&row), Ok(limit_only));
}

// Catches a row from before the prepare deadline and the wall limit that is refused, or read as
// having them, when it should read as having neither.
#[test]
fn an_allocation_row_from_before_the_prepare_deadline_reads_as_having_none() {
    let row = allocation_row(&waiting());
    let older = without(&row, alloc_whole_tag::PREPARE_DEADLINE);
    let older = without(&older, alloc_whole_tag::WALL_LIMIT_SECS);
    assert_eq!(
        decode_allocation(&older),
        Ok(AllocationRecord {
            prepare_deadline: None,
            wall_limit_secs: None,
            ..waiting()
        })
    );
}

// Catches a deadline or a limit read from a value of another width, or from nanoseconds that
// make a whole second.
#[test]
fn a_prepare_deadline_or_wall_limit_that_is_not_valid_is_refused_by_tag() {
    let row = allocation_row(&waiting());
    let mut past_a_second = 0u64.to_be_bytes().to_vec();
    past_a_second.extend_from_slice(&1_000_000_000u32.to_be_bytes());
    let cases: [(u16, &[u8], DecodeError); 4] = [
        (14, &[0; 11], DecodeError::BadLength { tag: 14 }),
        (14, &[0; 13], DecodeError::BadLength { tag: 14 }),
        (14, &past_a_second, DecodeError::BadValue { tag: 14 }),
        (15, &[0; 7], DecodeError::BadLength { tag: 15 }),
    ];
    for (tag, value, why) in cases {
        assert_eq!(
            decode_allocation(&with(&row, tag, value)),
            Err(why),
            "field {tag} of {value:?}"
        );
    }
}
