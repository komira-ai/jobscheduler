//! The queue fields of a job row (tags 48 to 53): each round-trips beside a later field, a row
//! from before them reads with their defaults and a default writes no field, the layout is
//! pinned byte for byte, a code or kind this build does not know reads as unspecified, and
//! every way a value can fail to parse is refused by tag.

use js_types::{HostId, JobEventKind, QueueCode};

use super::job_queue_tag;
use crate::codec::tests::{at, bare_job, framed, job_row, with, without};
use crate::codec::{DecodeError, decode_job, field_of};
use crate::queue::{JobEvent, QueueTrail};
use crate::record::JobRecord;

/// A time of whole seconds as a row holds it.
fn seconds(secs: u64) -> Vec<u8> {
    let mut out = secs.to_be_bytes().to_vec();
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

fn host(text: &str) -> HostId {
    HostId::new(text).expect("a host")
}

/// A job that failed over twice, was placed, and dropped one event.
fn noted() -> JobRecord {
    let events = vec![
        JobEvent {
            at: at(3),
            kind: JobEventKind::FailingOver,
            detail: "refused".to_owned(),
            host: None,
        },
        JobEvent {
            at: at(4),
            kind: JobEventKind::Placed,
            detail: "placed".to_owned(),
            host: Some(host("host-a")),
        },
    ];
    let status = (QueueCode::Placed, "placed".to_owned(), Some(at(4)));
    JobRecord {
        queue: QueueTrail::of_row(status, 3, events, 1),
        ..bare_job()
    }
}

/// The tags in order, as the design note numbers them.
const TAGS: [u16; 6] = [
    job_queue_tag::CODE,
    job_queue_tag::DETAIL,
    job_queue_tag::SINCE,
    job_queue_tag::ATTEMPTS,
    job_queue_tag::EVENTS,
    job_queue_tag::DROPPED_EVENTS,
];

// Catches a field written under another tag, in another shape, or dropped by a rewrite: the
// bytes are pinned with literal tags, the record reads back as written, and a later build's
// field beside them is kept when the row is written again.
#[test]
fn a_job_row_round_trips_its_queue_trail_with_a_later_field_beside_it() {
    assert_eq!(TAGS, [48, 49, 50, 51, 52, 53]);
    let job = noted();
    let row = job_row(&job);
    assert_eq!(field_of(&row, 48), Some(vec![5]), "the code is a byte");
    assert_eq!(field_of(&row, 49), Some(b"placed".to_vec()));
    assert_eq!(field_of(&row, 50), Some(seconds(4)));
    assert_eq!(field_of(&row, 51), Some(3u32.to_be_bytes().to_vec()));
    let first = framed(&[&seconds(3), &[4], b"refused"]);
    let second = framed(&[&seconds(4), &[5], b"placed", b"host-a"]);
    assert_eq!(field_of(&row, 52), Some(framed(&[&first, &second])));
    assert_eq!(field_of(&row, 53), Some(1u32.to_be_bytes().to_vec()));
    assert_eq!(decode_job(&row), Ok(job.clone()));
    let later = with(&row, 902, b"later");
    let read = decode_job(&later).expect("a row with a later field reads");
    assert_eq!(read.queue, job.queue);
    assert_eq!(field_of(&job_row(&read), 902), Some(b"later".to_vec()));
}

/// A job nothing was recorded of writes none of the fields, so its row has the bytes a build
/// before them wrote.
#[test]
fn a_job_at_the_default_writes_no_queue_field() {
    let row = job_row(&bare_job());
    for tag in TAGS {
        assert_eq!(field_of(&row, tag), None, "tag {tag}");
    }
}

/// A row from before the fields (or with any one of them left out) reads with that field's
/// default: the backward compatibility of old records.
#[test]
fn a_job_row_from_before_the_queue_fields_reads_with_defaults() {
    let row = job_row(&noted());
    let mut old = row.clone();
    for tag in TAGS {
        old = without(&old, tag);
    }
    let read = decode_job(&old).expect("an old row reads");
    assert_eq!(read.queue, QueueTrail::default());
    assert_eq!(read, bare_job());
    let without_events = decode_job(&without(&row, 52)).expect("reads");
    assert!(without_events.queue.events().is_empty());
    assert_eq!(without_events.queue.attempts(), 3);
}

/// Each code and kind byte this build knows reads back as written, and a byte it does not
/// know reads as unspecified rather than refusing the row.
#[test]
fn every_code_and_kind_reads_back_and_an_unknown_byte_reads_as_unspecified() {
    let codes = [
        QueueCode::WaitingForCapacity,
        QueueCode::WaitingForWholeHost,
        QueueCode::QuotaBackoff,
        QueueCode::FailingOver,
        QueueCode::Placed,
        QueueCode::WaitingForMachine,
    ];
    for code in codes {
        let job = JobRecord {
            queue: QueueTrail::of_row((code, String::new(), None), 0, Vec::new(), 0),
            ..bare_job()
        };
        assert_eq!(decode_job(&job_row(&job)).map(|j| j.queue.code()), Ok(code));
    }
    let kinds = [
        JobEventKind::Unspecified,
        JobEventKind::WaitingForCapacity,
        JobEventKind::WaitingForWholeHost,
        JobEventKind::QuotaBackoff,
        JobEventKind::FailingOver,
        JobEventKind::Placed,
        JobEventKind::Requeued,
        JobEventKind::WaitingForMachine,
    ];
    for kind in kinds {
        let event = JobEvent {
            at: at(1),
            kind,
            detail: String::new(),
            host: None,
        };
        let job = JobRecord {
            queue: QueueTrail::of_row(Default::default(), 0, vec![event.clone()], 0),
            ..bare_job()
        };
        let read = decode_job(&job_row(&job)).expect("reads");
        assert_eq!(read.queue.events(), &[event]);
    }
    let row = job_row(&noted());
    let unknown_code = with(&without(&row, 48), 48, &[200]);
    let read = decode_job(&unknown_code).expect("an unknown code reads");
    assert_eq!(read.queue.code(), QueueCode::Unspecified);
    let event = framed(&[&seconds(3), &[200], b"later"]);
    let unknown_kind = with(&without(&row, 52), 52, &framed(&[&event]));
    let read = decode_job(&unknown_kind).expect("an unknown kind reads");
    assert_eq!(read.queue.events()[0].kind, JobEventKind::Unspecified);
}

// Catches a malformed value read as some other value instead of refused.
#[test]
fn a_queue_value_that_does_not_parse_is_refused_by_tag() {
    let row = job_row(&noted());
    let replaced = |tag: u16, value: &[u8]| decode_job(&with(&without(&row, tag), tag, value));
    let event = |parts: &[&[u8]]| framed(&[&framed(parts)]);
    let cases: Vec<(u16, Vec<u8>, DecodeError)> = vec![
        (48, vec![1, 2], DecodeError::BadLength { tag: 48 }),
        (49, vec![0xff], DecodeError::NotUtf8 { tag: 49 }),
        (50, vec![0; 3], DecodeError::BadLength { tag: 50 }),
        (51, vec![0; 3], DecodeError::BadLength { tag: 51 }),
        (53, vec![0; 5], DecodeError::BadLength { tag: 53 }),
        (52, vec![0, 0, 0, 9], DecodeError::BadValue { tag: 52 }),
        (
            52,
            framed(&[&[0, 0, 0, 9]]),
            DecodeError::BadValue { tag: 52 },
        ),
        (
            52,
            event(&[&seconds(1), &[1]]),
            DecodeError::BadValue { tag: 52 },
        ),
        (
            52,
            event(&[&seconds(1), &[1], b"d", b"h", b"x"]),
            DecodeError::BadValue { tag: 52 },
        ),
        (
            52,
            event(&[&[0; 2], &[1], b"d"]),
            DecodeError::BadLength { tag: 52 },
        ),
        (
            52,
            event(&[&seconds(1), &[1, 2], b"d"]),
            DecodeError::BadLength { tag: 52 },
        ),
        (
            52,
            event(&[&seconds(1), &[1], &[0xff]]),
            DecodeError::NotUtf8 { tag: 52 },
        ),
        (
            52,
            event(&[&seconds(1), &[1], b"d", b""]),
            DecodeError::BadValue { tag: 52 },
        ),
    ];
    for (tag, value, refused) in cases {
        assert_eq!(
            replaced(tag, &value),
            Err(refused),
            "tag {tag}, value {value:?}"
        );
    }
}
