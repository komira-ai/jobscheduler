//! `GetLogs` with a stream (`logs.rs`, `logs/page.rs`) over both transports, reading objects
//! that js-logstore's client put in its fake store with js-logstore's layout: offsets and
//! resuming, raw bytes, finals, gap markers, holes and the settle window, chunks deleted under
//! the reader, the objects of one job only, the refusals, a store that fails, and the page's
//! bounds. Every answer a test reads with [`Rig::read`] is asked over gRPC and the gateway,
//! which must agree. Nothing sleeps: the settle window passes on the manual clock.

use std::time::Duration;

use js_jobs::{JobEnd, Output};
use js_logstore::{MAX_CHUNK_BYTES, Stream};
use js_proto::v1::{self, GetLogsRequest, GetLogsResponse, LogGap};
use tonic::{Code, Status};

use super::logs::page::LOG_SETTLE;
use super::logs::{DEFAULT_PAGE_BYTES, MAX_PAGE_BYTES};
use super::logs_rig::{Failure, Rig, ask, end, end_as, pattern, prefix};
use super::read_tests::{self, queue, running, spec};
use crate::api::rig::Lines;

/// The request for stdout of `job` from `from`.
fn out(job: &str, from: u64) -> GetLogsRequest {
    ask(job, v1::LogStream::Stdout, from)
}

/// The request for stdout of `job` from `from`, at most `max` bytes.
fn out_max(job: &str, from: u64, max: u32) -> GetLogsRequest {
    GetLogsRequest {
        max_bytes: Some(max),
        ..out(job, from)
    }
}

/// The answer of a job in `state` whose page asked from `from` is `data` at `offset`, at the
/// end or not, with `first` as its first-byte time.
fn page(
    state: v1::JobState,
    from: u64,
    offset: u64,
    data: &[u8],
    eof: bool,
    first: Option<u64>,
) -> GetLogsResponse {
    let gaps = match offset > from {
        true => vec![LogGap {
            offset: from,
            length: offset - from,
        }],
        false => Vec::new(),
    };
    GetLogsResponse {
        state: state.into(),
        data: data.to_vec(),
        offset,
        next_offset: offset + u64::try_from(data.len()).unwrap(),
        gaps,
        eof,
        first_byte_at_ms: first,
        ..GetLogsResponse::default()
    }
}

/// The (code, message) of a refusal.
fn refusal<T: std::fmt::Debug>(answer: Result<T, Status>) -> (Code, String) {
    let status = answer.expect_err("the call is refused");
    (status.code(), status.message().to_owned())
}

/// `n` as a byte offset.
fn at(n: usize) -> u64 {
    u64::try_from(n).unwrap()
}

/// Reads stdout of `job` over gRPC from `from` with pages of `max` bytes until the page ends
/// at `until` or the stream's end, asserting that each page starts where the last one ended
/// and has no gap; the bytes read.
async fn reassemble(rig: &Rig, job: &str, from: u64, max: Option<u32>, until: u64) -> Vec<u8> {
    let mut read = Vec::new();
    let mut next = from;
    loop {
        let request = GetLogsRequest {
            max_bytes: max,
            ..out(job, next)
        };
        let answer = rig.grpc(request).await.expect("a page");
        assert_eq!(
            (answer.offset, answer.gaps.len()),
            (next, 0),
            "{max:?} {next}"
        );
        let cap = max.map_or(DEFAULT_PAGE_BYTES, |max| max.min(MAX_PAGE_BYTES));
        assert!(
            answer.data.len() <= usize::try_from(cap).unwrap(),
            "{max:?}"
        );
        read.extend_from_slice(&answer.data);
        next = answer.next_offset;
        if answer.eof || next >= until {
            return read;
        }
        assert!(!answer.data.is_empty(), "{max:?} {next}");
    }
}

/// Asserts that a stream of chunks of 100, 50 and 1,048,576 bytes reads, while the job runs,
/// exactly the bytes from 0, 99, 100, 149, the middle of the big chunk, its last byte and the
/// end (an empty page there), with the first-byte time of the chunk each page begins in; and
/// that once the job has ended, reading on from each answer's `next_offset` until `eof`
/// gives the whole stream byte for byte, with the default page, pages of 64 KiB, 1 MiB and
/// one cut to 1 MiB, and, over the first and the last 300 bytes, pages of 1 to 8 bytes and
/// of one less, as many and one more than each chunk edge. Catches a byte duplicated or
/// skipped at a chunk edge, a page that is not contiguous, and a resume that is not exact.
#[tokio::test]
async fn a_stream_is_read_from_any_offset_and_resumes_exactly() {
    let rig = Rig::new().await;
    running(rig.jobs(), "big-1");
    let stream = pattern(100 + 50 + MAX_CHUNK_BYTES);
    let total = stream.len();
    rig.chunk("big-1", Stream::Stdout, 0, &stream[..100], 11);
    rig.chunk("big-1", Stream::Stdout, 100, &stream[100..150], 22);
    rig.chunk("big-1", Stream::Stdout, 150, &stream[150..], 33);
    let default = usize::try_from(DEFAULT_PAGE_BYTES).unwrap();
    for (from, first) in [
        (0, 11),
        (99, 11),
        (100, 22),
        (149, 22),
        (150 + MAX_CHUNK_BYTES / 2, 33),
        (total - 1, 33),
    ] {
        let to = (from + default).min(total);
        let answer = rig.read(out("big-1", at(from))).await.expect("a page");
        let live = v1::JobState::Running;
        let expected = page(
            live,
            at(from),
            at(from),
            &stream[from..to],
            false,
            Some(first),
        );
        assert_eq!(answer, expected, "{from}");
    }
    let answer = rig.read(out("big-1", at(total))).await.expect("a page");
    let empty = page(
        v1::JobState::Running,
        at(total),
        at(total),
        b"",
        false,
        None,
    );
    assert_eq!(answer, empty);

    end(rig.jobs(), "big-1", (at(total), 0), (0, 0));
    for max in [None, Some(65_536), Some(MAX_PAGE_BYTES), Some(4_000_000)] {
        let read = reassemble(&rig, "big-1", 0, max, u64::MAX).await;
        assert!(read == stream, "{max:?}");
    }
    let small = (1..=8).chain([49, 50, 51, 99, 100, 101, 149, 150, 151]);
    for max in small {
        let head = reassemble(&rig, "big-1", 0, Some(max), 300).await;
        assert_eq!(head, stream[..head.len()], "{max}");
        assert!(head.len() >= 300, "{max}");
        let tail = reassemble(&rig, "big-1", at(total - 300), Some(max), u64::MAX).await;
        assert_eq!(tail, stream[total - 300..], "{max}");
    }
}

/// Asserts that stdout and stderr, holding bytes that are not UTF-8, NULs, `\r\n` and an
/// escape, come back exactly as put, each from its own stream, while the job runs from their
/// chunks and after it ended from their finals. Catches streams mixed up, bytes re-encoded,
/// wrapped or cut at a NUL, and a page from a final carrying a first-byte time.
#[tokio::test]
async fn stdout_and_stderr_are_separate_and_the_bytes_are_raw() {
    let rig = Rig::new().await;
    running(rig.jobs(), "raw-1");
    let stdout = b"out\0\xff\r\n\x80";
    let stderr = b"\x1b[31merr\r\n\0";
    rig.chunk("raw-1", Stream::Stdout, 0, stdout, 5);
    rig.chunk("raw-1", Stream::Stderr, 0, stderr, 6);
    let live = v1::JobState::Running;
    let err = |from| ask("raw-1", v1::LogStream::Stderr, from);
    let answer = rig.read(out("raw-1", 0)).await.expect("stdout");
    assert_eq!(answer, page(live, 0, 0, stdout, false, Some(5)));
    let answer = rig.read(err(0)).await.expect("stderr");
    assert_eq!(answer, page(live, 0, 0, stderr, false, Some(6)));

    end(rig.jobs(), "raw-1", (8, 0), (11, 0));
    rig.final_file("raw-1", Stream::Stdout, stdout);
    rig.final_file("raw-1", Stream::Stderr, stderr);
    let ended = v1::JobState::Succeeded;
    let answer = rig.read(out("raw-1", 0)).await.expect("stdout");
    assert_eq!(answer, page(ended, 0, 0, stdout, true, None));
    let answer = rig.read(err(2)).await.expect("stderr");
    assert_eq!(answer, page(ended, 2, 2, &stderr[2..], true, None));
}

/// Asserts that an ended job's final serves any range by offset and page size, at the end
/// when it reaches the object's length, and answers at the stream's length with an empty page
/// at the end and no store request, and past it `OUT_OF_RANGE` naming `fromOffset`; that with
/// both a final and chunks (a deletion not yet done) the final is read and no chunk is; that
/// a lost job's final, of no reported length, ends where it ends; that a key beside the
/// final's name is not a final; and that a final gone between its listing and its read falls
/// back to the chunks. Catches a chunk read when a final exists, a range off by one, and an
/// end of stream missed or invented.
#[tokio::test]
async fn a_final_file_is_preferred_and_serves_ranges() {
    let rig = Rig::new().await;
    let ok = v1::JobState::Succeeded;
    running(rig.jobs(), "final-1");
    end(rig.jobs(), "final-1", (10, 0), (0, 0));
    rig.final_file("final-1", Stream::Stdout, b"0123456789");
    let answer = rig.read(out_max("final-1", 0, 4)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"0123", false, None));
    let answer = rig.read(out_max("final-1", 4, 4)).await.expect("a page");
    assert_eq!(answer, page(ok, 4, 4, b"4567", false, None));
    let answer = rig.read(out_max("final-1", 8, 4)).await.expect("a page");
    assert_eq!(answer, page(ok, 8, 8, b"89", true, None));
    let asked = rig.asked().len();
    let answer = rig.read(out("final-1", 10)).await.expect("a page");
    assert_eq!(answer, page(ok, 10, 10, b"", true, None));
    assert_eq!(rig.asked().len(), asked);
    let past = (Code::OutOfRange, "field: fromOffset".to_owned());
    assert_eq!(refusal(rig.read(out("final-1", 11)).await), past);

    running(rig.jobs(), "final-2");
    rig.chunk("final-2", Stream::Stdout, 0, b"XXXXXXXXXX", 1);
    end(rig.jobs(), "final-2", (10, 0), (0, 0));
    rig.final_file("final-2", Stream::Stdout, b"abcdefghij");
    let answer = rig.read(out("final-2", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"abcdefghij", true, None));
    let chunk = prefix("final-2").chunk_key(Stream::Stdout, 0);
    assert!(!rig.asked().contains(&chunk));

    let lost = running(rig.jobs(), "final-3");
    end_as(rig.jobs(), &lost, JobEnd::Lost);
    rig.final_file("final-3", Stream::Stdout, b"hello");
    let gone = v1::JobState::Lost;
    let answer = rig.read(out("final-3", 0)).await.expect("a page");
    assert_eq!(answer, page(gone, 0, 0, b"hello", true, None));
    let answer = rig.read(out("final-3", 5)).await.expect("a page");
    assert_eq!(answer, page(gone, 5, 5, b"", true, None));

    running(rig.jobs(), "final-4");
    rig.chunk("final-4", Stream::Stdout, 0, b"abc", 4);
    end(rig.jobs(), "final-4", (3, 0), (0, 0));
    let beside = format!("{}stdout.log.part", prefix("final-4").as_str());
    rig.put(&beside, b"not the final");
    let answer = rig.read(out("final-4", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"abc", true, Some(4)));

    running(rig.jobs(), "final-5");
    rig.chunk("final-5", Stream::Stdout, 0, b"abc", 7);
    end(rig.jobs(), "final-5", (3, 0), (0, 0));
    rig.final_file("final-5", Stream::Stdout, b"abc");
    let writer = rig.writer.clone();
    let key = prefix("final-5").final_key(Stream::Stdout);
    let deleted = key.clone();
    rig.tap
        .before(&key, move || writer.delete(&deleted).expect("deleted"));
    let answer = rig.read(out("final-5", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"abc", true, Some(7)));
}

/// Asserts the agent's race between writing the final and deleting the chunks: for an ended
/// job, a chunk deleted (with the final put) after the reader listed it is read from the
/// final, whole, with no gap; and a hole about to be reported, at the end or between two
/// chunks, is read from a final put after the reader first looked for one. For a running job,
/// a chunk deleted after the listing is a hole (empty until it settles, then a gap), and a
/// later chunk deleted after the first was read ends the page there. Catches the race
/// reported as a gap or an error, and a page that reads past a chunk that is gone.
#[tokio::test]
async fn a_chunk_deleted_after_the_listing_is_served_from_the_final() {
    let rig = Rig::new().await;
    let ok = v1::JobState::Succeeded;
    let bytes = pattern(200);
    running(rig.jobs(), "race-1");
    rig.chunk("race-1", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("race-1", Stream::Stdout, 100, &bytes[100..150], 2);
    end(rig.jobs(), "race-1", (150, 0), (0, 0));
    let writer = rig.writer.clone();
    let whole = bytes[..150].to_vec();
    let race = prefix("race-1");
    let (final_key, first, second) = (
        race.final_key(Stream::Stdout),
        race.chunk_key(Stream::Stdout, 0),
        race.chunk_key(Stream::Stdout, 100),
    );
    rig.tap.before(&first.clone(), move || {
        writer.put(&final_key, whole, &Default::default()).unwrap();
        writer.delete(&first).unwrap();
        writer.delete(&second).unwrap();
    });
    let answer = rig.read(out("race-1", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, &bytes[..150], true, None));

    for (job, from, chunks) in [("race-4", 100, vec![0]), ("race-5", 100, vec![0, 150])] {
        running(rig.jobs(), job);
        for offset in chunks {
            let to = if offset == 0 { 100 } else { 200 };
            rig.chunk(job, Stream::Stdout, at(offset), &bytes[offset..to], 3);
        }
        end(rig.jobs(), job, (200, 0), (0, 0));
        rig.advance(LOG_SETTLE);
        let writer = rig.writer.clone();
        let whole = bytes.clone();
        let final_key = prefix(job).final_key(Stream::Stdout);
        rig.tap.before(&format!("{job}%2Fstdout%2F"), move || {
            writer.put(&final_key, whole, &Default::default()).unwrap();
        });
        let answer = rig.grpc(out(job, from)).await.expect("a page");
        assert_eq!(
            answer,
            page(ok, from, from, &bytes[100..], true, None),
            "{job}"
        );
    }

    let live = v1::JobState::Running;
    running(rig.jobs(), "race-2");
    rig.chunk("race-2", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("race-2", Stream::Stdout, 100, &bytes[100..150], 2);
    let writer = rig.writer.clone();
    let key = prefix("race-2").chunk_key(Stream::Stdout, 0);
    let deleted = key.clone();
    rig.tap
        .before(&key, move || writer.delete(&deleted).expect("deleted"));
    let answer = rig.grpc(out("race-2", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"", false, None));
    rig.advance(LOG_SETTLE);
    let answer = rig.read(out("race-2", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 100, &bytes[100..150], false, Some(2)));

    running(rig.jobs(), "race-3");
    rig.chunk("race-3", Stream::Stdout, 0, &bytes[..10], 1);
    rig.chunk("race-3", Stream::Stdout, 10, &bytes[10..20], 2);
    let writer = rig.writer.clone();
    let key = prefix("race-3").chunk_key(Stream::Stdout, 10);
    let deleted = key.clone();
    rig.tap
        .before(&key, move || writer.delete(&deleted).expect("deleted"));
    let answer = rig.grpc(out("race-3", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, &bytes[..10], false, Some(1)));
}

/// Asserts that with chunks `[0,100)` and `[150,200)` and a gap marker at 100 of length 50, a
/// read from 0 answers the first chunk and stops before the gap; one from 100 reports the gap
/// `{100, 50}` and answers the second chunk at 150; one from 120 reports `{120, 30}`; that a
/// marker whose body is not a marker's, or that is gone when read, and an empty chunk, are a
/// hole, waited for until it settles; and that an ended job's dropped bytes are one gap from
/// where they begin to the reported length, at the end, without a store request, while the
/// page before them stops where they begin and an object past it is not read. Catches a gap
/// skipped silently, a byte of a gap invented, and a dropped tail read as data.
#[tokio::test]
async fn a_gap_marker_is_reported_and_skipped() {
    let rig = Rig::new().await;
    let live = v1::JobState::Running;
    let bytes = pattern(300);
    running(rig.jobs(), "gap-1");
    rig.chunk("gap-1", Stream::Stdout, 0, &bytes[..100], 1);
    rig.marker("gap-1", Stream::Stdout, 100, 50);
    rig.chunk("gap-1", Stream::Stdout, 150, &bytes[150..200], 2);
    let answer = rig.read(out("gap-1", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, &bytes[..100], false, Some(1)));
    let answer = rig.read(out("gap-1", 100)).await.expect("a page");
    assert_eq!(
        answer,
        page(live, 100, 150, &bytes[150..200], false, Some(2))
    );
    let answer = rig.read(out("gap-1", 120)).await.expect("a page");
    assert_eq!(
        answer,
        page(live, 120, 150, &bytes[150..200], false, Some(2))
    );

    running(rig.jobs(), "gap-2");
    rig.put(&prefix("gap-2").gap_key(Stream::Stdout, 100), b"nonsense");
    rig.chunk("gap-2", Stream::Stdout, 150, &bytes[150..200], 2);
    running(rig.jobs(), "gap-3");
    rig.marker("gap-3", Stream::Stdout, 100, 50);
    rig.chunk("gap-3", Stream::Stdout, 150, &bytes[150..200], 2);
    let writer = rig.writer.clone();
    let marker = prefix("gap-3").gap_key(Stream::Stdout, 100);
    let deleted = marker.clone();
    rig.tap
        .before(&marker, move || writer.delete(&deleted).expect("deleted"));
    running(rig.jobs(), "gap-4");
    rig.chunk("gap-4", Stream::Stdout, 100, b"", 1);
    rig.chunk("gap-4", Stream::Stdout, 150, &bytes[150..200], 2);
    for job in ["gap-2", "gap-3", "gap-4"] {
        let answer = rig.grpc(out(job, 100)).await.expect("a page");
        assert_eq!(answer, page(live, 100, 100, b"", false, None), "{job}");
    }
    rig.advance(LOG_SETTLE);
    for job in ["gap-2", "gap-4"] {
        let answer = rig.read(out(job, 100)).await.expect("a page");
        let expected = page(live, 100, 150, &bytes[150..200], false, Some(2));
        assert_eq!(answer, expected, "{job}");
    }

    let ok = v1::JobState::Succeeded;
    running(rig.jobs(), "drop-1");
    rig.chunk("drop-1", Stream::Stdout, 0, &bytes[..200], 1);
    rig.chunk("drop-1", Stream::Stdout, 200, b"zzz", 9);
    end(rig.jobs(), "drop-1", (300, 100), (0, 0));
    let answer = rig.read(out("drop-1", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, &bytes[..200], false, Some(1)));
    let asked = rig.asked().len();
    for from in [200, 250, 300] {
        let answer = rig.read(out("drop-1", from)).await.expect("a page");
        assert_eq!(answer, page(ok, from, 300, b"", true, None), "{from}");
    }
    assert_eq!(rig.asked().len(), asked);
    running(rig.jobs(), "drop-2");
    end(rig.jobs(), "drop-2", (50, 50), (0, 0));
    let answer = rig.read(out("drop-2", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 50, b"", true, None));
}

/// Asserts the settle window: an ended job whose result says 300 bytes and whose store holds
/// 200 answers from 200, inside the window, an empty page that is neither a gap nor the end,
/// and after it the gap `{200, 100}` and the end; a hole between two chunks of a running job
/// is waited for until the chunk after it was written that long ago, and is then a gap; an
/// ended job's hole between chunks is a gap once it settled; a lost job, with no reported
/// length, ends where its objects end once the window passed, with no gap; and a job that
/// was never placed has printed nothing, at the end at once. Catches data invented, a reader
/// that hangs on a hole, and an end declared early.
#[tokio::test]
async fn missing_bytes_after_the_settle_window_become_a_gap_and_before_it_are_waited_for() {
    let rig = Rig::new().await;
    let ok = v1::JobState::Succeeded;
    let live = v1::JobState::Running;
    let bytes = pattern(300);
    running(rig.jobs(), "short-1");
    rig.chunk("short-1", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("short-1", Stream::Stdout, 100, &bytes[100..200], 2);
    running(rig.jobs(), "hole-1");
    rig.chunk("hole-1", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("hole-1", Stream::Stdout, 200, &bytes[200..], 3);
    running(rig.jobs(), "hole-2");
    rig.chunk("hole-2", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("hole-2", Stream::Stdout, 150, &bytes[150..200], 2);
    let lost = running(rig.jobs(), "lost-1");
    rig.chunk("lost-1", Stream::Stdout, 0, &bytes[..100], 1);
    end(rig.jobs(), "short-1", (300, 0), (0, 0));
    end(rig.jobs(), "hole-2", (200, 0), (0, 0));
    end_as(rig.jobs(), &lost, JobEnd::Lost);
    let gone = v1::JobState::Lost;

    let answer = rig.read(out("short-1", 200)).await.expect("a page");
    assert_eq!(answer, page(ok, 200, 200, b"", false, None));
    let answer = rig.read(out("short-1", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, &bytes[..200], false, Some(1)));
    let answer = rig.read(out("hole-1", 100)).await.expect("a page");
    assert_eq!(answer, page(live, 100, 100, b"", false, None));
    let answer = rig.read(out("hole-2", 100)).await.expect("a page");
    assert_eq!(answer, page(ok, 100, 100, b"", false, None));
    let answer = rig.read(out("lost-1", 100)).await.expect("a page");
    assert_eq!(answer, page(gone, 100, 100, b"", false, None));
    let answer = rig.read(out("lost-1", 0)).await.expect("a page");
    assert_eq!(answer, page(gone, 0, 0, &bytes[..100], false, Some(1)));

    rig.advance(LOG_SETTLE);
    let answer = rig.read(out("short-1", 200)).await.expect("a page");
    assert_eq!(answer, page(ok, 200, 300, b"", true, None));
    let answer = rig.read(out("hole-1", 100)).await.expect("a page");
    assert_eq!(answer, page(live, 100, 200, &bytes[200..], false, Some(3)));
    let answer = rig.read(out("hole-2", 100)).await.expect("a page");
    assert_eq!(answer, page(ok, 100, 150, &bytes[150..200], true, Some(2)));
    let answer = rig.read(out("lost-1", 100)).await.expect("a page");
    assert_eq!(answer, page(gone, 100, 100, b"", true, None));

    let never = queue(rig.jobs(), "never-1", spec(&[]));
    rig.jobs().request_cancel(&never).expect("cancelled");
    let cancelled = v1::JobState::Cancelled;
    let answer = rig.read(out("never-1", 0)).await.expect("a page");
    assert_eq!(answer, page(cancelled, 0, 0, b"", true, None));
    let past = (Code::OutOfRange, "field: fromOffset".to_owned());
    assert_eq!(refusal(rig.read(out("never-1", 1)).await), past);
}

/// Asserts that the reader reads only the asked job's objects, under the prefix of the host
/// its record names: of `job-1` and `job-10`, whose ids share a beginning, each reads its own
/// bytes, and every key and listing prefix asked for `job-1` begins with its own prefix and
/// none with `job-10`'s; that a key under the stream's prefix that is not the layout's is
/// ignored; that the objects an earlier job left under a reused id (written more than the
/// margin before the placement) are ignored, its final too; that ids the layout gives no
/// prefix (`_x`, `-x`, `..`) have no live logs, with no store request; that a `job_id` that is
/// not a job id is refused naming `jobId`; and that a queued job answers an empty page without
/// a store request. Catches a prefix match that leaks a neighbour, a prefix that is not the
/// one the job's start named, and a client's text in a key.
#[tokio::test]
async fn the_object_prefix_comes_from_the_job_id_only() {
    let rig = Rig::new().await;
    let ok = v1::JobState::Succeeded;
    rig.put(
        &prefix("reuse-1").final_key(Stream::Stdout),
        b"the earlier job's",
    );
    rig.chunk("reuse-1", Stream::Stdout, 3, b"stale", 1);
    rig.advance(Duration::from_secs(360));
    running(rig.jobs(), "job-1");
    running(rig.jobs(), "job-10");
    running(rig.jobs(), "reuse-1");
    rig.chunk("job-1", Stream::Stdout, 0, b"one", 1);
    rig.chunk("job-10", Stream::Stdout, 0, b"ten!", 2);
    rig.final_file("job-10", Stream::Stdout, b"ten!");
    let odd = format!("{}stdout/not-an-offset", prefix("job-1").as_str());
    rig.put(&odd, b"ignored");
    rig.chunk("reuse-1", Stream::Stdout, 0, b"new", 4);
    end(rig.jobs(), "job-1", (3, 0), (0, 0));

    let before = rig.asked().len();
    let answer = rig.read(out("job-1", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"one", true, Some(1)));
    let asked = rig.asked().split_off(before);
    assert!(!asked.is_empty());
    for key in &asked {
        assert!(key.starts_with(prefix("job-1").as_str()), "{key}");
        assert!(!key.starts_with(prefix("job-10").as_str()), "{key}");
    }
    let live = v1::JobState::Running;
    let answer = rig.read(out("job-10", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"ten!", false, Some(2)));
    let answer = rig.read(out("reuse-1", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"new", false, Some(4)));
    end(rig.jobs(), "reuse-1", (3, 0), (0, 0));
    let answer = rig.read(out("reuse-1", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"new", true, Some(4)));

    let none = (Code::FailedPrecondition, "no-live-logs".to_owned());
    let before = rig.asked().len();
    for job in ["_x", "-x"] {
        running(rig.jobs(), job);
        assert_eq!(refusal(rig.read(out(job, 0)).await), none, "{job}");
    }
    running(rig.jobs(), "..");
    assert_eq!(refusal(rig.grpc(out("..", 0)).await), none);
    assert_eq!(rig.asked().len(), before);
    let bad = (Code::InvalidArgument, "field: jobId".to_owned());
    assert_eq!(refusal(rig.grpc(out("a/b", 0)).await), bad);

    queue(rig.jobs(), "queued-1", spec(&[]));
    let answer = rig.read(out("queued-1", 5)).await.expect("a page");
    assert_eq!(answer, page(v1::JobState::Queued, 5, 5, b"", false, None));
    assert_eq!(rig.asked().len(), before);
}

/// Asserts the refusals over gRPC and the gateway (its row of each status): an unknown job is
/// `unknown-job`; one past its retention end `expired`, and once swept `unknown-job`; a server
/// with no sink `no-log-sink`; an ended job whose result reports no stream lengths
/// `no-live-logs`; a page size of 0 `INVALID_ARGUMENT` naming `maxBytes`; and an offset past
/// an ended job's stream `OUT_OF_RANGE` naming `fromOffset`. A request without a stream, or
/// with one this version does not know, is the old call and answers the tails. Catches a
/// refusal answered as data or as another code, and the old call changed by the new fields.
#[tokio::test]
async fn unknown_expired_and_unstored_jobs_are_answered_as_the_other_calls_do() {
    let rig = Rig::new().await;
    let unknown = (Code::NotFound, "unknown-job".to_owned());
    assert_eq!(refusal(rig.read(out("nobody-1", 0)).await), unknown);

    let quiet = running(rig.jobs(), "quiet-1");
    let output = Output {
        stdout: "tail".to_owned(),
        ..Output::default()
    };
    let exited = JobEnd::Exited {
        exit_code: 0,
        output,
    };
    end_as(rig.jobs(), &quiet, exited);
    let none = (Code::FailedPrecondition, "no-live-logs".to_owned());
    assert_eq!(refusal(rig.read(out("quiet-1", 0)).await), none);
    let old = GetLogsRequest {
        job_id: "quiet-1".to_owned(),
        ..GetLogsRequest::default()
    };
    let tails = GetLogsResponse {
        state: v1::JobState::Succeeded.into(),
        stdout_tail: b"tail".to_vec(),
        ..GetLogsResponse::default()
    };
    assert_eq!(rig.read(old.clone()).await.expect("the tails"), tails);
    let unnamed = GetLogsRequest { stream: 7, ..old };
    assert_eq!(rig.grpc(unnamed).await.expect("the tails"), tails);

    running(rig.jobs(), "sized-1");
    end(rig.jobs(), "sized-1", (3, 0), (0, 0));
    let zero = (Code::InvalidArgument, "field: maxBytes".to_owned());
    assert_eq!(refusal(rig.read(out_max("sized-1", 0, 0)).await), zero);
    let past = (Code::OutOfRange, "field: fromOffset".to_owned());
    assert_eq!(refusal(rig.read(out("sized-1", 4)).await), past);

    let retention = rig.jobs().retention().window();
    rig.advance(retention);
    let expired = (Code::NotFound, "expired".to_owned());
    assert_eq!(refusal(rig.read(out("sized-1", 0)).await), expired);
    rig.jobs().tick().expect("the sweep runs");
    assert_eq!(refusal(rig.read(out("sized-1", 0)).await), unknown);

    let clock = read_tests::clock();
    let jobs = read_tests::over_fake(&clock);
    let plain = read_tests::Rig::over(jobs, clock).await;
    running(&plain.jobs, "plain-1");
    let request = out("plain-1", 0);
    let uri = super::logs_rig::uri(&request);
    let answer = plain
        .read::<_, GetLogsResponse>("GetLogs", request, &uri)
        .await;
    let no_sink = (Code::FailedPrecondition, "no-log-sink".to_owned());
    assert_eq!(refusal(answer), no_sink);
}

/// Asserts that a store that is down, by a refused connection, a 503 `SlowDown`, a 500, a 403
/// `AccessDenied` or `ExpiredToken` (a refused read key) or a 404 `NoSuchBucket`, at the
/// listing, at a marker's read or at a chunk's read, answers `UNAVAILABLE`
/// `log-store-unavailable` (503 over the gateway) and nothing else; that its log line is
/// exactly the fixed text of its kind, a refused key an error and the rest a warning, and no
/// line holds the store's URL, bucket, key, message or the read key; and that the next call,
/// with the store back, answers the same bytes. Catches a store failure answered as INTERNAL
/// or with the store's words, a secret or a URL in a log line, and a failure that sticks.
#[tokio::test]
async fn a_store_that_is_down_is_unavailable_and_leaks_nothing() {
    let rig = Rig::new().await;
    let live = v1::JobState::Running;
    running(rig.jobs(), "down-1");
    rig.chunk("down-1", Stream::Stdout, 0, b"abc", 1);
    rig.marker("down-1", Stream::Stdout, 3, 2);
    let warned = "WARN the log store could not be read";
    let refused = "ERROR the log store refused the read key: check logs.read_key_file and logs.url";
    let cases = [
        (0, Failure::Refused, warned),
        (0, Failure::Answer(503, "SlowDown"), warned),
        (1, Failure::Answer(500, "InternalError"), warned),
        (2, Failure::Answer(500, "InternalError"), warned),
        (0, Failure::Answer(403, "AccessDenied"), refused),
        (0, Failure::Answer(403, "ExpiredToken"), refused),
        (0, Failure::Answer(404, "NoSuchBucket"), refused),
    ];
    for (passing, failure, line) in cases {
        let lines = Lines::default();
        crate::api::rig::process_sink();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        rig.tap.fail_after(passing, failure);
        let status = rig.grpc(out("down-1", 0)).await.expect_err("refused");
        assert_eq!(
            (status.code(), status.message(), status.details()),
            (Code::Unavailable, "log-store-unavailable", &b""[..]),
            "{failure:?}"
        );
        rig.tap.fail_after(passing, failure);
        let (code, body) = rig
            .calls
            .get(&super::logs_rig::uri(&out("down-1", 0)))
            .await;
        assert_eq!(code.as_u16(), 503, "{failure:?}");
        assert_eq!(body, br#"{"error":"log-store-unavailable"}"#, "{failure:?}");
        drop(guard);
        let logged = lines.take();
        let store_lines: Vec<&str> = logged
            .lines()
            .map(str::trim)
            .filter(|text| text.contains("log store"))
            .collect();
        assert_eq!(store_lines, [line, line], "{failure:?}");
        for secret in [
            "s3://",
            "secret",
            "READKEY",
            "objects.example",
            "js/",
            "logs/",
        ] {
            assert!(!logged.contains(secret), "{failure:?} {secret}");
        }
        let answer = rig.grpc(out("down-1", 0)).await.expect("the store is back");
        assert_eq!(
            answer,
            page(live, 0, 0, b"abc", false, Some(1)),
            "{failure:?}"
        );
    }
}

/// Asserts the page's bounds: no page size gives 262,144 bytes of a 1 MiB chunk; a page size
/// above 1 MiB is cut to 1 MiB (not refused), also across two chunks; and a page of many
/// small chunks reads at most 64 of them, the next page going on from there. Catches an
/// unbounded read, a page cut to the wrong size, and a page of thousands of reads.
#[tokio::test]
async fn a_page_is_bounded() {
    let rig = Rig::new().await;
    let live = v1::JobState::Running;
    let bytes = pattern(2 * MAX_CHUNK_BYTES);
    running(rig.jobs(), "page-1");
    rig.chunk("page-1", Stream::Stdout, 0, &bytes[..MAX_CHUNK_BYTES], 1);
    let second = &bytes[MAX_CHUNK_BYTES..];
    rig.chunk("page-1", Stream::Stdout, at(MAX_CHUNK_BYTES), second, 2);
    let default = usize::try_from(DEFAULT_PAGE_BYTES).unwrap();
    let answer = rig.read(out("page-1", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, &bytes[..default], false, Some(1)));
    let answer = rig
        .read(out_max("page-1", 0, u32::MAX))
        .await
        .expect("a page");
    let one = &bytes[..MAX_CHUNK_BYTES];
    assert_eq!(answer, page(live, 0, 0, one, false, Some(1)));
    let across = &bytes[1000..1000 + MAX_CHUNK_BYTES];
    let answer = rig
        .grpc(out_max("page-1", 1000, 2_000_000))
        .await
        .expect("a page");
    assert_eq!(answer, page(live, 1000, 1000, across, false, Some(1)));

    running(rig.jobs(), "page-2");
    for offset in 0..70 {
        rig.chunk("page-2", Stream::Stdout, offset, &bytes[..1], offset);
    }
    let answer = rig.grpc(out("page-2", 0)).await.expect("a page");
    assert_eq!((answer.data.len(), answer.next_offset), (64, 64));
    let answer = rig.grpc(out("page-2", 64)).await.expect("a page");
    assert_eq!((answer.data.len(), answer.next_offset), (6, 70));
}

/// Asserts the listing's limits: a page that skips more gap markers than one listing returns
/// stops at the last listed one, and the next page lists on from there; an offset that is a
/// chunk's start is found with thousands of chunks before it; and an offset inside a chunk
/// with more than four pages of keys before it in its MiB answers an empty page, never bytes
/// of another chunk. Catches a page that reports a gap past what it listed, and a search for
/// the chunk before an offset that does not end.
#[tokio::test]
async fn a_listing_longer_than_one_page_is_continued_by_the_next_call() {
    let rig = Rig::new().await;
    let live = v1::JobState::Running;
    running(rig.jobs(), "dense-1");
    for offset in 0..1001 {
        rig.marker("dense-1", Stream::Stdout, offset, 1);
    }
    let answer = rig.grpc(out("dense-1", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 1000, b"", false, None));
    let answer = rig.grpc(out("dense-1", 1000)).await.expect("a page");
    assert_eq!(answer, page(live, 1000, 1001, b"", false, None));

    running(rig.jobs(), "dense-2");
    for offset in 0..4001 {
        rig.chunk("dense-2", Stream::Stdout, offset, b"d", 1);
    }
    rig.chunk("dense-2", Stream::Stdout, 4001, b"0123456789", 2);
    let answer = rig.grpc(out("dense-2", 4001)).await.expect("a page");
    assert_eq!(
        answer,
        page(live, 4001, 4001, b"0123456789", false, Some(2))
    );
    let answer = rig.grpc(out("dense-2", 4005)).await.expect("a page");
    assert_eq!(answer, page(live, 4005, 4005, b"", false, None));
}

/// Asserts that a store failing at any one request of a page answers `UNAVAILABLE`
/// `log-store-unavailable`: the final's listing, the final's read, the listing of the chunk
/// before an offset, and the final looked for again at a hole, after a chunk that is gone,
/// and past the last object. Catches a failure at a later step answered as data, as a gap
/// or as the end.
#[tokio::test]
async fn a_store_failing_at_each_step_is_unavailable() {
    let rig = Rig::new().await;
    let bytes = pattern(300);
    running(rig.jobs(), "step-1");
    rig.chunk("step-1", Stream::Stdout, 0, &bytes[..3], 1);
    end(rig.jobs(), "step-1", (3, 0), (0, 0));
    running(rig.jobs(), "step-2");
    end(rig.jobs(), "step-2", (10, 0), (0, 0));
    rig.final_file("step-2", Stream::Stdout, &bytes[..10]);
    running(rig.jobs(), "step-3");
    rig.chunk("step-3", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("step-3", Stream::Stdout, 150, &bytes[150..200], 2);
    end(rig.jobs(), "step-3", (200, 0), (0, 0));
    running(rig.jobs(), "step-4");
    rig.chunk("step-4", Stream::Stdout, 0, &bytes[..3], 1);
    end(rig.jobs(), "step-4", (3, 0), (0, 0));
    running(rig.jobs(), "step-5");
    rig.chunk("step-5", Stream::Stdout, 0, &bytes[..200], 1);
    end(rig.jobs(), "step-5", (300, 0), (0, 0));
    running(rig.jobs(), "step-6");
    rig.chunk("step-6", Stream::Stdout, 0, &bytes[..100], 1);
    rig.advance(LOG_SETTLE);
    let down = (Code::Unavailable, "log-store-unavailable".to_owned());
    for (job, from, passing) in [
        ("step-1", 0, 0),
        ("step-2", 0, 1),
        ("step-3", 100, 3),
        ("step-4", 0, 3),
        ("step-5", 200, 3),
        ("step-6", 100, 1),
    ] {
        if job == "step-4" {
            let writer = rig.writer.clone();
            let key = prefix(job).chunk_key(Stream::Stdout, 0);
            let deleted = key.clone();
            rig.tap
                .before(&key, move || writer.delete(&deleted).expect("deleted"));
        }
        rig.tap.fail_after(passing, Failure::Refused);
        assert_eq!(refusal(rig.grpc(out(job, from)).await), down, "{job}");
    }
}

/// Asserts the cases of the walk over a page's objects: a chunk that does not begin where
/// the data ends stops the page; a chunk read shorter than listed (rewritten meanwhile) stops
/// the page after what was read; a marker that reaches an ended job's length ends the
/// stream; a hole before a fresh chunk of a job that ended long enough ago is a gap; a
/// chunk gone after the final was asked for twice is a hole up to the end, not a third
/// request for the final; and a chunk rewritten so short that the read starts past its end
/// (the store answers 416) is a hole, not an error. Catches a page that reads past a hole, a short read taken as the
/// whole chunk, and an end missed after a gap.
#[tokio::test]
async fn the_walk_stops_at_every_break_in_the_bytes() {
    let rig = Rig::new().await;
    let live = v1::JobState::Running;
    let ok = v1::JobState::Succeeded;
    let bytes = pattern(300);
    running(rig.jobs(), "walk-1");
    rig.chunk("walk-1", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("walk-1", Stream::Stdout, 150, &bytes[150..200], 2);
    let answer = rig.read(out("walk-1", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, &bytes[..100], false, Some(1)));

    running(rig.jobs(), "walk-2");
    rig.chunk("walk-2", Stream::Stdout, 0, &bytes[..100], 1);
    rig.chunk("walk-2", Stream::Stdout, 100, &bytes[100..150], 2);
    let writer = rig.writer.clone();
    let key = prefix("walk-2").chunk_key(Stream::Stdout, 0);
    let shorter = key.clone();
    let head = bytes[..60].to_vec();
    rig.tap.before(&key, move || {
        writer
            .put(&shorter, head, &Default::default())
            .expect("put");
    });
    let answer = rig.grpc(out("walk-2", 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, &bytes[..60], false, None));

    running(rig.jobs(), "walk-3");
    rig.chunk("walk-3", Stream::Stdout, 0, &bytes[..100], 1);
    rig.marker("walk-3", Stream::Stdout, 100, 100);
    end(rig.jobs(), "walk-3", (200, 0), (0, 0));
    let answer = rig.read(out("walk-3", 100)).await.expect("a page");
    assert_eq!(answer, page(ok, 100, 200, b"", true, None));

    running(rig.jobs(), "walk-4");
    rig.chunk("walk-4", Stream::Stdout, 0, &bytes[..100], 1);
    end(rig.jobs(), "walk-4", (200, 0), (0, 0));
    running(rig.jobs(), "walk-5");
    rig.chunk("walk-5", Stream::Stdout, 10, &bytes[10..20], 1);
    end(rig.jobs(), "walk-5", (20, 0), (0, 0));
    rig.advance(LOG_SETTLE);
    rig.chunk("walk-4", Stream::Stdout, 150, &bytes[150..200], 2);
    let answer = rig.read(out("walk-4", 100)).await.expect("a page");
    assert_eq!(answer, page(ok, 100, 150, &bytes[150..200], true, Some(2)));
    let writer = rig.writer.clone();
    let key = prefix("walk-5").chunk_key(Stream::Stdout, 10);
    let deleted = key.clone();
    rig.tap
        .before(&key, move || writer.delete(&deleted).expect("deleted"));
    let answer = rig.grpc(out("walk-5", 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 20, b"", true, None));

    running(rig.jobs(), "walk-6");
    rig.chunk("walk-6", Stream::Stdout, 0, &bytes[..100], 1);
    let writer = rig.writer.clone();
    let key = prefix("walk-6").chunk_key(Stream::Stdout, 0);
    let shorter = key.clone();
    let head = bytes[..10].to_vec();
    rig.tap.before(&key, move || {
        writer
            .put(&shorter, head, &Default::default())
            .expect("put");
    });
    let answer = rig.grpc(out("walk-6", 50)).await.expect("a page");
    assert_eq!(answer, page(live, 50, 50, b"", false, None));
}
