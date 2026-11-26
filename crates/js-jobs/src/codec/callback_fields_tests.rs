//! A job's callback in its job row (tag 68) and the callback row: each reads back as written, a
//! job without a callback writes no tag (the bytes a build before the field wrote), the job row
//! never holds the key, a field a later build adds is carried, and each malformed value is
//! refused by its tag.

use std::time::Duration;

use js_state::StoreTime;
use js_types::{FailureClass, SigningKey};

use super::{callback_tag as t, decode_callback, encode_callback, job_callback_tag};
use crate::callback::{CallbackKey, CallbackRecord, CallbackState, JobCallback, KeyDigest};
use crate::codec::tests::{full_job, with, without};
use crate::codec::{Carried, DecodeError, FORMAT, decode_job, encode_job, field_of, list};
use crate::ids::{InstanceId, JobId};
use crate::record::JobRecord;

const CALLBACK: u16 = job_callback_tag::CALLBACK;

/// A tag no build has used: a field a later build adds to a callback row.
const LATER_TAG: u16 = 903;

fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::new(secs, 7))
}

fn key() -> SigningKey {
    SigningKey::new((0..64).collect()).expect("a key")
}

fn digest() -> KeyDigest {
    CallbackKey::Key(key()).digest()
}

fn callback(deadline_seconds: Option<u32>) -> JobCallback {
    JobCallback {
        url: "https://hooks.example/js-done/ref-1?token=t".to_owned(),
        key: CallbackKey::Key(key()),
        deadline_seconds,
    }
}

fn job_with(callback: Option<JobCallback>) -> JobRecord {
    let mut job = full_job();
    job.spec.callback = callback;
    job
}

/// A callback row with every field set.
fn full_row() -> CallbackRecord {
    CallbackRecord {
        job: JobId::parse("job-1").expect("a job id"),
        url: "https://hooks.example/js-done/ref-1?token=t".to_owned(),
        key: Some(key()),
        webhook_id: "AQEBAQEBAQEBAQEBAQEBAQ".to_owned(),
        deadline_seconds: Some(60),
        state: CallbackState::Pending,
        tries: 3,
        next_try_at: Some(at(10)),
        deadline_at: Some(at(70)),
        last_try_at: Some(at(5)),
        last_failure: Some(FailureClass::RefusedAddress),
        delivered_at: Some(at(6)),
        claimed_by: Some(InstanceId::new("server-a")),
        claim_until: Some(at(40)),
        carried: Carried::default(),
    }
}

/// A callback row with nothing optional set.
fn bare_row() -> CallbackRecord {
    CallbackRecord {
        key: None,
        deadline_seconds: None,
        state: CallbackState::Armed,
        tries: 0,
        next_try_at: None,
        deadline_at: None,
        last_try_at: None,
        last_failure: None,
        delivered_at: None,
        claimed_by: None,
        claim_until: None,
        ..full_row()
    }
}

/// A job's callback is written as its URL, its key's SHA-256 and its deadline (empty for none),
/// and reads back with the digest in place of the key; the row holds none of the key's bytes.
/// It catches a key written into the job row, a deadline lost, and a field read into another.
#[test]
fn a_jobs_callback_round_trips_with_its_keys_digest() {
    for deadline in [Some(0), Some(u32::MAX), None] {
        let job = job_with(Some(callback(deadline)));
        let row = encode_job(&job).expect("encodes");
        let deadline_bytes = deadline.map(u32::to_be_bytes).map(Vec::from);
        let deadline_bytes = deadline_bytes.unwrap_or_default();
        let url = callback(deadline).url;
        let digest = digest();
        let items: [&[u8]; 3] = [url.as_bytes(), digest.bytes(), &deadline_bytes];
        assert_eq!(field_of(&row, CALLBACK), Some(list(items)));
        let stored = JobCallback {
            key: CallbackKey::Digest(digest),
            ..callback(deadline)
        };
        assert_eq!(decode_job(&row), Ok(job_with(Some(stored))));
        let bytes = key();
        assert!(!row.windows(64).any(|window| window == bytes.as_bytes()));
    }
}

/// A job without a callback writes no tag 68, so its row is the row a build before the field
/// wrote, and that row reads with none. It catches a none written as an empty value, and a row
/// from before the field refused.
#[test]
fn no_callback_writes_no_tag_and_a_row_without_it_reads_as_none() {
    let row = encode_job(&job_with(None)).expect("encodes");
    assert_eq!(field_of(&row, CALLBACK), None);
    let asked = encode_job(&job_with(Some(callback(None)))).expect("encodes");
    assert_eq!(without(&asked, CALLBACK), row);
    assert_eq!(decode_job(&row), Ok(job_with(None)));
}

/// A callback field with another number of items than three, a digest of another length than 32
/// bytes, a deadline of another length than four bytes or none, and a URL that is not UTF-8, is
/// refused by its tag. It catches a field read past its items, a short digest padded, and a
/// deadline of another width read as one.
#[test]
fn a_malformed_callback_field_is_refused_by_its_tag() {
    let row = encode_job(&job_with(None)).expect("encodes");
    let digest = digest();
    let url: &[u8] = b"https://hooks.example/";
    let cases: [(Vec<u8>, DecodeError); 6] = [
        (
            list([url, digest.bytes()]),
            DecodeError::BadValue { tag: CALLBACK },
        ),
        (
            list([url, digest.bytes(), &[], &[]]),
            DecodeError::BadValue { tag: CALLBACK },
        ),
        (
            list([url, &[1; 31], &[]]),
            DecodeError::BadLength { tag: CALLBACK },
        ),
        (
            list([url, digest.bytes(), &[0, 0, 1]]),
            DecodeError::BadLength { tag: CALLBACK },
        ),
        (
            list([&[0xff][..], digest.bytes(), &[]]),
            DecodeError::NotUtf8 { tag: CALLBACK },
        ),
        (vec![0, 0, 0, 9], DecodeError::BadValue { tag: CALLBACK }),
    ];
    for (value, error) in cases {
        assert_eq!(
            decode_job(&with(&row, CALLBACK, &value)),
            Err(error),
            "{value:?}"
        );
    }
}

/// A callback row with every field set and one with nothing optional set read back as written,
/// in every state; a field a later build adds is carried into the next write; a row of a later
/// format is a later build's. It catches a field written and not read back, or read into
/// another, and a later build's field dropped.
#[test]
fn a_callback_row_reads_back_as_written() {
    let states = [
        CallbackState::Armed,
        CallbackState::Pending,
        CallbackState::Delivered,
        CallbackState::Expired,
        CallbackState::Abandoned,
    ];
    for state in states {
        for row in [full_row(), bare_row()] {
            let row = CallbackRecord { state, ..row };
            assert_eq!(
                decode_callback(&encode_callback(&row).expect("encodes")),
                Ok(row)
            );
        }
    }
    let failed = CallbackRecord {
        last_failure: Some(FailureClass::Failed),
        ..full_row()
    };
    assert_eq!(
        decode_callback(&encode_callback(&failed).expect("encodes")),
        Ok(failed)
    );
    let bytes = encode_callback(&full_row()).expect("encodes");
    let later = with(&bytes, LATER_TAG, &[1, 2, 3]);
    let read = decode_callback(&later).expect("reads");
    assert_eq!(
        field_of(&encode_callback(&read).expect("encodes"), LATER_TAG),
        Some(vec![1, 2, 3])
    );
    assert_eq!(decode_callback(&[FORMAT + 1]), Err(DecodeError::Newer));
    assert_eq!(decode_callback(&[]), Err(DecodeError::Empty));
}

/// Each malformed field of a callback row is refused by its tag, a required field missing is
/// missing, and a state or failure class this build does not know is a later build's. It
/// catches a key of the wrong length taken, a count of another width read, and an unknown
/// state read as one this build knows.
#[test]
fn a_malformed_callback_row_is_refused_by_its_tag() {
    let row = encode_callback(&full_row()).expect("encodes");
    let cases: Vec<(u16, Vec<u8>, DecodeError)> = vec![
        (t::KEY, vec![1; 31], DecodeError::BadValue { tag: t::KEY }),
        (t::STATE, vec![5], DecodeError::Newer),
        (
            t::STATE,
            vec![1, 1],
            DecodeError::BadLength { tag: t::STATE },
        ),
        (t::LAST_FAILURE, vec![2], DecodeError::Newer),
        (
            t::LAST_FAILURE,
            Vec::new(),
            DecodeError::BadLength {
                tag: t::LAST_FAILURE,
            },
        ),
        (t::TRIES, vec![1], DecodeError::BadLength { tag: t::TRIES }),
        (
            t::DEADLINE_SECONDS,
            vec![0; 8],
            DecodeError::BadLength {
                tag: t::DEADLINE_SECONDS,
            },
        ),
        (
            t::CLAIMED_BY,
            vec![0xff],
            DecodeError::NotUtf8 { tag: t::CLAIMED_BY },
        ),
        (
            t::JOB,
            b"a b".to_vec(),
            DecodeError::BadValue { tag: t::JOB },
        ),
    ];
    for (tag, value, error) in cases {
        assert_eq!(
            decode_callback(&with(&row, tag, &value)),
            Err(error),
            "{tag}"
        );
    }
    for tag in [t::JOB, t::URL, t::WEBHOOK_ID, t::STATE] {
        assert_eq!(
            decode_callback(&without(&row, tag)),
            Err(DecodeError::Missing { tag }),
            "{tag}"
        );
    }
    let untried = without(&row, t::TRIES);
    assert_eq!(decode_callback(&untried).map(|row| row.tries), Ok(0));
}
