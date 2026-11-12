//! The mark row: round trips with every field set and with none of the optional ones, the
//! layout byte for byte, the fields a later build adds, and every way a mark row can fail to
//! parse.

use std::collections::BTreeMap;

use js_types::HostId;

use super::mark_tag as t;
use super::{decode_mark, encode_mark};
use crate::codec::tests::{at, framed, with, without};
use crate::codec::{Carried, DecodeError, FORMAT, LATER_FORMAT, field_of, rearranged};
use crate::ids::JobId;
use crate::record::{Exclusion, MarkRecord};

fn host(text: &str) -> HostId {
    HostId::new(text).expect("a host")
}

/// A mark with every field set, and an exclusion of each kind.
fn full_mark() -> MarkRecord {
    MarkRecord {
        tail_end: Some(at(30)),
        exclusions: BTreeMap::from([
            (host("host-x"), Exclusion::Backoff(at(40))),
            (host("host-y"), Exclusion::Until(at(50))),
            (host("host-z"), Exclusion::ForLife),
        ]),
        ..MarkRecord::new(
            JobId::parse("mark-1").expect("an id"),
            host("host-a"),
            at(10),
            at(20),
        )
    }
}

fn mark_row(mark: &MarkRecord) -> Vec<u8> {
    encode_mark(mark).expect("encodes")
}

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

// Catches a field written and not read back (or read into another), an exclusion read as
// another kind or with another end, and `made` or the tail end dropped: a full mark and a
// bare one read back as written.
#[test]
fn a_mark_reads_back_as_written() {
    let full = full_mark();
    assert_eq!(decode_mark(&mark_row(&full)), Ok(full));
    let bare = MarkRecord::new(
        JobId::parse("mark-2").expect("an id"),
        host("host-b"),
        at(1),
        at(2),
    );
    assert_eq!(decode_mark(&mark_row(&bare)), Ok(bare));
}

// Catches a change of the bytes an earlier build reads: each field is pinned by tag and
// value, the optional tail end is absent when unset, and the exclusions are written empty
// when there are none.
#[test]
fn the_mark_layout_is_pinned() {
    let row = mark_row(&full_mark());
    assert_eq!(row[0], FORMAT);
    assert_eq!(field_of(&row, t::JOB), Some(b"mark-1".to_vec()));
    assert_eq!(field_of(&row, t::HOST), Some(b"host-a".to_vec()));
    assert_eq!(field_of(&row, t::MADE), Some(seconds(10)));
    assert_eq!(field_of(&row, t::DEADLINE), Some(seconds(20)));
    assert_eq!(field_of(&row, t::TAIL_END), Some(seconds(30)));
    assert_eq!(
        field_of(&row, t::EXCLUSIONS),
        Some(framed(&[
            b"host-x",
            &kind(0, &seconds(40)),
            b"host-y",
            &kind(1, &seconds(50)),
            b"host-z",
            &[2],
        ]))
    );
    let bare = mark_row(&MarkRecord::new(
        JobId::parse("mark-2").expect("an id"),
        host("host-b"),
        at(1),
        at(2),
    ));
    assert_eq!(field_of(&bare, t::TAIL_END), None);
    assert_eq!(field_of(&bare, t::EXCLUSIONS), Some(Vec::new()));
}

// Catches a later build's field dropped on a rewrite, and a row an earlier build wrote
// without the optional fields, in another field order, read as anything but the defaults.
#[test]
fn a_mark_row_is_additive() {
    let row = with(&mark_row(&full_mark()), 77, &[1, 2, 3]);
    let read = decode_mark(&row).expect("a later field is carried");
    assert_eq!(read.carried, Carried(BTreeMap::from([(77, vec![1, 2, 3])])));
    assert_eq!(mark_row(&read), row, "the rewrite is not the row it read");

    let older =
        rearranged(&mark_row(&full_mark()), &[t::TAIL_END, t::EXCLUSIONS]).expect("the row parses");
    let defaults = MarkRecord {
        tail_end: None,
        exclusions: BTreeMap::new(),
        ..full_mark()
    };
    assert_eq!(decode_mark(&older), Ok(defaults));
    assert_eq!(decode_mark(&[LATER_FORMAT]), Err(DecodeError::Newer));
}

// Catches a mark row that does not parse taken for a mark (or a panic): each required field
// missing, an exclusion of a kind a later build adds, and each malformed exclusion value is
// the error that names it.
#[test]
fn every_malformed_mark_row_is_an_error() {
    let row = mark_row(&full_mark());
    for tag in [t::JOB, t::HOST, t::MADE, t::DEADLINE] {
        assert_eq!(
            decode_mark(&without(&row, tag)),
            Err(DecodeError::Missing { tag }),
            "without field {tag}"
        );
    }
    let x = t::EXCLUSIONS;
    let cases: [(&[&[u8]], DecodeError); 8] = [
        (&[b"host-x", &[3]], DecodeError::Newer),
        (&[b"host-x", &[]], DecodeError::BadLength { tag: x }),
        (&[b"host-x", &[2, 0]], DecodeError::BadLength { tag: x }),
        (&[b"host-x", &[0, 1, 2]], DecodeError::BadLength { tag: x }),
        (
            &[b"host-x", &[1, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255]],
            DecodeError::BadValue { tag: x },
        ),
        (
            &[b"host-x", &[2], b"host-x", &[2]],
            DecodeError::BadValue { tag: x },
        ),
        (&[b"host x", &[2]], DecodeError::BadValue { tag: x }),
        (&[&[0xff], &[2]], DecodeError::NotUtf8 { tag: x }),
    ];
    for (items, error) in cases {
        assert_eq!(
            decode_mark(&with(&row, x, &framed(items))),
            Err(error),
            "exclusions {items:?}"
        );
    }
    assert_eq!(
        decode_mark(&with(&row, x, &framed(&[b"host-x"]))),
        Err(DecodeError::BadValue { tag: x }),
        "a host without its exclusion"
    );
    assert_eq!(
        decode_mark(&with(&row, t::HOST, b"host a")),
        Err(DecodeError::BadValue { tag: t::HOST })
    );
    assert_eq!(
        decode_mark(&with(&row, t::TAIL_END, &[0])),
        Err(DecodeError::BadLength { tag: t::TAIL_END })
    );
}
