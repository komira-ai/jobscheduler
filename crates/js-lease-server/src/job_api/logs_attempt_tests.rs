//! `GetLogs` of a job's attempts (`logs.rs`, # Attempts) over both transports: the attempt
//! asked for, absent and each one; an attempt that ended while the job waits for its next one
//! or ended in that wait; an attempt the record's list dropped, read through its row; the
//! refusals of `attempt`; and the reader's side of the length rule of an attempt's prefix.
//!
//! The rig's reader learns each job's attempts from [`attempts`] and reads rows with [`rows`],
//! keyed by the job's id, so a job of several attempts (up to 300) is set up without running
//! each one; the server reads them from the record's trail (`Attempts::of`), which
//! `callback_tests.rs` follows through a real retry. Every other job is read as the server
//! reads it. Nothing sleeps: the settle window passes
//! on the manual clock.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use js_jobs::{AttemptRow, JobEnd, JobId, JobRecord, JobStoreError};
use js_logmint::LogPrefix;
use js_logstore::{JobPrefix, Stream};
use js_proto::v1::{self, GetLogsRequest};
use js_state::{StoreError, StoreTime};
use js_types::HostId;
use tonic::Code;

use super::logs::page::LOG_SETTLE;
use super::logs::stored_row;
use super::logs_rig::{PREFIX, Rig, ask, end, end_as};
use super::logs_tests::{page, refusal};
use super::read_tests::{START_MS, queue, running, spec};
use crate::job_store::ServerJobs;
use crate::logs::attempt::{Attempts, attempt_prefix};

/// The current attempt of `roll-1`, which its one test moves on while a follow waits; no other
/// test reads it.
static ROLL: AtomicU32 = AtomicU32::new(1);

/// The host of the earlier attempts that did not run on `host-a`.
const HOST_B: &str = "host-b";

/// An instant on the store's clock, `ms` after its origin.
fn at(ms: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(ms))
}

/// An ended attempt on `host`, placed and ended when the rig's clock starts.
fn ran_on(host: &str) -> AttemptRow {
    AttemptRow {
        host: HostId::new(host).expect("a host id"),
        placed_at: at(START_MS),
        ended_at: at(START_MS),
    }
}

/// The attempts of the rig's jobs, by id:
/// - `three-1`: on attempt 3; attempt 1 ran on `host-b`, attempt 2 on `host-a`;
/// - `done-1`: ended on attempt 2, which the record also lists; attempt 1 ran on `host-b`;
/// - `wait-1`, `cancel-1`: attempt 1 ran on `host-a` and ended, and the job waits for its next
///   attempt (`wait-1`) or was cancelled while it waited (`cancel-1`);
/// - `many-1`, `gone-1`, `down-1`: on attempt 300; the record lists the newest 256 (45 to
///   300), on `host-a`;
/// - `long-1`: on attempt 2; attempt 1 ran on `host-a`;
/// - `long-3`: on attempt 3; attempts 1 and 2 ran on `host-a`;
/// - `roll-1`: on the attempt [`ROLL`] holds, the ones before it ended on `host-a`;
/// - every other job as the server learns it.
fn attempts(job: &JobRecord) -> Attempts {
    let listed = |rows: &[(u32, &str)]| -> BTreeMap<u32, AttemptRow> {
        rows.iter()
            .map(|(number, host)| (*number, ran_on(host)))
            .collect()
    };
    match job.id.as_str() {
        "three-1" => Attempts {
            current: 3,
            ended: listed(&[(1, HOST_B), (2, "host-a")]),
        },
        "done-1" => Attempts {
            current: 2,
            ended: listed(&[(1, HOST_B), (2, "host-a")]),
        },
        "wait-1" | "cancel-1" => Attempts {
            current: 1,
            ended: listed(&[(1, "host-a")]),
        },
        "many-1" | "gone-1" | "down-1" => Attempts {
            current: 300,
            ended: (45..=300)
                .map(|number| (number, ran_on("host-a")))
                .collect(),
        },
        "long-1" => Attempts {
            current: 2,
            ended: listed(&[(1, "host-a")]),
        },
        "long-3" => Attempts {
            current: 3,
            ended: listed(&[(1, "host-a"), (2, "host-a")]),
        },
        "roll-1" => {
            let current = ROLL.load(Ordering::SeqCst);
            Attempts {
                current,
                ended: (1..current).map(|n| (n, ran_on("host-a"))).collect(),
            }
        }
        _ => Attempts::of(job),
    }
}

/// The rows of the rig's jobs: attempt 1 of `many-1` ran on `host-b`, and has a row; a read
/// of `gone-1`'s rows finds the job gone (swept meanwhile) and one of `down-1`'s fails; every
/// other row is read from the job tables.
fn rows(jobs: &ServerJobs, id: &JobId, attempt: u32) -> Result<Option<AttemptRow>, JobStoreError> {
    match (id.as_str(), attempt) {
        ("many-1", 1) => Ok(Some(ran_on(HOST_B))),
        ("gone-1", _) => Err(JobStoreError::NotFound { id: id.clone() }),
        ("down-1", _) => Err(JobStoreError::Store(StoreError::Unavailable(
            "busy".to_owned(),
        ))),
        _ => stored_row(jobs, id, attempt),
    }
}

/// The rig under the log prefix `prefix`, its reader learning attempts from [`attempts`].
async fn rig_under(prefix: &str) -> Rig {
    Rig::attempted(prefix, attempts, rows).await
}

/// The prefix of attempt `attempt` of `job` on `host`, under the log prefix `prefix`.
fn under(prefix: &str, host: &str, job: &str, attempt: u32) -> JobPrefix {
    let logs = LogPrefix::new(prefix).expect("a log prefix");
    attempt_prefix(&logs, host, job, attempt).expect("an attempt prefix")
}

/// The request for stdout of attempt `attempt` (none: absent) of `job` from `from`.
fn out(job: &str, attempt: Option<u32>, from: u64) -> GetLogsRequest {
    GetLogsRequest {
        attempt,
        ..ask(job, v1::LogStream::Stdout, from)
    }
}

/// Asserts that a running job on attempt 3 whose attempts ran on two hosts reads, over both
/// transports, attempt 3's bytes when the request names no attempt or names 3, and each
/// earlier attempt's own bytes, under its own host, when it names it: those two at their end
/// (an ended attempt has no reported length, so the end is where its stored bytes end once the
/// window passed), while the job's state is still running; that a follow of an ended attempt
/// answers at once; that a job that ended on its attempt 2 reads attempt 2 by its result's
/// length (an offset past it is out of range) and from its own final, not attempt 1's
/// prefix, and attempt 1 by its stored bytes; that attempt 0 is an
/// invalid argument naming `attempt` and one above the current attempt is out of range naming
/// `attempt`; and that a job of one attempt reads it, absent or named, and refuses attempt 2.
/// Catches an `attempt` ignored (every attempt reading the latest), one attempt reading
/// another's bytes or host, a follow waiting on an attempt that will write no more, and an
/// attempt the job never had read as empty.
#[tokio::test]
async fn get_logs_reads_the_attempt_asked_for() {
    let rig = rig_under(PREFIX).await;
    let live = v1::JobState::Running;
    running(rig.jobs(), "three-1");
    rig.chunk_under(
        &under(PREFIX, HOST_B, "three-1", 1),
        Stream::Stdout,
        0,
        b"one\n",
        1,
    );
    rig.chunk_under(
        &under(PREFIX, "host-a", "three-1", 2),
        Stream::Stdout,
        0,
        b"two\n",
        2,
    );
    rig.chunk_under(
        &under(PREFIX, "host-a", "three-1", 3),
        Stream::Stdout,
        0,
        b"three\n",
        3,
    );
    rig.advance(LOG_SETTLE);
    for attempt in [None, Some(3)] {
        let answer = rig.read(out("three-1", attempt, 0)).await.expect("a page");
        assert_eq!(answer, page(live, 0, 0, b"three\n", false, Some(3)));
    }
    let answer = rig.read(out("three-1", Some(2), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"two\n", false, Some(2)));
    let answer = rig.read(out("three-1", Some(2), 4)).await.expect("a page");
    assert_eq!(answer, page(live, 4, 4, b"", true, None));
    let answer = rig.read(out("three-1", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"one\n", false, Some(1)));
    let followed = GetLogsRequest {
        follow: Some(true),
        ..out("three-1", Some(1), 4)
    };
    let answer = rig.read(followed).await.expect("a page");
    assert_eq!(answer, page(live, 4, 4, b"", true, None));
    assert_eq!(rig.steps.begun(), 0);

    running(rig.jobs(), "done-1");
    rig.chunk_under(
        &under(PREFIX, HOST_B, "done-1", 1),
        Stream::Stdout,
        0,
        b"first",
        5,
    );
    rig.final_file("done-1", Stream::Stdout, b"wrong");
    rig.final_under(&under(PREFIX, "host-a", "done-1", 2), b"second!!");
    end(rig.jobs(), "done-1", (8, 0), (0, 0));
    let ok = v1::JobState::Succeeded;
    for attempt in [None, Some(2)] {
        let answer = rig.read(out("done-1", attempt, 0)).await.expect("a page");
        assert_eq!(answer, page(ok, 0, 0, b"second!!", true, None));
    }
    let past = (Code::OutOfRange, "field: fromOffset".to_owned());
    assert_eq!(refusal(rig.read(out("done-1", Some(2), 9)).await), past);
    let answer = rig.read(out("done-1", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(ok, 0, 0, b"first", false, Some(5)));
    let answer = rig.read(out("done-1", Some(1), 5)).await.expect("a page");
    assert_eq!(answer, page(ok, 5, 5, b"", true, None));

    let zero = (Code::InvalidArgument, "field: attempt".to_owned());
    let above = (Code::OutOfRange, "field: attempt".to_owned());
    assert_eq!(refusal(rig.read(out("three-1", Some(0), 0)).await), zero);
    assert_eq!(refusal(rig.read(out("three-1", Some(4), 0)).await), above);
    assert_eq!(refusal(rig.read(out("done-1", Some(3), 0)).await), above);

    running(rig.jobs(), "one-1");
    rig.chunk("one-1", Stream::Stdout, 0, b"only\n", 6);
    for attempt in [None, Some(1)] {
        let answer = rig.read(out("one-1", attempt, 0)).await.expect("a page");
        assert_eq!(answer, page(live, 0, 0, b"only\n", false, Some(6)));
    }
    assert_eq!(refusal(rig.read(out("one-1", Some(0), 0)).await), zero);
    assert_eq!(refusal(rig.read(out("one-1", Some(2), 0)).await), above);
}

/// Asserts that a job whose attempt 1 ended and that waits, queued, for its next attempt
/// reads attempt 1's bytes, by number and when the request names none, and at their end once
/// the window passed, where a queued job of one attempt answers an empty page; that a follow
/// of it answers at once; and that a job cancelled while it waited reads attempt 1 the same
/// way. Catches the queued job's empty answer applied to an attempt that ran, and the logs of
/// a job cancelled in its backoff lost.
#[tokio::test]
async fn an_earlier_attempts_logs_read_during_backoff() {
    let rig = rig_under(PREFIX).await;
    let queued = v1::JobState::Queued;
    queue(rig.jobs(), "wait-1", spec(&[]));
    rig.chunk("wait-1", Stream::Stdout, 0, b"tried\n", 7);
    let cancel = queue(rig.jobs(), "cancel-1", spec(&[]));
    rig.chunk("cancel-1", Stream::Stdout, 0, b"tried too\n", 8);
    rig.jobs().request_cancel(&cancel).expect("cancelled");
    queue(rig.jobs(), "first-1", spec(&[]));
    rig.chunk("first-1", Stream::Stdout, 0, b"not this job's\n", 9);

    for attempt in [None, Some(1)] {
        let answer = rig.read(out("wait-1", attempt, 0)).await.expect("a page");
        assert_eq!(answer, page(queued, 0, 0, b"tried\n", false, Some(7)));
    }
    let answer = rig.read(out("first-1", None, 0)).await.expect("a page");
    assert_eq!(answer, page(queued, 0, 0, b"", false, None));
    rig.advance(LOG_SETTLE);
    let followed = GetLogsRequest {
        follow: Some(true),
        ..out("wait-1", None, 6)
    };
    let answer = rig.read(followed).await.expect("a page");
    assert_eq!(answer, page(queued, 6, 6, b"", true, None));
    assert_eq!(rig.steps.begun(), 0);

    let cancelled = v1::JobState::Cancelled;
    let answer = rig.read(out("cancel-1", None, 0)).await.expect("a page");
    assert_eq!(
        answer,
        page(cancelled, 0, 0, b"tried too\n", false, Some(8))
    );
    let answer = rig
        .read(out("cancel-1", Some(1), 10))
        .await
        .expect("a page");
    assert_eq!(answer, page(cancelled, 10, 10, b"", true, None));
}

/// Asserts that attempt 1 of a job on attempt 300, which the record's list of the newest 256
/// no longer holds, reads its own bytes under the host its row names, not those of the host
/// the listed attempts ran on; that attempt 45, the oldest listed, reads from the list; that
/// attempt 44, dropped from the list and with no row (the job tables hold none), has no live
/// logs; and that a row read that finds the job gone is `unknown-job` and one that fails is
/// `store-unavailable`. Catches a dropped attempt read under the wrong host or as empty, and a
/// failed row read answered as logs.
#[tokio::test]
async fn a_dropped_attempts_logs_read_through_its_row() {
    let rig = rig_under(PREFIX).await;
    let live = v1::JobState::Running;
    running(rig.jobs(), "many-1");
    rig.chunk_under(
        &under(PREFIX, HOST_B, "many-1", 1),
        Stream::Stdout,
        0,
        b"the first\n",
        10,
    );
    rig.chunk("many-1", Stream::Stdout, 0, b"not attempt 1 here\n", 11);
    rig.chunk_under(
        &under(PREFIX, "host-a", "many-1", 45),
        Stream::Stdout,
        0,
        b"forty-five\n",
        12,
    );
    let answer = rig.read(out("many-1", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"the first\n", false, Some(10)));
    let answer = rig.read(out("many-1", Some(45), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"forty-five\n", false, Some(12)));
    let none = (Code::FailedPrecondition, "no-live-logs".to_owned());
    assert_eq!(refusal(rig.read(out("many-1", Some(44), 0)).await), none);

    running(rig.jobs(), "gone-1");
    running(rig.jobs(), "down-1");
    let unknown = (Code::NotFound, "unknown-job".to_owned());
    assert_eq!(refusal(rig.read(out("gone-1", Some(1), 0)).await), unknown);
    let busy = (Code::Unavailable, "store-unavailable".to_owned());
    assert_eq!(refusal(rig.read(out("down-1", Some(1), 0)).await), busy);
}

/// Asserts, under a configured prefix of 412 bytes, that attempt 2 of a job, whose start got
/// no sink, has no live logs, as current attempt, after the job was lost, and as an earlier
/// attempt of a job on attempt 3, while attempt 1 reads; and under one of 411 bytes that both
/// read. Catches a reader that builds a prefix the start never named, and one that refuses an
/// attempt whose prefix fits.
#[tokio::test]
async fn an_attempt_prefix_that_does_not_fit_has_no_live_logs_to_read() {
    let none = (Code::FailedPrecondition, "no-live-logs".to_owned());
    let live = v1::JobState::Running;
    let long = "p".repeat(412);
    let rig = rig_under(&long).await;
    let lost = running(rig.jobs(), "long-1");
    rig.chunk_under(
        &under(&long, "host-a", "long-1", 1),
        Stream::Stdout,
        0,
        b"fits\n",
        13,
    );
    assert_eq!(refusal(rig.read(out("long-1", None, 0)).await), none);
    let answer = rig.read(out("long-1", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"fits\n", false, Some(13)));
    end_as(rig.jobs(), &lost, JobEnd::Lost);
    assert_eq!(refusal(rig.read(out("long-1", Some(2), 0)).await), none);
    running(rig.jobs(), "long-3");
    rig.chunk_under(
        &under(&long, "host-a", "long-3", 1),
        Stream::Stdout,
        0,
        b"fits too\n",
        16,
    );
    assert_eq!(refusal(rig.read(out("long-3", Some(2), 0)).await), none);
    let answer = rig.read(out("long-3", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"fits too\n", false, Some(16)));

    let fits = "p".repeat(411);
    let rig = rig_under(&fits).await;
    running(rig.jobs(), "long-1");
    rig.chunk_under(
        &under(&fits, "host-a", "long-1", 1),
        Stream::Stdout,
        0,
        b"first\n",
        14,
    );
    rig.chunk_under(
        &under(&fits, "host-a", "long-1", 2),
        Stream::Stdout,
        0,
        b"second\n",
        15,
    );
    let answer = rig.read(out("long-1", None, 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"second\n", false, Some(15)));
    let answer = rig.read(out("long-1", Some(1), 0)).await.expect("a page");
    assert_eq!(answer, page(live, 0, 0, b"first\n", false, Some(14)));
}

/// Asserts that a follow of a running job's current attempt, which waits at the end of its
/// bytes, keeps reading that attempt when the next attempt begins during the wait: it answers
/// at once, after one wait, with the attempt's end and none of the next attempt's bytes.
/// Catches a follow that switches to the new current attempt and answers its bytes at the
/// first attempt's offset, and one that waits on an attempt that will write no more.
#[tokio::test]
async fn a_follow_keeps_its_attempt_when_the_next_one_begins() {
    let rig = Arc::new(rig_under(PREFIX).await);
    let live = v1::JobState::Running;
    running(rig.jobs(), "roll-1");
    rig.chunk("roll-1", Stream::Stdout, 0, b"abc", 17);
    let calling = Arc::clone(&rig);
    let followed = GetLogsRequest {
        follow: Some(true),
        ..out("roll-1", None, 3)
    };
    let waiting = tokio::spawn(async move { calling.grpc(followed).await });
    rig.steps.until_waiting(1).await;
    rig.chunk_under(
        &under(PREFIX, "host-a", "roll-1", 2),
        Stream::Stdout,
        0,
        b"next attempt",
        18,
    );
    ROLL.store(2, Ordering::SeqCst);
    rig.advance(LOG_SETTLE);
    rig.steps.release(1);
    let answer = waiting.await.expect("the call ends").expect("a page");
    assert_eq!(answer, page(live, 3, 3, b"", true, None));
    assert_eq!(rig.steps.begun(), 1);
}
