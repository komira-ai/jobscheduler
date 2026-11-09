//! Unit tests of how the writer forms groups. They count groups, syncs and window waits,
//! never time. A request is queued through [`Shared::queue`], the path every call on the
//! store takes, and the writer is run on the test's own thread after the requests are
//! queued and the queue is closed, so which requests were queued when the writer took each
//! one is fixed by the test and not by a race.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use js_testkit::dir::TestDir;
use rusqlite::Connection;

use super::writer::{Reply, Request, Shared, Writer, uint};
use super::{Options, SqliteStore, schema};
use crate::{
    Clock, Commit, Cond, Revision, Store, StoreError, SystemClock, TARGET_WRITES_PER_SECOND, Txn,
};

/// A window no test here may wait out: a group that waited it would not end the test.
const NEVER: Duration = Duration::from_secs(3600);

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::default())
}

fn unit_dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-state-group-{name}"))
}

fn put(key: &str) -> Txn {
    Txn::new().put(key, "x", Cond::Any)
}

/// A writer on the state file in `dir`, which it creates when it does not exist, sharing
/// its counters through `shared`.
fn writer(dir: &Path, options: Options, shared: &Arc<Shared>) -> Writer {
    let conn = Connection::open(dir.join("state.db")).expect("open the state file");
    schema::init(&conn).expect("schema");
    schema::configure(&conn).expect("configure");
    let head = schema::head(&conn).expect("head");
    Writer {
        conn,
        clock: clock(),
        options,
        shared: Arc::clone(shared),
        head: uint(head),
    }
}

/// Queues one request per batch, in order, then runs a writer over them until the queue,
/// now closed, is empty. Returns each request's answer, in the order they were queued.
fn serve(
    dir: &Path,
    options: Options,
    shared: &Arc<Shared>,
    batches: Vec<Vec<Txn>>,
) -> Vec<Result<Reply, StoreError>> {
    let (requests, queue) = mpsc::sync_channel(batches.len().max(1));
    let answers: Vec<Receiver<Result<Reply, StoreError>>> = batches
        .into_iter()
        .map(|txns| {
            let (reply, answer) = mpsc::sync_channel(1);
            let request = Request {
                txns,
                reap: false,
                trim: None,
                queued: Instant::now(),
                reply,
            };
            shared.queue(&requests, request);
            answer
        })
        .collect();
    drop(requests);
    writer(dir, options, shared).run(&queue);
    answers
        .into_iter()
        .map(|answer| answer.recv().expect("every queued request is answered"))
        .collect()
}

/// The commits of every answer, in order.
fn commits(answers: Vec<Result<Reply, StoreError>>) -> Vec<Commit> {
    answers
        .into_iter()
        .flat_map(|answer| answer.expect("the group commits").commits)
        .map(|commit| commit.expect("an unconditional put commits"))
        .collect()
}

fn groups(shared: &Shared) -> u64 {
    shared
        .metrics
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .groups
}

// Catches a lone write waiting out the window for a group that never forms: on a store
// opened with the default options, a caller writing one write at a time is never held for
// the window, and each of its writes is a commit of its own.
#[test]
fn a_lone_write_does_not_wait_for_the_window() {
    let dir = unit_dir("lone");
    let store =
        SqliteStore::open(&dir.path().join("state.db"), clock(), Options::default()).expect("open");
    for n in 0..5 {
        store
            .txn(put(&format!("k{n}")))
            .expect("the store is reachable")
            .expect("an unconditional put commits");
    }
    assert_eq!(
        store.shared.window_waits.load(Ordering::Acquire),
        0,
        "no write had another queued behind it, so none waited the window"
    );
    assert_eq!(store.commit_stats().syncs, 5, "one commit per lone write");
    assert_eq!(store.commit_metrics().groups, 5);
    assert_eq!(
        store.shared.waiting.load(Ordering::Acquire),
        0,
        "every queued request was taken"
    );
}

// Catches a writer that stopped grouping: requests queued behind the one it takes wait the
// window with it and commit as one group, with one sync, in the order they were queued.
#[test]
fn writes_queued_together_are_one_group_and_one_sync() {
    let dir = unit_dir("queued");
    let shared = Arc::new(Shared::default());
    let options = Options {
        window: NEVER,
        max_group: 8,
        ..Options::default()
    };
    let batches = (0..8).map(|n| vec![put(&format!("q{n}"))]).collect();
    let commits = commits(serve(dir.path(), options, &shared, batches));
    let revisions: Vec<Revision> = commits.iter().map(|commit| commit.revision).collect();
    assert_eq!(
        revisions,
        (1..=8).map(Revision).collect::<Vec<_>>(),
        "the group applies its requests in the order they were queued"
    );
    assert_eq!(
        shared.commits.load(Ordering::Acquire),
        1,
        "8 writes, one sync"
    );
    assert_eq!(shared.txns.load(Ordering::Acquire), 8);
    assert_eq!(groups(&shared), 1);
    assert_eq!(shared.window_waits.load(Ordering::Acquire), 1);
    assert_eq!(shared.waiting.load(Ordering::Acquire), 0);
}

// Catches a group that waits for the window to close though its queue already has; and a
// group that is short of `max_group` is still one group: two requests, one sync.
#[test]
fn a_waiting_group_ends_when_the_queue_closes() {
    let dir = unit_dir("closed");
    let shared = Arc::new(Shared::default());
    let options = Options {
        window: NEVER,
        ..Options::default()
    };
    let batches = vec![vec![put("a")], vec![put("b")]];
    let commits = commits(serve(dir.path(), options, &shared, batches));
    assert_eq!(commits.len(), 2);
    assert_eq!(shared.commits.load(Ordering::Acquire), 1);
    assert_eq!(shared.window_waits.load(Ordering::Acquire), 1);
}

// Catches a group that waits though its first request already fills it: the window is for
// more to join, and none can. The request queued behind it is then the first of a group
// with nothing behind it, and does not wait either.
#[test]
fn a_first_request_that_fills_the_group_does_not_wait() {
    let dir = unit_dir("full");
    let shared = Arc::new(Shared::default());
    let options = Options {
        window: NEVER,
        max_group: 2,
        ..Options::default()
    };
    let batches = vec![vec![put("a"), put("b")], vec![put("c")]];
    let commits = commits(serve(dir.path(), options, &shared, batches));
    assert_eq!(commits.len(), 3);
    assert_eq!(shared.commits.load(Ordering::Acquire), 2, "two groups");
    assert_eq!(
        shared.window_waits.load(Ordering::Acquire),
        0,
        "neither group could take another request"
    );
}

// Catches a writer that syncs per write, or that does not group writers queued behind one
// another, at the target rate: 1500 writes from 50 callers, each round of 50 queued before
// the writer takes the first, take exactly one commit per round. The window is far longer
// than the test needs, so a group closes only because it is full.
#[test]
fn the_target_rate_takes_one_commit_per_round_of_queued_callers() {
    const CALLERS: usize = 50;
    let writes = usize::try_from(TARGET_WRITES_PER_SECOND).expect("the rate fits a usize");
    let rounds = writes / CALLERS;
    assert_eq!(
        rounds * CALLERS,
        writes,
        "the callers divide the rate evenly"
    );

    let dir = unit_dir("rate");
    let shared = Arc::new(Shared::default());
    let options = Options {
        window: NEVER,
        max_group: CALLERS,
        ..Options::default()
    };
    for round in 0..rounds {
        let batches = (0..CALLERS)
            .map(|caller| vec![put(&format!("gate/{caller:02}/{round:03}"))])
            .collect();
        let committed = commits(serve(dir.path(), options, &shared, batches));
        assert_eq!(committed.len(), CALLERS);
        assert_eq!(
            groups(&shared),
            round as u64 + 1,
            "round {round}: every round of {CALLERS} callers is one group"
        );
    }

    let commits = shared.commits.load(Ordering::Acquire);
    assert_eq!(
        shared.txns.load(Ordering::Acquire),
        writes as u64,
        "every write committed"
    );
    assert_eq!(
        commits, rounds as u64,
        "{writes} writes took {commits} commits; one per round of {CALLERS} is the bound"
    );
    assert!(
        commits * 10 <= writes as u64,
        "grouping must cut commits by at least ten times"
    );
    assert_eq!(shared.window_waits.load(Ordering::Acquire), rounds as u64);
    let largest = shared
        .metrics
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .largest_group;
    assert_eq!(largest, CALLERS as u64);

    let store = SqliteStore::open(&dir.path().join("state.db"), clock(), Options::default())
        .expect("reopen");
    assert_eq!(
        store.scan_prefix("gate/").expect("scan").len(),
        writes,
        "every acknowledged write is readable"
    );
}
