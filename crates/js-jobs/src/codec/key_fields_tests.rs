//! The history key of a job row (tag 66): it round-trips byte for byte, a job that names none
//! writes no tag (the bytes a build before the field wrote), a row without it reads as none,
//! and a value that is not a history key is refused by its tag.

use js_types::HistoryKey;

use crate::codec::tests::{full_job, with, without};
use crate::codec::{DecodeError, decode_job, encode_job, field_of};
use crate::record::JobRecord;

const HISTORY_KEY: u16 = 66;

/// A full build target's name and the kind of its step, 200 bytes long.
fn target() -> String {
    let path = "src/".repeat(40);
    let name = format!("komira//{path}json:json mojo_compile build");
    let name = format!("{name}{}", " x".repeat((200 - name.len()) / 2));
    assert_eq!(name.len(), 200);
    name
}

fn job_with(class: Option<&str>) -> JobRecord {
    let mut job = full_job();
    job.spec.history_key = class.map(|class| HistoryKey::parse(class).expect("a class"));
    job
}

fn row_of(job: &JobRecord) -> Vec<u8> {
    encode_job(job).expect("encodes")
}

/// A 200-byte class with slashes, a colon and spaces is the tag's value byte for byte and
/// reads back as itself, and so does the longest one. It catches a class lowered, cut or
/// hashed on its way through the row, or read into another field.
#[test]
fn a_history_key_round_trips_byte_for_byte() {
    let longest = "C".repeat(512);
    for class in [target(), longest] {
        let job = job_with(Some(&class));
        let row = row_of(&job);
        assert_eq!(
            field_of(&row, HISTORY_KEY),
            Some(class.clone().into_bytes())
        );
        assert_eq!(decode_job(&row), Ok(job));
    }
}

/// A job that names no class writes no tag 66, so its row is the row a build before the field
/// wrote; and a row without the tag (an older build's) reads with none. It catches a none
/// written as an empty value, and a row from before the field refused or given a class.
#[test]
fn no_class_writes_no_tag_and_a_row_without_it_reads_as_none() {
    let row = row_of(&job_with(None));
    assert_eq!(field_of(&row, HISTORY_KEY), None);
    assert_eq!(decode_job(&row), Ok(job_with(None)));
    let named = row_of(&job_with(Some(&target())));
    assert_eq!(without(&named, HISTORY_KEY), row);
    assert_eq!(
        decode_job(&without(&named, HISTORY_KEY)),
        Ok(job_with(None))
    );
}

/// An empty value, one of 513 bytes, one holding a line break or a byte above ASCII is
/// refused as a bad value of tag 66, and a value that is not UTF-8 as such. It catches a row
/// that smuggles a class the submit would refuse into the history key and the logs.
#[test]
fn a_value_that_is_not_a_history_key_is_refused_by_its_tag() {
    let row = row_of(&job_with(Some("compile")));
    let over = "x".repeat(513);
    for bad in [
        &b""[..],
        over.as_bytes(),
        b"two\nlines",
        "caf\u{e9}".as_bytes(),
    ] {
        assert_eq!(
            decode_job(&with(&row, HISTORY_KEY, bad)),
            Err(DecodeError::BadValue { tag: HISTORY_KEY }),
            "{bad:?}"
        );
    }
    assert_eq!(
        decode_job(&with(&row, HISTORY_KEY, &[0xff, 0xfe])),
        Err(DecodeError::NotUtf8 { tag: HISTORY_KEY })
    );
}
