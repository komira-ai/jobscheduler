//! Tests of the attempt fields: the defaults a row without them reads as, the bytes they are
//! written as, the fields a build that does not know them carries, every outcome, failure and
//! reason, and every way their values can fail to parse.

use std::collections::BTreeSet;

use super::super::tests::bare_job;
use super::*;
use crate::codec::{LATER_FORMAT, decode_job, encode_job, field_of, rearranged, set_field};
use crate::record::JobRecord;
use crate::result::ResultReason;
use crate::suite::{host, job_id, policy};

/// A time of `secs` seconds and `nanos` nanoseconds, as its twelve bytes: written out here
/// from the format's description, not by the encoder.
fn tm(secs: u64, nanos: u32) -> Vec<u8> {
    let mut out = secs.to_be_bytes().to_vec();
    out.extend_from_slice(&nanos.to_be_bytes());
    out
}

/// The instant `tm` writes.
fn st(secs: u64, nanos: u32) -> StoreTime {
    StoreTime::from_duration(Duration::new(secs, nanos))
}

/// One field of a row: tag, length and value.
fn fld(tag: u16, value: &[u8]) -> Vec<u8> {
    let mut out = tag.to_be_bytes().to_vec();
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// A row of format 1 holding `fields`.
fn row_of(fields: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![1];
    out.extend(fields.concat());
    out
}

/// A list value of `items`.
fn items(items: &[&[u8]]) -> Vec<u8> {
    items
        .iter()
        .flat_map(|item| {
            let mut out = (item.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(item);
            out
        })
        .collect()
}

/// The three-item value of a policy, as the module documents it.
fn three(most: &[u8], initial: &[u8], longest: &[u8]) -> Vec<u8> {
    items(&[most, initial, longest])
}

/// A job whose row has every attempt field set, with two ended attempts.
fn attempted_job() -> JobRecord {
    let mut job = bare_job();
    job.spec.retry = Some(policy(3));
    job.attempt = AttemptTrail::from_parts(
        3,
        vec![first_attempt(), second_attempt()],
        4,
        Some(st(30, 9)),
        BTreeSet::from([host("host-a"), host("host-b")]),
    );
    job
}

/// An attempt with every optional field set.
fn first_attempt() -> Attempt {
    Attempt {
        number: 1,
        host: host("host-a"),
        queued_at: st(10, 1),
        placed_at: st(11, 2),
        started_at: Some(st(12, 3)),
        ended_at: st(13, 4),
        end: AttemptRecord {
            outcome: AttemptOutcome::StartFailed,
            exit_code: Some(-9),
            reason: Some(ResultReason::WallLimit),
            start_failure: Some(StartFailure::AgentRefused),
            start_failure_detail: Some("no-gpu".to_owned()),
        },
        retried: true,
        carried: Carried::default(),
    }
}

/// An attempt with no optional field set.
fn second_attempt() -> Attempt {
    Attempt {
        number: 2,
        host: host("host-b"),
        queued_at: st(20, 0),
        placed_at: st(21, 0),
        started_at: None,
        ended_at: st(22, 0),
        end: AttemptRecord {
            outcome: AttemptOutcome::Succeeded,
            exit_code: None,
            reason: None,
            start_failure: None,
            start_failure_detail: None,
        },
        retried: false,
        carried: Carried::default(),
    }
}

/// The fields of an entry numbered `number` that every entry holds, on host `host-a`, ended
/// as a success.
fn entry_fields(number: u32) -> Vec<Vec<u8>> {
    vec![
        fld(1, &number.to_be_bytes()),
        fld(2, b"host-a"),
        fld(3, &[0]),
        fld(4, &tm(1, 0)),
        fld(5, &tm(2, 0)),
        fld(7, &tm(3, 0)),
    ]
}

/// `fields` without the field `tag`.
fn without(fields: &[Vec<u8>], tag: u16) -> Vec<Vec<u8>> {
    fields
        .iter()
        .filter(|field| field[..2] != tag.to_be_bytes())
        .cloned()
        .collect()
}

/// `fields` with `field` in place of the one of the same tag, or added.
fn with(fields: &[Vec<u8>], tag: u16, value: &[u8]) -> Vec<Vec<u8>> {
    let mut out = without(fields, tag);
    out.push(fld(tag, value));
    out
}

/// `row`'s job row with its history field set to the one entry `entry`.
fn job_row_with_entry(entry: &[u8]) -> Vec<u8> {
    let row = encode_job(&bare_job()).expect("encodes");
    set_field(&row, 70, &items(&[entry])).expect("a row")
}

/// The error of reading a job row whose history holds the one entry made of `fields`.
fn entry_error(fields: &[Vec<u8>]) -> DecodeError {
    decode_job(&job_row_with_entry(&row_of(fields))).expect_err("a bad entry")
}

/// The error of reading a bare job's row with field `tag` set to `value`.
fn job_error(tag: u16, value: &[u8]) -> DecodeError {
    let row = encode_job(&bare_job()).expect("encodes");
    decode_job(&set_field(&row, tag, value).expect("a row")).expect_err("a bad field")
}

/// The error of reading an attempt row made of `fields`.
fn attempt_row_error(fields: &[Vec<u8>]) -> DecodeError {
    decode_attempt_row(&row_of(fields)).expect_err("a bad row")
}

/// The fields of a good attempt row.
fn good_attempt_row() -> Vec<Vec<u8>> {
    vec![
        fld(1, b"job-1"),
        fld(2, &[0, 0, 0, 2]),
        fld(3, b"host-a"),
        fld(4, &tm(5, 6)),
        fld(5, &tm(7, 8)),
    ]
}

/// Asserts a rewrite and a read back of `job` give the job.
fn round_trip(job: &JobRecord) {
    let row = encode_job(job).expect("encodes");
    assert_eq!(decode_job(&row).as_ref(), Ok(job));
}

/// A row with none of tags 67 and 69 to 73 reads as the first attempt with nothing ended and no
/// policy, and a job at those defaults is written without them, so its bytes are what they
/// were before the fields.
///
/// Catches a reader that fails or invents a history for a row an older build wrote, and a
/// writer that adds a tag for a default, which would change every existing job's bytes.
#[test]
fn a_record_without_attempt_tags_reads_as_attempt_one() {
    let name = "a_record_without_attempt_tags_reads_as_attempt_one";
    let mut job = bare_job();
    job.spec.retry = None;
    let row = encode_job(&job).expect("encodes");
    for tag in [67, 69, 70, 71, 72, 73] {
        assert_eq!(field_of(&row, tag), None, "{name}: tag {tag} is written");
    }
    let read = decode_job(&row).expect("reads");
    assert_eq!(read.attempt.number(), 1, "{name}: number");
    assert_eq!(read.attempt.history(), &[][..], "{name}: history");
    assert_eq!(read.attempt.dropped(), 0, "{name}: dropped");
    assert_eq!(read.attempt.retry_not_before(), None, "{name}: not before");
    assert_eq!(read.attempt.hosts(), &BTreeSet::new(), "{name}: hosts");
    assert_eq!(read.spec.retry, None, "{name}: retry");
    assert_eq!(read, job, "{name}: record");
    assert_eq!(encode_job(&read), Ok(row), "{name}: bytes");
}

/// A build that does not know tags 67 and 69 to 73 reads a row that holds them, keeps them as
/// fields it does not know, and writes them back byte for byte when it rewrites the job for a
/// change of a field it does know; this build reads that row as the original attempts and
/// policy.
///
/// Catches a codec whose fields are lost by a build one release back, which would erase a
/// job's attempts and policy whenever an older server rewrites its row.
#[test]
fn a_build_without_the_new_tags_carries_them() {
    let name = "a_build_without_the_new_tags_carries_them";
    let job = attempted_job();
    let row = encode_job(&job).expect("encodes");
    let stripped = rearranged(&row, &[67, 69, 70, 71, 72, 73]).expect("a row");
    let mut older = decode_job(&stripped).expect("reads without the tags");
    assert_eq!(older.attempt, AttemptTrail::default(), "{name}: not read");
    assert_eq!(older.spec.retry, None, "{name}: no policy read");
    for tag in [67, 69, 70, 71, 72, 73] {
        let value = field_of(&row, tag).expect("the original writes it");
        older.carried.0.insert(tag, value);
    }
    older.cancel_requested = true;
    let rewritten = encode_job(&older).expect("encodes");
    for tag in [67, 69, 70, 71, 72, 73] {
        assert_eq!(
            field_of(&rewritten, tag),
            field_of(&row, tag),
            "{name}: tag {tag}"
        );
    }
    let mut expected = job;
    expected.cancel_requested = true;
    assert_eq!(decode_job(&rewritten), Ok(expected), "{name}: read back");
}

/// A job row's tags 67 and 69 to 73 are these bytes, for a record on its third attempt with two
/// ended attempts (one with every optional field, one with none), a not-before time, four
/// dropped attempts, two hosts and a policy; they read back as the record.
///
/// Catches a changed tag number, width, order of an entry's fields or encoding of a time,
/// which no round trip would show and which breaks a build one release away.
#[test]
fn the_attempt_fields_have_these_bytes() {
    let name = "the_attempt_fields_have_these_bytes";
    let job = attempted_job();
    let row = encode_job(&job).expect("encodes");
    let first = row_of(&[
        fld(1, &[0, 0, 0, 1]),
        fld(2, b"host-a"),
        fld(3, &[3]),
        fld(4, &tm(10, 1)),
        fld(5, &tm(11, 2)),
        fld(6, &tm(12, 3)),
        fld(7, &tm(13, 4)),
        fld(8, &[0xff, 0xff, 0xff, 0xf7]),
        fld(9, &[1]),
        fld(10, &[11]),
        fld(11, b"no-gpu"),
        fld(12, &[1]),
    ]);
    let second = row_of(&[
        fld(1, &[0, 0, 0, 2]),
        fld(2, b"host-b"),
        fld(3, &[0]),
        fld(4, &tm(20, 0)),
        fld(5, &tm(21, 0)),
        fld(7, &tm(22, 0)),
        fld(12, &[0]),
    ]);
    let expected = [
        (69, vec![0, 0, 0, 3]),
        (70, items(&[&first, &second])),
        (71, tm(30, 9)),
        (72, vec![0, 0, 0, 4]),
        (73, items(&[b"host-a", b"host-b"])),
        (67, three(&[0, 0, 0, 3], &tm(5, 0), &tm(60, 0))),
    ];
    for (tag, bytes) in expected {
        assert_eq!(field_of(&row, tag), Some(bytes), "{name}: tag {tag}");
    }
    assert_eq!(decode_job(&row), Ok(job), "{name}: read back");
}

/// An attempt row is these bytes (its job, number, host, and when it was placed and ended),
/// and reads back as its job, number and where and when it ran.
///
/// Catches a changed tag or encoding of the row a log read of an old attempt depends on.
#[test]
fn an_attempt_row_has_these_bytes() {
    let name = "an_attempt_row_has_these_bytes";
    let mut attempt = first_attempt();
    attempt.number = 2;
    attempt.placed_at = st(5, 6);
    attempt.ended_at = st(7, 8);
    let row = encode_attempt_row(&job_id("job-1"), &attempt).expect("encodes");
    assert_eq!(row, row_of(&good_attempt_row()), "{name}: bytes");
    let expected = AttemptRow {
        host: host("host-a"),
        placed_at: st(5, 6),
        ended_at: st(7, 8),
    };
    assert_eq!(
        decode_attempt_row(&row),
        Ok((job_id("job-1"), 2, expected)),
        "{name}: read back"
    );
}

/// Every outcome, every start failure, every reason, a retried and an unretried attempt, and
/// every shape of a policy (both waits, none, only the first, only the second) read back as
/// written.
///
/// Catches a code swapped, missing or mapped to another value in either direction, and a
/// wait of a policy that is lost or moved to the other.
#[test]
fn every_outcome_failure_reason_and_policy_reads_back() {
    let name = "every_outcome_failure_reason_and_policy_reads_back";
    let outcomes = [
        AttemptOutcome::Succeeded,
        AttemptOutcome::Failed,
        AttemptOutcome::HostLost,
        AttemptOutcome::StartFailed,
        AttemptOutcome::Cancelled,
    ];
    let failures = [
        StartFailure::NoCapacity,
        StartFailure::StartTimeout,
        StartFailure::ExclusiveNotReady,
        StartFailure::AgentRefused,
        StartFailure::InputUnavailable,
    ];
    let reasons = [
        ResultReason::Exit,
        ResultReason::WallLimit,
        ResultReason::Memory,
        ResultReason::Cancelled,
        ResultReason::InputFailed,
    ];
    let mut history = Vec::new();
    for (i, outcome) in outcomes.into_iter().enumerate() {
        let mut attempt = second_attempt();
        attempt.number = i as u32 + 1;
        attempt.end.outcome = outcome;
        attempt.retried = i % 2 == 0;
        history.push(attempt);
    }
    for (i, failure) in failures.into_iter().enumerate() {
        let mut attempt = first_attempt();
        attempt.number = i as u32 + 6;
        attempt.end.start_failure = Some(failure);
        attempt.end.start_failure_detail = None;
        history.push(attempt);
    }
    for (i, reason) in reasons.into_iter().enumerate() {
        let mut attempt = first_attempt();
        attempt.number = i as u32 + 11;
        attempt.end.reason = Some(reason);
        attempt.retried = false;
        history.push(attempt);
    }
    let mut job = bare_job();
    job.attempt = AttemptTrail::from_parts(16, history, 0, None, BTreeSet::new());
    round_trip(&job);
    let waits = |initial, longest| {
        Some(RetryPolicy {
            max_attempts: 7,
            initial_backoff: initial,
            max_backoff: longest,
        })
    };
    let one = Some(Duration::new(2, 5));
    let two = Some(Duration::from_secs(90));
    for retry in [
        waits(one, two),
        waits(None, None),
        waits(one, None),
        waits(None, two),
    ] {
        let mut job = bare_job();
        job.spec.retry = retry;
        round_trip(&job);
        let row = encode_job(&job).expect("encodes");
        assert!(field_of(&row, 67).is_some(), "{name}: written");
    }
}

/// A policy's value is three items: the attempts, then each wait as a time or empty.
///
/// Catches a policy written with an empty wait as a zero time (a policy that then has no
/// default) or its waits swapped.
#[test]
fn a_policy_without_waits_writes_them_empty() {
    let name = "a_policy_without_waits_writes_them_empty";
    let mut job = bare_job();
    job.spec.retry = Some(RetryPolicy {
        max_attempts: 5,
        initial_backoff: None,
        max_backoff: Some(Duration::from_secs(9)),
    });
    let row = encode_job(&job).expect("encodes");
    assert_eq!(
        field_of(&row, 67),
        Some(three(&[0, 0, 0, 5], &[], &tm(9, 0))),
        "{name}: bytes"
    );
}

/// An attempt carrying a tag this build does not know keeps it across a read and a write, in
/// the same bytes.
///
/// Catches an entry that drops a later build's fields, which an older server would erase from
/// every history it rewrites.
#[test]
fn an_attempt_keeps_a_tag_this_build_does_not_know() {
    let name = "an_attempt_keeps_a_tag_this_build_does_not_know";
    let mut fields = entry_fields(1);
    fields.push(fld(12, &[0]));
    fields.push(fld(99, b"later"));
    let row = job_row_with_entry(&row_of(&fields));
    let job = decode_job(&row).expect("reads");
    let kept = Carried([(99, b"later".to_vec())].into());
    assert_eq!(job.attempt.history()[0].carried, kept, "{name}: carried");
    let again = encode_job(&job).expect("encodes");
    assert_eq!(field_of(&again, 70), field_of(&row, 70), "{name}: bytes");
}

/// An entry without the retried field reads as not retried.
///
/// Catches a default of true, which would queue a retry that never happened.
#[test]
fn an_attempt_without_the_retried_field_was_not_retried() {
    let name = "an_attempt_without_the_retried_field_was_not_retried";
    let job = decode_job(&job_row_with_entry(&row_of(&entry_fields(1)))).expect("reads");
    assert!(!job.attempt.history()[0].retried, "{name}: retried");
}

/// A policy whose value is not three items, or whose items are not a count and two times,
/// is a bad value or a bad length of tag 67.
///
/// Catches a policy read leniently (two items, four items, a short count, a wait of another
/// width or with nanoseconds past a second), which would give a job another policy than the
/// one it was submitted with.
#[test]
fn a_bad_policy_value_is_refused() {
    let name = "a_bad_policy_value_is_refused";
    let count = [0, 0, 0, 3];
    let bad = |value: &[u8]| job_error(67, value);
    let two_items = items(&[&count, &[]]);
    assert_eq!(
        bad(&two_items),
        DecodeError::BadValue { tag: 67 },
        "{name}: 2"
    );
    let four = items(&[&count, &[], &[], &[]]);
    assert_eq!(bad(&four), DecodeError::BadValue { tag: 67 }, "{name}: 4");
    assert_eq!(
        bad(&[0, 0, 0, 9, 1]),
        DecodeError::BadValue { tag: 67 },
        "{name}: cut"
    );
    let short = three(&[0, 0, 3], &[], &[]);
    assert_eq!(
        bad(&short),
        DecodeError::BadLength { tag: 67 },
        "{name}: count"
    );
    let width = three(&count, &[1; 11], &[]);
    assert_eq!(
        bad(&width),
        DecodeError::BadLength { tag: 67 },
        "{name}: initial"
    );
    let width = three(&count, &[], &[1; 13]);
    assert_eq!(
        bad(&width),
        DecodeError::BadLength { tag: 67 },
        "{name}: longest"
    );
    let nanos = tm(1, 1_000_000_000);
    let late = three(&count, &nanos, &[]);
    assert_eq!(
        bad(&late),
        DecodeError::BadValue { tag: 67 },
        "{name}: nanos"
    );
    let late = three(&count, &[], &nanos);
    assert_eq!(
        bad(&late),
        DecodeError::BadValue { tag: 67 },
        "{name}: longest nanos"
    );
}

/// A job's attempt fields that are the wrong width, zero where a number starts at 1, a time
/// with nanoseconds past a second, or hosts that repeat or are not host ids are refused with
/// the tag named.
///
/// Catches a number 0 that makes the next placement count attempt 1 again, a repeated host
/// that silently shrinks the set, and a width read as another.
#[test]
fn a_bad_job_attempt_field_is_refused() {
    let name = "a_bad_job_attempt_field_is_refused";
    let cases = [
        (69, vec![0, 0, 0, 0], DecodeError::BadValue { tag: 69 }),
        (69, vec![0, 0, 1], DecodeError::BadLength { tag: 69 }),
        (71, vec![0; 11], DecodeError::BadLength { tag: 71 }),
        (71, tm(0, 1_000_000_000), DecodeError::BadValue { tag: 71 }),
        (72, vec![0; 8], DecodeError::BadLength { tag: 72 }),
        (
            73,
            items(&[b"host-a", b"host-b", b"host-a"]),
            DecodeError::BadValue { tag: 73 },
        ),
        (73, items(&[b"host a"]), DecodeError::BadValue { tag: 73 }),
        (73, items(&[&[0xff]]), DecodeError::NotUtf8 { tag: 73 }),
        (73, vec![0, 0, 0, 9, 1], DecodeError::BadValue { tag: 73 }),
        (70, vec![0, 0, 0, 9, 1], DecodeError::BadValue { tag: 70 }),
    ];
    for (tag, value, error) in cases {
        assert_eq!(job_error(tag, &value), error, "{name}: tag {tag}");
    }
}

/// An entry of the history that is empty, cut, of format 0 or of a later format is an
/// error that says so.
///
/// Catches a history entry read without its format byte checked, which would read a later
/// build's layout as this one's.
#[test]
fn a_bad_history_entry_is_refused() {
    let name = "a_bad_history_entry_is_refused";
    let bad = |entry: &[u8]| decode_job(&job_row_with_entry(entry)).expect_err("a bad entry");
    assert_eq!(bad(&[]), DecodeError::Empty, "{name}: empty");
    assert_eq!(bad(&[1, 0]), DecodeError::Truncated, "{name}: cut");
    let row = row_of(&entry_fields(1));
    let mut format_0 = row.clone();
    format_0[0] = 0;
    assert_eq!(bad(&format_0), DecodeError::BadFormat, "{name}: format 0");
    let mut later = row;
    later[0] = 2;
    assert_eq!(bad(&later), DecodeError::Newer, "{name}: format 2");
    later[0] = LATER_FORMAT;
    assert_eq!(bad(&later), DecodeError::Newer, "{name}: later format");
}

/// An entry missing any of its required fields names it.
///
/// Catches an entry that reads without its number, host, outcome or times, and so invents
/// them.
#[test]
fn an_entry_missing_a_required_field_names_it() {
    let name = "an_entry_missing_a_required_field_names_it";
    for tag in [1, 2, 3, 4, 5, 7] {
        let fields = without(&entry_fields(1), tag);
        assert_eq!(
            entry_error(&fields),
            DecodeError::Missing { tag },
            "{name}: tag {tag}"
        );
    }
}

/// An entry's values that are zero for a number, an outcome, start failure or reason this
/// build does not know, a state that is not a start failure, a bad host or byte or width are
/// refused: a later build's value as newer, the others as bad.
///
/// Catches a later build's outcome read as a known one, and a start failure naming a state
/// that is no failure to start.
#[test]
fn a_bad_entry_value_is_refused() {
    let name = "a_bad_entry_value_is_refused";
    let base = entry_fields(1);
    let cases = [
        (1, vec![0, 0, 0, 0], DecodeError::BadValue { tag: 1 }),
        (2, b"host a".to_vec(), DecodeError::BadValue { tag: 2 }),
        (3, vec![5], DecodeError::Newer),
        (3, vec![0, 0], DecodeError::BadLength { tag: 3 }),
        (8, vec![0; 3], DecodeError::BadLength { tag: 8 }),
        (9, vec![5], DecodeError::Newer),
        (10, vec![4], DecodeError::BadValue { tag: 10 }),
        (10, vec![0], DecodeError::BadValue { tag: 10 }),
        (10, vec![13], DecodeError::Newer),
        (11, vec![0xff], DecodeError::NotUtf8 { tag: 11 }),
        (12, vec![2], DecodeError::BadValue { tag: 12 }),
    ];
    for (tag, value, error) in cases {
        let fields = with(&base, tag, &value);
        assert_eq!(entry_error(&fields), error, "{name}: tag {tag} {value:?}");
    }
}

/// A row of an attempt missing any of its fields names it, and one with a bad job id, a number
/// of 0, a later format or format 0 is refused.
///
/// Catches an attempt row read without its job and number checked, which would show one
/// job's host as another's.
#[test]
fn a_bad_attempt_row_is_refused() {
    let name = "a_bad_attempt_row_is_refused";
    for tag in 1..=5 {
        let fields = without(&good_attempt_row(), tag);
        assert_eq!(
            attempt_row_error(&fields),
            DecodeError::Missing { tag },
            "{name}: missing {tag}"
        );
    }
    let bad_job = with(&good_attempt_row(), 1, b"job 1");
    assert_eq!(
        attempt_row_error(&bad_job),
        DecodeError::BadValue { tag: 1 },
        "{name}: job"
    );
    let zero = with(&good_attempt_row(), 2, &[0; 4]);
    assert_eq!(
        attempt_row_error(&zero),
        DecodeError::BadValue { tag: 2 },
        "{name}: number"
    );
    let bad_host = with(&good_attempt_row(), 3, b"host a");
    assert_eq!(
        attempt_row_error(&bad_host),
        DecodeError::BadValue { tag: 3 },
        "{name}: host"
    );
    let mut row = row_of(&good_attempt_row());
    row[0] = 2;
    assert_eq!(
        decode_attempt_row(&row),
        Err(DecodeError::Newer),
        "{name}: newer"
    );
    row[0] = 0;
    assert_eq!(
        decode_attempt_row(&row),
        Err(DecodeError::BadFormat),
        "{name}: format 0"
    );
    assert_eq!(
        decode_attempt_row(&[]),
        Err(DecodeError::Empty),
        "{name}: empty"
    );
}

/// A row that holds 257 ended attempts and 2 counted as dropped reads as the newest 256 and
/// 3 dropped, and one whose count is the most a `u32` holds stays there.
///
/// Catches a history read without its bound, which a later or hostile row would grow without
/// limit, and a dropped count that wraps.
#[test]
fn a_history_over_its_bound_is_bounded_and_counted() {
    let name = "a_history_over_its_bound_is_bounded_and_counted";
    let entries: Vec<Vec<u8>> = (1..=257).map(|n| row_of(&entry_fields(n))).collect();
    let refs: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
    let row = encode_job(&bare_job()).expect("encodes");
    let row = set_field(&row, 70, &items(&refs)).expect("a row");
    let row = set_field(&row, 72, &[0, 0, 0, 2]).expect("a row");
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.attempt.history().len(), 256, "{name}: kept");
    assert_eq!(job.attempt.history()[0].number, 2, "{name}: oldest");
    assert_eq!(job.attempt.history()[255].number, 257, "{name}: newest");
    assert_eq!(job.attempt.dropped(), 3, "{name}: dropped");
    let row = set_field(&row, 72, &u32::MAX.to_be_bytes()).expect("a row");
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.attempt.dropped(), u32::MAX, "{name}: saturated");
}
