//! Unit tests of the statement caches: the reader and the writer each prepare a statement
//! once per connection, not once per call or per group. Prepares are counted with SQLite's
//! authorizer, which SQLite calls while it prepares a statement and never while it runs
//! one. The `BEGIN` and `ROLLBACK` of a read transaction and the `BEGIN IMMEDIATE` and
//! `COMMIT` of a group are left out of the count: rusqlite runs those itself, uncached.

use std::num::NonZeroUsize;
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use js_testkit::dir::TestDir;
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::writer::{Reply, Request, Shared, Writer, uint};
use super::{Options, SqliteStore, schema};
use crate::{
    Clock, Cond, Direction, Revision, ScanRange, Store, StoreError, SystemClock, Txn, Versioned,
};

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::default())
}

fn unit_dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-state-cache-{name}"))
}

/// Counts the statements `conn` prepares from now on, other than transaction control.
/// Installing the authorizer expires the statements `conn` has already prepared, so each
/// of those is prepared again, and counted, the next time it runs.
fn count_prepares(conn: &mut Connection) -> Arc<AtomicUsize> {
    let prepares = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&prepares);
    let hook = move |context: AuthContext<'_>| {
        if !matches!(context.action, AuthAction::Transaction { .. }) {
            counter.fetch_add(1, Ordering::AcqRel);
        }
        Authorization::Allow
    };
    conn.authorizer(Some(hook)).expect("install the authorizer");
    prepares
}

fn put(store: &SqliteStore, key: &str) {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

fn keys(rows: &[(String, Versioned)]) -> Vec<&str> {
    rows.iter().map(|(key, _)| key.as_str()).collect()
}

/// Every statement the reader has, once each: `get`, `scan_prefix`, the twelve shapes of
/// `scan_range`, and `changes`.
fn read_everything(store: &SqliteStore) {
    let limit = NonZeroUsize::MIN;
    store.get("b").expect("get");
    store.scan_prefix("b").expect("scan_prefix");
    for lower in [Bound::Included("a"), Bound::Excluded("a")] {
        for upper in [Bound::Included("z"), Bound::Excluded("z"), Bound::Unbounded] {
            for direction in [Direction::Forward, Direction::Reverse] {
                let scan = ScanRange {
                    prefix: "",
                    lower,
                    upper,
                    direction,
                    limit,
                };
                store.scan_range(scan).expect("scan_range");
            }
        }
    }
    store.changes(Revision::ZERO, 1).expect("changes");
}

// Catches a read path that prepares its statement on every call, and a reader cache too
// small to hold every statement the reader runs (it would evict one on every pass and
// prepare each again): after one pass over every read statement, a second pass prepares
// nothing.
#[test]
fn the_reader_prepares_each_statement_once() {
    let dir = unit_dir("reader");
    let store =
        SqliteStore::open(&dir.join("state.db"), clock(), Options::default()).expect("open");
    for key in ["a", "b", "c"] {
        put(&store, key);
    }
    let prepares = count_prepares(&mut store.reader());

    read_everything(&store);
    assert!(
        prepares.load(Ordering::Acquire) > 0,
        "the first pass prepares the reader's statements, and the count sees it"
    );
    prepares.store(0, Ordering::Release);
    for _ in 0..3 {
        read_everything(&store);
    }
    assert_eq!(
        prepares.load(Ordering::Acquire),
        0,
        "every later read runs a statement the reader already prepared"
    );
}

// Catches a cached statement handed back to the reader's cache still holding the snapshot
// of a read that stopped before its last row: the reader would go on answering from that
// snapshot and never see a later commit. A range scan and a feed page each stop after one
// row of three; then a commit is made, and `get`, both scans and the feed all see it.
#[test]
fn a_read_that_stops_early_does_not_keep_its_snapshot() {
    let dir = unit_dir("snapshot");
    let store =
        SqliteStore::open(&dir.join("state.db"), clock(), Options::default()).expect("open");
    for key in ["a", "b", "c"] {
        put(&store, key);
    }
    let one = ScanRange::new("", NonZeroUsize::MIN);
    let first = store.scan_range(one).expect("scan_range");
    assert_eq!(keys(&first.rows), ["a"], "the page stops after one row");
    assert_eq!(store.scan_prefix("").expect("scan_prefix").len(), 3);
    let page = store.changes(Revision::ZERO, 1).expect("changes");
    assert_eq!(page.cursor, Revision(1), "a full page of one row");

    put(&store, "d");

    assert!(
        store.get("d").expect("get").is_some(),
        "get sees the commit"
    );
    assert_eq!(
        keys(&store.scan_prefix("").expect("scan_prefix")),
        ["a", "b", "c", "d"]
    );
    let all = ScanRange::new("", NonZeroUsize::new(10).expect("ten is not zero"));
    assert_eq!(
        keys(&store.scan_range(all).expect("scan_range").rows),
        ["a", "b", "c", "d"]
    );
    let changes = store.changes(Revision(3), 10).expect("changes");
    let revisions: Vec<u64> = changes.rows.iter().map(|row| row.revision.0).collect();
    assert_eq!(revisions, [4], "the feed reaches the commit");
    assert_eq!(changes.cursor, Revision(4));
}

/// A writer on the state file in `dir`, one transaction a group, with its prepares
/// counted from before its first group.
fn counted_writer(dir: &Path, shared: &Arc<Shared>) -> (Writer, Arc<AtomicUsize>) {
    let mut conn = Connection::open(dir.join("state.db")).expect("open the state file");
    schema::init(&conn).expect("schema");
    schema::configure(&conn).expect("configure");
    let head = schema::head(&conn).expect("head");
    let prepares = count_prepares(&mut conn);
    let options = Options {
        max_group: 1,
        ..Options::default()
    };
    let writer = Writer {
        conn,
        clock: clock(),
        options,
        shared: Arc::clone(shared),
        head: uint(head),
    };
    (writer, prepares)
}

/// A request that runs every statement of a group: a put with a time to live, a check, a
/// delete that removes a row, a counter, and then a reap and a trim. `n` keeps its keys
/// apart from the other requests'; `reply` is where its answer goes.
fn every_statement(n: usize, reply: SyncSender<Result<Reply, StoreError>>) -> Request {
    let kept = format!("kept-{n}");
    let gone = format!("gone-{n}");
    let txn = Txn::new()
        .put_ttl(kept.clone(), "v", Duration::from_secs(60), Cond::Absent)
        .put(gone.clone(), "v", Cond::Any)
        .check(kept, Cond::Any)
        .delete(gone, Cond::Any)
        .next("counter");
    Request {
        txns: vec![txn],
        reap: true,
        trim: Some(1),
        queued: Instant::now(),
        reply,
    }
}

/// Runs a fresh writer over `groups` requests of [`every_statement`], each a group of its
/// own; how many statements it prepared, and every answer.
fn serve(dir: &Path, groups: usize) -> (usize, Vec<Result<Reply, StoreError>>) {
    let shared = Arc::new(Shared::default());
    let (writer, prepares) = counted_writer(dir, &shared);
    let (requests, queue) = mpsc::sync_channel(groups);
    let answers: Vec<Receiver<Result<Reply, StoreError>>> = (0..groups)
        .map(|n| {
            let (reply, answer) = mpsc::sync_channel(1);
            shared.queue(&requests, every_statement(n, reply));
            answer
        })
        .collect();
    drop(requests);
    writer.run(&queue);
    let answers = answers
        .into_iter()
        .map(|answer| answer.recv().expect("every queued request is answered"))
        .collect();
    let groups_run = shared
        .metrics
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .groups;
    assert_eq!(groups_run, groups as u64, "one group a request");
    (prepares.load(Ordering::Acquire), answers)
}

// Catches a writer that prepares its statements for every group, and a writer cache too
// small to hold them all: a writer that commits five groups prepares exactly what a
// writer that commits one does, so the four groups after its first prepare nothing.
#[test]
fn the_writer_prepares_each_statement_once() {
    let one_dir = unit_dir("writer-one");
    let (one, answers) = serve(one_dir.path(), 1);
    assert!(one > 0, "the first group prepares the writer's statements");
    let reply = answers
        .into_iter()
        .next()
        .expect("one answer")
        .expect("the group commits");
    assert!(reply.commits.iter().all(Result::is_ok), "the txn commits");
    assert_eq!(reply.trimmed, 1, "the trim ran");

    let five_dir = unit_dir("writer-five");
    let (five, answers) = serve(five_dir.path(), 5);
    let trimmed: Vec<u64> = answers
        .into_iter()
        .map(|answer| {
            let reply = answer.expect("the group commits");
            assert!(reply.commits.iter().all(Result::is_ok), "every txn commits");
            reply.trimmed
        })
        .collect();
    assert_eq!(
        trimmed,
        [1, 0, 0, 0, 0],
        "the first group trims its first revision; the rest find nothing at or below it"
    );
    assert_eq!(
        five, one,
        "the groups after the first prepare nothing: their statements are cached"
    );
}
