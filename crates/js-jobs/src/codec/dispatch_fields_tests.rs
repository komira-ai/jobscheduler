//! The dispatch message row: round trips with every field set and with none of the optional
//! ones, the layout byte for byte, the fields a later build adds, and every way a dispatch row
//! can fail to parse.

use std::collections::BTreeMap;

use super::dispatch_tag as t;
use super::{decode_dispatch, encode_dispatch};
use crate::codec::tests::{at, with, without};
use crate::codec::{Carried, DecodeError, FORMAT, LATER_FORMAT, field_of, rearranged};
use crate::ids::{AllocationId, JobId};
use crate::record::{DispatchKind, DispatchMessage};
use crate::suite::{host, start_body};

/// A start that was handed out, with every field set.
fn full_message() -> DispatchMessage {
    DispatchMessage {
        kind: DispatchKind::Start,
        job: JobId::parse("send-1").expect("an id"),
        allocation_id: AllocationId::from_bits([7; 16]),
        host: host("host-a"),
        queued_at: at(10),
        start: Some(start_body("send-1", "07070707070707070707070707070707")),
        claimed_by: Some("server-1".to_owned()),
        claim_until: Some(at(20)),
        carried: Carried::default(),
    }
}

/// A stop never handed out: nothing optional set.
fn bare_message() -> DispatchMessage {
    DispatchMessage {
        kind: DispatchKind::Stop,
        start: None,
        claimed_by: None,
        claim_until: None,
        ..full_message()
    }
}

fn row(message: &DispatchMessage) -> Vec<u8> {
    encode_dispatch(message).expect("encodes")
}

/// A time of whole seconds as a row holds it.
fn seconds(secs: u64) -> Vec<u8> {
    let mut out = secs.to_be_bytes().to_vec();
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

// Catches a field written and not read back (or read into another), and a kind read as the
// other: a handed-out start with its body and a bare stop read back as written.
#[test]
fn a_dispatch_message_reads_back_as_written() {
    let full = full_message();
    assert_eq!(decode_dispatch(&row(&full)), Ok(full));
    let bare = bare_message();
    assert_eq!(decode_dispatch(&row(&bare)), Ok(bare));
}

// Catches a change of the bytes an earlier build reads: each field is pinned by tag and value,
// the start body is the agent protocol's JSON, and the optional fields are absent when unset.
#[test]
fn the_dispatch_layout_is_pinned() {
    let full = full_message();
    let bytes = row(&full);
    assert_eq!(bytes[0], FORMAT);
    assert_eq!(field_of(&bytes, t::KIND), Some(vec![1]));
    assert_eq!(field_of(&bytes, t::JOB), Some(b"send-1".to_vec()));
    assert_eq!(field_of(&bytes, t::ALLOCATION_ID), Some(vec![7; 16]));
    assert_eq!(field_of(&bytes, t::HOST), Some(b"host-a".to_vec()));
    assert_eq!(field_of(&bytes, t::QUEUED_AT), Some(seconds(10)));
    let body = serde_json::to_vec(&full.start).expect("a start is JSON");
    assert_eq!(field_of(&bytes, t::START), Some(body));
    assert_eq!(field_of(&bytes, t::CLAIMED_BY), Some(b"server-1".to_vec()));
    assert_eq!(field_of(&bytes, t::CLAIM_UNTIL), Some(seconds(20)));
    let bare = row(&bare_message());
    assert_eq!(field_of(&bare, t::KIND), Some(vec![2]));
    for tag in [t::START, t::CLAIMED_BY, t::CLAIM_UNTIL] {
        assert_eq!(field_of(&bare, tag), None, "field {tag} written unset");
    }
}

// Catches a later build's field dropped on a rewrite (a claim of a newer server's message must
// keep it), a row an earlier build wrote without the optional fields, in another order, read as
// anything but the defaults, and a start body with a field a later agent protocol adds refused.
#[test]
fn a_dispatch_row_is_additive() {
    let later = with(&row(&full_message()), 77, &[1, 2, 3]);
    let read = decode_dispatch(&later).expect("a later field is carried");
    assert_eq!(read.carried, Carried(BTreeMap::from([(77, vec![1, 2, 3])])));
    assert_eq!(row(&read), later, "the rewrite is not the row it read");

    let older = rearranged(
        &row(&full_message()),
        &[t::START, t::CLAIMED_BY, t::CLAIM_UNTIL],
    )
    .expect("the row parses");
    let defaults = DispatchMessage {
        start: None,
        claimed_by: None,
        claim_until: None,
        ..full_message()
    };
    assert_eq!(decode_dispatch(&older), Ok(defaults));

    let mut body = serde_json::to_value(start_body("send-1", "a")).expect("JSON");
    body["a_later_field"] = serde_json::Value::from(true);
    let newer_body = serde_json::to_vec(&body).expect("JSON");
    let read = decode_dispatch(&with(&row(&full_message()), t::START, &newer_body));
    assert_eq!(
        read.expect("a start body with a later field reads").start,
        Some(start_body("send-1", "a"))
    );
    assert_eq!(decode_dispatch(&[LATER_FORMAT]), Err(DecodeError::Newer));
}

// Catches a dispatch row that does not parse taken for a message (or a panic): each required
// field missing, a kind a later build adds, and each malformed value is the error that names it.
#[test]
fn every_malformed_dispatch_row_is_an_error() {
    let bytes = row(&full_message());
    for tag in [t::KIND, t::JOB, t::ALLOCATION_ID, t::HOST, t::QUEUED_AT] {
        assert_eq!(
            decode_dispatch(&without(&bytes, tag)),
            Err(DecodeError::Missing { tag }),
            "without field {tag}"
        );
    }
    let cases: [(u16, &[u8], DecodeError); 9] = [
        (t::KIND, &[3], DecodeError::Newer),
        (t::KIND, &[1, 1], DecodeError::BadLength { tag: t::KIND }),
        (t::JOB, b"no spaces", DecodeError::BadValue { tag: t::JOB }),
        (
            t::ALLOCATION_ID,
            &[1],
            DecodeError::BadLength {
                tag: t::ALLOCATION_ID,
            },
        ),
        (t::HOST, b"host a", DecodeError::BadValue { tag: t::HOST }),
        (
            t::QUEUED_AT,
            &[0],
            DecodeError::BadLength { tag: t::QUEUED_AT },
        ),
        (t::START, b"{", DecodeError::BadValue { tag: t::START }),
        (
            t::CLAIMED_BY,
            &[0xff],
            DecodeError::NotUtf8 { tag: t::CLAIMED_BY },
        ),
        (
            t::CLAIM_UNTIL,
            &[0],
            DecodeError::BadLength {
                tag: t::CLAIM_UNTIL,
            },
        ),
    ];
    for (tag, value, error) in cases {
        assert_eq!(
            decode_dispatch(&with(&bytes, tag, value)),
            Err(error),
            "field {tag} holding {value:?}"
        );
    }
}
