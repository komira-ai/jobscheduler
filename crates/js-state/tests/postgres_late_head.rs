//! The commit head taken only at a transaction's tail, on a real Postgres server: the bodies
//! of many writers run at once and a commit goes through while another transaction is under
//! way; a key with no row is reserved so two writers that both need it absent do not both
//! write it; a reservation never outlives its transaction; a pool with no connection free and
//! a statement past its server limit answer [`StoreError::Unavailable`]; and a writer of the
//! previous release, which takes the head first, still queues with this one.
//!
//! The tests need a server (`KBF_TEST_POSTGRES_URL` names it) and fail without one. They wait
//! on conditions (a count of threads, a count of connections waiting on a lock), never on
//! elapsed time. Where the code before this design would have waited for ever, the server's
//! `lock_timeout` ends the wait with an error (3 s: the bound of a failure, which a passing
//! test never reaches).
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use js_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore, Timeouts};
use js_state::{Claim, Cond, Conflict, Event, Revision, Store, StoreError, Txn};
use postgres::{Client, NoTls};

/// How many writers run their bodies at once in the concurrency test.
const WRITERS: usize = 4;

fn url() -> String {
    js_testkit::database::server_url()
}

/// The server URL with `lock_timeout` set, so a wait the old design made endless fails.
fn bounded_url() -> String {
    let base = url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20lock_timeout%3D3000")
}

/// A backend on `url` and a configuration for a fresh schema of it; the schema is dropped with
/// the backend, so a test keeps both.
fn fresh(url: &str) -> (PostgresBackend, PostgresConfig) {
    let backend = PostgresBackend::new(url, Flaw::None);
    let config = backend.fresh_config();
    (backend, config)
}

fn monitor() -> Client {
    Client::connect(&url(), NoTls).expect("a monitoring connection")
}

/// How many of the connections named `app` wait on a lock.
fn waiting(monitor: &mut Client, app: &str) -> i64 {
    monitor
        .query_one(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE application_name = $1 AND wait_event_type = 'Lock'",
            &[&app],
        )
        .expect("the activity view is readable")
        .get(0)
}

/// Yields until `ready` holds; fails after a bound of polls (a lost wake-up, not a time).
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..2_000_000_u32 {
        if ready() {
            return;
        }
        thread::yield_now();
    }
    panic!("{what}: never happened");
}

/// The rows of the store's `kv` table, placeholders included, for `key`.
fn raw_rows(monitor: &mut Client, schema: &str, key: &str) -> i64 {
    monitor
        .query_one(
            &format!("SELECT count(*) FROM {schema}.kv WHERE key = $1"),
            &[&key],
        )
        .expect("the table is readable")
        .get(0)
}

fn put(store: &PostgresStore, key: &str, value: &str) -> Revision {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store works")
        .expect("no precondition")
        .revision
}

/// Threads inside a body at once, and the most there ever were.
#[derive(Default)]
struct Inside {
    state: Mutex<(usize, usize, usize)>,
    changed: Condvar,
}

impl Inside {
    /// A thread entered its body: waits until every thread is inside or the rest have failed,
    /// and returns how many were inside at once.
    fn enter_and_wait(&self, all: usize) -> usize {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 += 1;
        state.1 = state.1.max(state.0);
        self.changed.notify_all();
        let state = self
            .changed
            .wait_while(state, |state| state.0 + state.2 < all)
            .unwrap_or_else(PoisonError::into_inner);
        state.1
    }

    /// A thread failed before it entered its body.
    fn failed(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.2 += 1;
        self.changed.notify_all();
    }

    fn most(&self) -> usize {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1
    }
}

// Catches a transaction that takes the commit head before its body: four writers each claim
// a row of their own and, inside the claim, wait until all four are inside theirs. With the
// head taken first only one is ever inside and the other three fail at the lock timeout;
// with the head taken at the tail all four are inside at once and all four commit, each at a
// revision of its own.
#[test]
fn the_bodies_of_writers_run_at_once() {
    let (_backend, config) = fresh(&bounded_url());
    let store = PostgresStore::open(&config).expect("opens");
    for w in 0..WRITERS {
        put(&store, &format!("q{w}/row"), "queued");
    }
    let inside = Inside::default();
    let revisions = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for w in 0..WRITERS {
            let (store, inside, revisions) = (&store, &inside, &revisions);
            scope.spawn(move || {
                let mut entered = false;
                let claimed = store.claim(&format!("q{w}/"), 1, |key, was| {
                    entered = true;
                    inside.enter_and_wait(WRITERS);
                    Some(Txn::new().put(key, "taken", Cond::Version(was.version)))
                });
                match claimed {
                    Ok(claimed) => revisions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend(claimed.iter().map(|claim| claim.commit.revision)),
                    Err(_) if !entered => inside.failed(),
                    Err(error) => panic!("a claim failed after its body: {error}"),
                }
            });
        }
    });
    assert_eq!(
        inside.most(),
        WRITERS,
        "the writers' bodies ran one at a time"
    );
    let mut revisions = revisions
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    revisions.sort();
    revisions.dedup();
    assert_eq!(
        revisions.len(),
        WRITERS,
        "each claim has a revision of its own"
    );
}

// Catches a body that holds the commit head: while one claim is inside its transaction (its
// row locked, its write not yet made), another writer's batch commits. With the head taken
// first the batch waits on it, and fails at the lock timeout; with the head at the tail it
// commits, and the claim, whose tail comes after, commits at the later revision.
#[test]
fn a_batch_commits_while_another_transaction_is_under_way() {
    let (_backend, config) = fresh(&bounded_url());
    let store = PostgresStore::open(&config).expect("opens");
    put(&store, "q/row", "queued");
    let mut during = None;
    let claimed = store
        .claim("q/", 1, |key, was| {
            during = Some(thread::scope(|scope| {
                scope
                    .spawn(|| store.txn(Txn::new().put("other", "v", Cond::Absent)))
                    .join()
                    .expect("no panic")
            }));
            Some(Txn::new().put(key, "taken", Cond::Version(was.version)))
        })
        .expect("the claim commits");
    let during = during
        .expect("the claim's body ran")
        .expect("the batch commits while the claim is under way")
        .expect("no precondition");
    assert_eq!(claimed.len(), 1);
    assert!(
        claimed[0].commit.revision > during.revision,
        "the claim's tail came after the batch, so its revision is later"
    );
    let feed = store.changes(Revision::ZERO, 10).expect("reads");
    let revisions: Vec<u64> = feed.rows.iter().map(|change| change.revision.0).collect();
    assert_eq!(
        revisions,
        vec![1, 2, 3],
        "the feed has no gap: {revisions:?}"
    );
}

// Catches a key with no row that is not reserved: A writes `k` on condition it is absent and
// holds its commit; B, which needs `k` absent too, waits on A's reservation rather than judging
// it absent. When A commits, B finds A's row and conflicts; the value is A's. A reservation that
// did not block would let B overwrite A.
#[test]
fn two_writers_that_need_a_key_absent_do_not_both_write_it() {
    let (_backend, config) = fresh(&url());
    let app = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    let mut monitor = monitor();
    let a = store
        .begin(Txn::new().put("k", "a", Cond::Absent))
        .expect("A runs to its commit");
    thread::scope(|scope| {
        let b = scope.spawn(|| store.txn(Txn::new().put("k", "b", Cond::Absent)));
        until("B waits on A's reservation", || {
            waiting(&mut monitor, &app) == 1
        });
        a.commit().expect("A commits");
        let b = b.join().expect("no panic").expect("the store works");
        assert_eq!(
            b,
            Err(Conflict {
                index: 0,
                key: "k".to_owned()
            }),
            "B wrote a key A had written"
        );
    });
    let row = store.get("k").expect("reads").expect("A's row");
    assert_eq!(row.value, b"a");
}

// Catches a reservation that outlives a transaction that rolled back: A reserves `k` and is
// dropped before it commits; B, waiting on the reservation, then finds `k` absent and writes it.
#[test]
fn a_reservation_goes_with_a_transaction_that_rolls_back() {
    let (_backend, config) = fresh(&url());
    let app = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    let mut monitor = monitor();
    let a = store
        .begin(Txn::new().put("k", "a", Cond::Absent))
        .expect("A runs to its commit");
    thread::scope(|scope| {
        let b = scope.spawn(|| store.txn(Txn::new().put("k", "b", Cond::Absent)));
        until("B waits on A's reservation", || {
            waiting(&mut monitor, &app) == 1
        });
        drop(a);
        b.join()
            .expect("no panic")
            .expect("the store works")
            .expect("B finds the key absent");
    });
    assert_eq!(store.get("k").expect("reads").expect("B's row").value, b"b");
}

// Catches a placeholder row that a transaction commits: every way a transaction can reserve a
// key and leave it absent (a failed version check, a check that it is absent with only a counter
// moved, a put then a delete in one batch, a decision that declines after reading the key, a
// delete of a key with no row) leaves no row for it, reads and scans see none, and a later write
// that needs it absent succeeds. The feed holds exactly the writes made, at consecutive
// revisions, and the head moved by their number.
#[test]
fn a_key_left_absent_holds_no_row_after_any_commit() {
    let (_backend, config) = fresh(&url());
    let schema = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    let mut monitor = monitor();

    let failed = store
        .txn(Txn::new().put("ghost/version", "v", Cond::Version(js_state::Version(5))))
        .expect("the store works");
    assert!(failed.is_err(), "a version check on no row conflicts");

    let counted = store
        .apply_batch(vec![
            Txn::new().next("ids"),
            Txn::new().check("ghost/checked", Cond::Absent),
        ])
        .expect("the store works");
    assert_eq!(counted[0].as_ref().expect("applies").counters, vec![1]);
    assert!(counted[1].is_ok(), "the key is absent");

    let churn = store
        .apply_batch(vec![
            Txn::new().put("ghost/churn", "v", Cond::Absent),
            Txn::new().delete("ghost/churn", Cond::Any),
            Txn::new().delete("ghost/never", Cond::Any),
        ])
        .expect("the store works");
    let churned: Vec<u64> = churn
        .iter()
        .map(|result| result.as_ref().expect("applies").revision.0)
        .collect();
    assert_eq!(
        churned,
        vec![1, 2, 2],
        "a put and a delete take two revisions"
    );

    put(&store, "q/row", "queued");
    let declined = store
        .claim_decide(&["q/row".to_owned()], &mut |read| {
            assert_eq!(read.get("ghost/decided").expect("reads"), None);
            None
        })
        .expect("the store works");
    assert_eq!(declined, Claim::Declined);

    for key in [
        "ghost/version",
        "ghost/checked",
        "ghost/churn",
        "ghost/never",
        "ghost/decided",
    ] {
        assert_eq!(raw_rows(&mut monitor, &schema, key), 0, "{key} holds a row");
        assert_eq!(store.get(key).expect("reads"), None, "{key} reads as a row");
        store
            .txn(Txn::new().put(key, "now", Cond::Absent))
            .expect("the store works")
            .expect("the key is absent for a later writer");
    }
    assert!(
        store.scan_prefix("ghost/").expect("scans").len() == 5,
        "only the later writes are rows"
    );
    let events: Vec<(u64, String, bool)> = store
        .changes(Revision::ZERO, 20)
        .expect("reads")
        .rows
        .into_iter()
        .map(|change| {
            let put = matches!(change.event, Event::Put(_));
            (change.revision.0, change.key, put)
        })
        .collect();
    let mut expected = vec![
        (1, "ghost/churn".to_owned(), true),
        (2, "ghost/churn".to_owned(), false),
        (3, "q/row".to_owned(), true),
    ];
    for (n, key) in ["version", "checked", "churn", "never", "decided"]
        .iter()
        .enumerate()
    {
        expected.push((4 + n as u64, format!("ghost/{key}"), true));
    }
    assert_eq!(events, expected);
    assert_eq!(store.head().expect("reads"), Some(Revision(8)));
}

// Catches a checkout that waits for ever when the pool has nothing free: with the store's only
// connection held by a transaction and no wait allowed, a read fails at once with
// `Unavailable`, saying why, and runs nothing; when the connection is back the read works.
#[test]
fn a_call_with_no_connection_free_is_unavailable() {
    let mut no_wait = Timeouts::default();
    no_wait.checkout = Duration::ZERO;
    let (_backend, config) = fresh(&url());
    let config = config.with_max_connections(1).with_timeouts(no_wait);
    let store = PostgresStore::open(&config).expect("opens");
    let held = store
        .begin(Txn::new().put("held", "1", Cond::Any))
        .expect("the only connection runs a transaction");
    assert_eq!(
        store.get("held"),
        Err(StoreError::Unavailable(
            "every one of the store's 1 connections stayed in use for 0ns".to_owned()
        ))
    );
    assert_eq!(
        store.get("held").map_err(|error| error.to_string()),
        Err(
            "state store busy: every one of the store's 1 connections stayed in use for 0ns"
                .to_owned()
        )
    );
    held.commit().expect("commits");
    assert!(store.get("held").expect("the connection is back").is_some());
}

// Catches a statement the server ends at its limit reported as a failure of the store: a write
// on a row another session holds waits past the store's statement timeout and is
// `Unavailable` (SQLSTATE 57014), and one past the URL's lock timeout too (55P03); neither
// wrote anything, and the same write commits once the row is free.
#[test]
fn a_statement_ended_at_its_limit_is_unavailable() {
    let mut short = Timeouts::default();
    short.statement = Some(Duration::from_millis(200));
    for (url, timeouts, code) in [
        (url(), short, "57014"),
        (bounded_url(), Timeouts::default(), "55P03"),
    ] {
        let (_backend, config) = fresh(&url);
        let config = config.with_timeouts(timeouts);
        let store = PostgresStore::open(&config).expect("opens");
        put(&store, "k", "before");
        let mut holder = monitor();
        holder
            .batch_execute(&format!(
                "BEGIN; SELECT 1 FROM {}.kv WHERE key = 'k' FOR UPDATE",
                config.schema()
            ))
            .expect("the holder locks the row");
        let refused = store.txn(Txn::new().put("k", "after", Cond::Any));
        match refused {
            Err(StoreError::Unavailable(why)) => {
                assert!(why.ends_with(&format!("(SQLSTATE {code})")), "{why}");
            }
            other => panic!("not unavailable ({code}): {other:?}"),
        }
        assert_eq!(
            store.get("k").expect("reads").expect("a row").value,
            b"before"
        );
        holder.batch_execute("COMMIT").expect("released");
        put(&store, "k", "after");
        assert_eq!(
            store.retries(),
            0,
            "a statement past its limit is not run again"
        );
    }
}

// Catches a tail that does not queue with a writer of the previous release, which locks the
// head first and holds it through its body: while such a writer holds the head (and has moved
// it, uncommitted), this release's batch runs its body and waits at its tail; when the old
// writer commits, the batch takes the next revision, so the two never take the same one and the
// feed has no gap.
#[test]
fn a_tail_queues_behind_a_writer_that_holds_the_head_from_its_start() {
    let (_backend, config) = fresh(&url());
    let app = config.schema().to_owned();
    let schema = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    put(&store, "first", "1");
    let mut old = monitor();
    old.batch_execute(&format!(
        "BEGIN ISOLATION LEVEL SERIALIZABLE; SET LOCAL search_path TO {schema}; \
         LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE; SELECT rev FROM head FOR UPDATE; \
         INSERT INTO kv (key, value, version) VALUES ('old', 'o', 2); \
         INSERT INTO feed (rev, key, kind, value) VALUES (2, 'old', 0, 'o'); \
         UPDATE head SET rev = 2"
    ))
    .expect("the old release's writer holds the head");
    let mut monitor = monitor();
    let done = AtomicUsize::new(0);
    thread::scope(|scope| {
        let new = scope.spawn(|| {
            let revision = put(&store, "new", "n");
            done.fetch_add(1, Ordering::SeqCst);
            revision
        });
        until("the new batch waits at its tail", || {
            waiting(&mut monitor, &app) == 1
        });
        assert_eq!(
            done.load(Ordering::SeqCst),
            0,
            "the batch committed past the head"
        );
        old.batch_execute("COMMIT").expect("the old writer commits");
        assert_eq!(new.join().expect("no panic"), Revision(3));
    });
    let revisions: Vec<u64> = store
        .changes(Revision::ZERO, 10)
        .expect("reads")
        .rows
        .iter()
        .map(|change| change.revision.0)
        .collect();
    assert_eq!(revisions, vec![1, 2, 3]);
}

// Catches a decided claim and a batch that wait on each other for good: the claim holds the
// head while it decides, a batch whose body locked `shared` waits for the head at its tail, and
// the claim's transaction then writes `shared`. The server finds the deadlock and rolls one of
// the two back; the store runs that one again (once: the retry count is 1), and both commit.
#[test]
fn a_decided_claim_and_a_batch_that_wait_on_each_other_both_commit() {
    let (_backend, config) = fresh(&url());
    let app = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    put(&store, "q/row", "queued");
    put(&store, "shared", "0");
    let mut monitor = monitor();
    thread::scope(|scope| {
        let mut batch = None;
        let outcome = store
            .claim_decide(&["q/row".to_owned()], &mut |_| {
                if batch.is_none() {
                    let store = &store;
                    batch =
                        Some(scope.spawn(move || {
                            store.txn(Txn::new().put("shared", "batch", Cond::Any))
                        }));
                    until("the batch waits for the head at its tail", || {
                        waiting(&mut monitor, &app) == 1
                    });
                }
                Some(Txn::new().put("q/row", "placed", Cond::Any).put(
                    "shared",
                    "decided",
                    Cond::Any,
                ))
            })
            .expect("the decided claim commits");
        assert!(matches!(outcome, Claim::Committed(_)), "{outcome:?}");
        batch
            .expect("the decision ran")
            .join()
            .expect("no panic")
            .expect("the batch commits")
            .expect("no precondition");
    });
    assert_eq!(store.retries(), 1, "one of the two was run again, once");
    assert_eq!(
        store.get("q/row").expect("reads").expect("a row").value,
        b"placed"
    );
}
