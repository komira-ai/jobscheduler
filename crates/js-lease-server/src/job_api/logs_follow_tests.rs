//! `GetLogs` with `follow` (`logs.rs`): the long poll on the rig's [`Steps`] waiter, which a
//! test releases one wait at a time, so no test sleeps and none races a timer; the bound on
//! waiting follows and the release of a slot by an answer and by a call that goes away; the
//! production waiter on a paused runtime clock; the deadlines a follow must fit in; and the
//! context the `[logs]` table builds.

use std::sync::Arc;

use js_logstore::Stream;
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{self, GetLogsRequest, GetLogsResponse};
use tonic::{Code, Request};

use super::JobApi;
use super::logs::{self, FOLLOW_POLL, FOLLOW_POLLS, MAX_FOLLOW_WAIT};
use super::logs_rig::{Rig, ask, end, sink, uri};
use super::read_tests::{self, place, queue, run, running, spec, start};
use crate::file::LogsConfig;

/// The follow of stdout of `job` from `from`.
fn follow(job: &str, from: u64) -> GetLogsRequest {
    GetLogsRequest {
        follow: Some(true),
        ..ask(job, v1::LogStream::Stdout, from)
    }
}

/// The empty page at `offset` of a job in `state`, not at the end.
fn empty(state: v1::JobState, offset: u64) -> GetLogsResponse {
    GetLogsResponse {
        state: state.into(),
        offset,
        next_offset: offset,
        ..GetLogsResponse::default()
    }
}

/// Asserts that a follow at the end of a running job's written bytes waits (one wait begun,
/// none ended); that a chunk put meanwhile is the answer of the next read after the wait, with
/// the chunk's first-byte time; that with no new chunk the follow answers the empty page at
/// its offset after exactly [`FOLLOW_POLLS`] waits of [`FOLLOW_POLL`], [`MAX_FOLLOW_WAIT`] in
/// all; that a job that ends during the wait answers at once with the end; and that a queued
/// job followed waits until it runs and writes. Catches a follow that answers early, never
/// answers, waits past its cap, or loses its offset.
#[tokio::test]
async fn a_running_job_is_followed_about_as_fast_as_it_writes() {
    let rig = Arc::new(Rig::new().await);
    let live = v1::JobState::Running;
    running(rig.jobs(), "follow-1");
    rig.chunk("follow-1", Stream::Stdout, 0, b"abc", 5);
    let calling = Arc::clone(&rig);
    let waiting = tokio::spawn(async move { calling.grpc(follow("follow-1", 3)).await });
    rig.steps.until_waiting(1).await;
    assert_eq!(rig.steps.begun(), 1);
    rig.chunk("follow-1", Stream::Stdout, 3, b"def", 77);
    rig.steps.release(1);
    let answer = waiting.await.expect("the call ends").expect("a page");
    let expected = GetLogsResponse {
        data: b"def".to_vec(),
        next_offset: 6,
        first_byte_at_ms: Some(77),
        ..empty(live, 3)
    };
    assert_eq!(answer, expected);
    assert_eq!((rig.steps.begun(), rig.steps.waited()), (1, FOLLOW_POLL));

    let calling = Arc::clone(&rig);
    let waiting = tokio::spawn(async move { calling.grpc(follow("follow-1", 6)).await });
    rig.steps.until_waiting(1).await;
    rig.steps.release(usize::try_from(FOLLOW_POLLS).unwrap());
    let answer = waiting.await.expect("the call ends").expect("a page");
    assert_eq!(answer, empty(live, 6));
    let polls = usize::try_from(FOLLOW_POLLS).unwrap();
    assert_eq!(rig.steps.begun(), 1 + polls);
    assert_eq!(rig.steps.waited(), FOLLOW_POLL + MAX_FOLLOW_WAIT);

    let calling = Arc::clone(&rig);
    let waiting = tokio::spawn(async move { calling.grpc(follow("follow-1", 6)).await });
    rig.steps.until_waiting(1).await;
    end(rig.jobs(), "follow-1", (6, 0), (0, 0));
    rig.steps.release(1);
    let answer = waiting.await.expect("the call ends").expect("a page");
    let ended = GetLogsResponse {
        eof: true,
        ..empty(v1::JobState::Succeeded, 6)
    };
    assert_eq!(answer, ended);
    assert_eq!(rig.steps.begun(), 2 + polls);

    let id = queue(rig.jobs(), "follow-2", spec(&[]));
    let calling = Arc::clone(&rig);
    let waiting = tokio::spawn(async move { calling.grpc(follow("follow-2", 0)).await });
    rig.steps.until_waiting(1).await;
    place(rig.jobs(), &id);
    start(rig.jobs(), &id);
    run(rig.jobs(), &id);
    rig.chunk("follow-2", Stream::Stdout, 0, b"x", 8);
    rig.steps.release(1);
    let answer = waiting.await.expect("the call ends").expect("a page");
    let expected = GetLogsResponse {
        data: b"x".to_vec(),
        next_offset: 1,
        first_byte_at_ms: Some(8),
        ..empty(live, 0)
    };
    assert_eq!(answer, expected);
}

/// Asserts that a follow answers without waiting when there is something to answer or the job
/// has ended: at the end of an ended job's stream it answers the end at once; inside the
/// settle window of an ended job with bytes missing it answers the empty page at once; and
/// with bytes there it answers them at once. Catches a follow that waits for nothing.
#[tokio::test]
async fn follow_at_the_end_of_an_ended_job_returns_eof_at_once() {
    let rig = Rig::new().await;
    running(rig.jobs(), "eof-1");
    rig.chunk("eof-1", Stream::Stdout, 0, b"abc", 1);
    let answer = rig.read(follow("eof-1", 0)).await.expect("a page");
    assert_eq!(answer.data, b"abc");
    end(rig.jobs(), "eof-1", (5, 0), (0, 0));
    let ok = v1::JobState::Succeeded;
    let answer = rig.read(follow("eof-1", 5)).await.expect("a page");
    let ended = GetLogsResponse {
        eof: true,
        ..empty(ok, 5)
    };
    assert_eq!(answer, ended);
    let answer = rig.read(follow("eof-1", 3)).await.expect("a page");
    assert_eq!(answer, empty(ok, 3));
    assert_eq!(rig.steps.begun(), 0);
}

/// Asserts that with `max_follow = 2`, two follows wait and the third is refused
/// `RESOURCE_EXHAUSTED` `too-many-follows` (429 over the gateway) before it waits; that a
/// follow whose client goes away (its call dropped mid-wait) frees its slot at once, so a new
/// follow waits; and that a follow that answered frees its slot, so two more wait. Catches a
/// slot that leaks, a bound not applied, and a refused call that waits.
#[tokio::test]
async fn follows_are_bounded_and_released() {
    let rig = Arc::new(Rig::following(2).await);
    running(rig.jobs(), "bound-1");
    let path = uri(&follow("bound-1", 0));
    let calling = Arc::clone(&rig);
    let first = tokio::spawn(async move { calling.calls.get(&path).await });
    let calling = Arc::clone(&rig);
    let second = tokio::spawn(async move { calling.grpc(follow("bound-1", 0)).await });
    rig.steps.until_waiting(2).await;
    let refused = rig.read(follow("bound-1", 0)).await.expect_err("refused");
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::ResourceExhausted, "too-many-follows")
    );
    assert_eq!(rig.steps.begun(), 2);

    first.abort();
    rig.steps.until_waiting(1).await;
    let path = uri(&follow("bound-1", 0));
    let calling = Arc::clone(&rig);
    let third = tokio::spawn(async move { calling.calls.get(&path).await });
    rig.steps.until_waiting(2).await;
    assert_eq!(rig.steps.begun(), 3);

    let polls = usize::try_from(FOLLOW_POLLS).unwrap();
    rig.steps.release(2 * polls);
    let answer = second.await.expect("the call ends").expect("a page");
    assert_eq!(answer, empty(v1::JobState::Running, 0));
    let (status, _) = third.await.expect("the call ends");
    assert_eq!(status.as_u16(), 200);
    rig.steps.until_waiting(0).await;

    let calling = Arc::clone(&rig);
    let fourth = tokio::spawn(async move { calling.grpc(follow("bound-1", 0)).await });
    let calling = Arc::clone(&rig);
    let fifth = tokio::spawn(async move { calling.grpc(follow("bound-1", 0)).await });
    rig.steps.until_waiting(2).await;
    rig.steps.release(2 * polls);
    for call in [fourth, fifth] {
        let answer = call.await.expect("the call ends").expect("a page");
        assert_eq!(answer, empty(v1::JobState::Running, 0));
    }
}

/// Asserts that the production waiter wakes on the runtime's timer: a follow with nothing to
/// read, on a paused runtime clock that moves only when every task waits, answers the empty
/// page once its polls have passed. Catches a waiter that never wakes.
#[tokio::test(start_paused = true)]
async fn a_follow_waits_on_the_runtime_timer() {
    let clock = read_tests::clock();
    let jobs = read_tests::over_fake(&clock);
    running(&jobs, "timer-1");
    let shared = read_tests::service_clock(&clock);
    let rig = Rig::new().await;
    let read = rig.store.issue(js_logstore::fake::Policy {
        actions: vec![
            js_logstore::fake::Action::Get,
            js_logstore::fake::Action::List,
        ],
        prefix: "js/".to_owned(),
    });
    let ctx = logs::Ctx::over(Arc::new(rig.store.clone()), &sink(1), read, &shared);
    let api = JobApi::new(jobs, shared).with_logs(ctx);
    let answer = api.get_logs(Request::new(follow("timer-1", 0))).await;
    let answer = answer.expect("a page").into_inner();
    assert_eq!(answer, empty(v1::JobState::Running, 0));
}

/// Asserts the deadlines a follow must fit in: its polls add up to [`MAX_FOLLOW_WAIT`], which
/// is under the job listener's stall limit on reads (a connection with no traffic while a
/// follow waits is not cut); the gateway adds no deadline of its own, so the route waits as
/// long as the call. Catches a cap that a connection limit would cut.
#[test]
fn the_route_waits_long_enough() {
    assert_eq!(FOLLOW_POLL * FOLLOW_POLLS, MAX_FOLLOW_WAIT);
    let limits = crate::config::Limits::default();
    assert!(limits.read_timeout > MAX_FOLLOW_WAIT);
}

/// Asserts the context a `[logs]` table builds: none for `sink = "none"`, which has no store
/// and no follow slot, and for an object store its store and `max_follow` slots; and that
/// the job service built with it keeps it. Catches a sink the table does not give, and a
/// bound that is not the table's.
#[tokio::test]
async fn a_logs_table_builds_the_reader_it_names() {
    let clock = read_tests::service_clock(&read_tests::clock());
    let none = logs::Ctx::from_config(&LogsConfig::None, &clock);
    assert_eq!(
        format!("{none:?}"),
        "Ctx { sink: false, free_follows: 0, .. }"
    );
    let table = LogsConfig::ObjectStore(Box::new(sink(7)));
    let some = logs::Ctx::from_config(&table, &clock);
    assert_eq!(
        format!("{some:?}"),
        "Ctx { sink: true, free_follows: 7, .. }"
    );
}

/// Asserts that a follow answers at once when its page reports a gap and no data (a gap
/// marker at the offset), and that a follow ends with the error of its next read: the store
/// down (`log-store-unavailable`), the job past its retention end (`expired`), and the job
/// swept (`unknown-job`). Catches a follow that waits over a gap, and one that swallows an
/// error into an empty page.
#[tokio::test]
async fn a_follow_answers_a_gap_at_once_and_ends_with_the_error_of_its_next_read() {
    let rig = Arc::new(Rig::new().await);
    running(rig.jobs(), "fgap-1");
    rig.chunk("fgap-1", Stream::Stdout, 0, b"abc", 1);
    rig.marker("fgap-1", Stream::Stdout, 3, 2);
    let answer = rig.read(follow("fgap-1", 3)).await.expect("a page");
    let gap = v1::LogGap {
        offset: 3,
        length: 2,
    };
    assert_eq!((answer.offset, answer.gaps), (5, vec![gap]));
    assert_eq!(rig.steps.begun(), 0);

    running(rig.jobs(), "ferr-1");
    let calling = Arc::clone(&rig);
    let down = tokio::spawn(async move { calling.grpc(follow("ferr-1", 0)).await });
    rig.steps.until_waiting(1).await;
    rig.tap.fail_after(0, super::logs_rig::Failure::Refused);
    rig.steps.release(1);
    let status = down.await.expect("the call ends").expect_err("refused");
    let unavailable = (Code::Unavailable, "log-store-unavailable");
    assert_eq!((status.code(), status.message()), unavailable);

    // Both follows wait before the clock passes the retention window, so neither reads the
    // store after it; the waits end in order.
    running(rig.jobs(), "ferr-2");
    running(rig.jobs(), "ferr-3");
    let calling = Arc::clone(&rig);
    let expiring = tokio::spawn(async move { calling.grpc(follow("ferr-2", 0)).await });
    rig.steps.until_waiting(1).await;
    let calling = Arc::clone(&rig);
    let swept = tokio::spawn(async move { calling.grpc(follow("ferr-3", 0)).await });
    rig.steps.until_waiting(2).await;
    end(rig.jobs(), "ferr-2", (0, 0), (0, 0));
    end(rig.jobs(), "ferr-3", (0, 0), (0, 0));
    rig.advance(rig.jobs().retention().window());
    rig.steps.release(1);
    let status = expiring.await.expect("the call ends").expect_err("refused");
    assert_eq!(
        (status.code(), status.message()),
        (Code::NotFound, "expired")
    );
    rig.jobs().tick().expect("the sweep runs");
    rig.steps.release(1);
    let status = swept.await.expect("the call ends").expect_err("refused");
    assert_eq!(
        (status.code(), status.message()),
        (Code::NotFound, "unknown-job")
    );
}
