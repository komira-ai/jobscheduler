//! `Store::claim_decide` and `Store::head` on every backend: a decision made on locked rows
//! commits, declines, conflicts or is held, and the head moves with every commit whoever made it.
//! The Postgres tests also hold a row from another connection, and race writers over one row.

mod common;

use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::sqlite::Options;
use js_state::{Claim, Cond, ManualClock, Store, Txn, TxnRead};

fn put(store: &dyn Store, key: &str, value: &str) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// The value at `key`, as text.
fn text(store: &dyn Store, key: &str) -> Option<String> {
    store
        .get(key)
        .expect("reads")
        .map(|row| String::from_utf8(row.value).expect("text"))
}

/// A decision that moves the claimed `queue/<n>` row to `placed/<n>` when it still reads `queued`,
/// as a placement does, and reads the rows of a prefix beside it.
fn place(store: &dyn Store, key: &str, booked_prefix: &str) -> (Claim, usize) {
    let mut seen = 0;
    let claim = [key.to_owned()];
    let outcome = store
        .claim_decide(&claim, &mut |read: &mut dyn TxnRead| {
            let row = read.get(key).expect("reads")?;
            if row.value != b"queued" {
                return None;
            }
            seen = read.scan_prefix(booked_prefix).expect("scans").len();
            Some(
                Txn::new()
                    .put(key, "placed", Cond::Version(row.version))
                    .put(format!("{booked_prefix}{key}"), "1", Cond::Absent),
            )
        })
        .expect("the store answers");
    (outcome, seen)
}

/// A decision commits what it decided on the rows it read, declines when it returns nothing,
/// is a conflict when a precondition of what it decided fails, and is held (with nothing
/// decided) when a claimed key holds no live row. Catches a claim that commits a declined
/// decision, one that reports a failed precondition as a commit, and one that decides on a key
/// that is not there.
fn decisions_commit_decline_conflict_or_are_held(store: &dyn Store) {
    put(store, "q/a", "queued");
    put(store, "on/x", "1");
    let (committed, seen) = place(store, "q/a", "on/");
    assert!(matches!(committed, Claim::Committed(_)), "{committed:?}");
    assert_eq!(seen, 1, "the decision read the rows under the prefix");
    assert_eq!(text(store, "q/a").as_deref(), Some("placed"));
    assert_eq!(text(store, "on/q/a").as_deref(), Some("1"));

    let (declined, _) = place(store, "q/a", "on/");
    assert_eq!(
        declined,
        Claim::Declined,
        "a row that no longer reads queued"
    );

    put(store, "q/b", "queued");
    let claim = ["q/b".to_owned()];
    let conflict = store
        .claim_decide(&claim, &mut |_| {
            Some(
                Txn::new()
                    .put("z", "1", Cond::Absent)
                    .put("on/x", "2", Cond::Absent),
            )
        })
        .expect("answers");
    assert!(
        matches!(conflict, Claim::Conflict(ref c) if c.key == "on/x"),
        "{conflict:?}"
    );
    assert_eq!(text(store, "z"), None, "a conflict wrote nothing");

    let missing = ["q/none".to_owned()];
    let mut decided = false;
    let held = store
        .claim_decide(&missing, &mut |_| {
            decided = true;
            None
        })
        .expect("answers");
    assert_eq!(held, Claim::Held);
    assert!(!decided, "a key with no row is not decided on");
}

/// The head moves with every commit and with nothing else, whoever commits: a second handle's
/// commit is seen by the first.
fn the_head_moves_with_every_commit(store: &dyn Store, other: &dyn Store) {
    let Some(before) = store.head().expect("reads") else {
        panic!("this store tells its head");
    };
    assert_eq!(
        store.head().expect("reads"),
        Some(before),
        "a read moves nothing"
    );
    put(other, "k", "v");
    let after = store.head().expect("reads").expect("a head");
    assert!(after > before, "{after:?} is not past {before:?}");
    put(store, "k", "w");
    assert!(store.head().expect("reads").expect("a head") > after);
}

#[test]
fn the_fake_decides_claims() {
    let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    decisions_commit_decline_conflict_or_are_held(&fake);
}

#[test]
fn the_fake_tells_its_head() {
    let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    the_head_moves_with_every_commit(&fake, &fake);
}

// The SQLite store has one writer, so it uses the provided claim: reads, then a transaction with
// a check on each claimed row's version. It tells no head.
#[test]
fn sqlite_decides_claims_with_the_provided_implementation_and_tells_no_head() {
    let dir = common::scratch("claim-sqlite");
    let (store, _clock) = common::open_manual(&dir, Options::default());
    decisions_commit_decline_conflict_or_are_held(&store);
    assert_eq!(store.head().expect("answers"), None);
}

// The provided claim puts a check on the claimed row, so a write to it between the read and the
// commit is a conflict, not a lost update. Catches a claim that does not check what it read.
#[test]
fn the_provided_claim_conflicts_when_the_claimed_row_changes_under_it() {
    let dir = common::scratch("claim-sqlite-race");
    let (store, _clock) = common::open_manual(&dir, Options::default());
    put(&store, "q/a", "queued");
    let claim = ["q/a".to_owned()];
    let outcome = store
        .claim_decide(&claim, &mut |read| {
            let row = read.get("q/a").expect("reads")?;
            put(&store, "q/a", "changed elsewhere");
            Some(Txn::new().put("p/a", "1", Cond::Absent).put(
                "q/a",
                "placed",
                Cond::Version(row.version),
            ))
        })
        .expect("answers");
    assert!(matches!(outcome, Claim::Conflict(_)), "{outcome:?}");
    assert_eq!(text(&store, "p/a"), None);
}

#[cfg(feature = "postgres")]
mod postgres {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use js_state::postgres::{PostgresConfig, PostgresStore, test_database};
    use js_state::{Claim, Cond, Store, StoreError, Txn};
    use postgres::{Client, NoTls};

    use super::{
        decisions_commit_decline_conflict_or_are_held, place, put, text,
        the_head_moves_with_every_commit,
    };

    fn open(url: &str) -> PostgresStore {
        PostgresStore::open(&PostgresConfig::parse(url).expect("parses")).expect("opens")
    }

    #[test]
    fn postgres_decides_claims() {
        let database = test_database();
        decisions_commit_decline_conflict_or_are_held(&open(&database.url()));
    }

    #[test]
    fn postgres_tells_its_head_across_handles() {
        let database = test_database();
        let (first, second) = (open(&database.url()), open(&database.url()));
        the_head_moves_with_every_commit(&first, &second);
    }

    // A row another transaction holds is skipped, not waited for: the claim answers Held at once
    // and writes nothing, and once the holder lets go the same claim commits. Catches a claim
    // that waits on a held row, and one that decides on it anyway.
    #[test]
    fn a_row_another_transaction_holds_is_held_not_waited_for() {
        let database = test_database();
        let store = open(&database.url());
        put(&store, "q/a", "queued");
        let mut holder = Client::connect(&database.url(), NoTls).expect("connects");
        holder
            .batch_execute(&format!(
                "BEGIN; SET LOCAL search_path TO {}; SELECT 1 FROM kv WHERE key = 'q/a' FOR UPDATE",
                store.config().schema()
            ))
            .expect("the holder locks the row");
        let (held, _) = place(&store, "q/a", "on/");
        assert_eq!(held, Claim::Held);
        assert_eq!(text(&store, "q/a").as_deref(), Some("queued"));
        holder.batch_execute("COMMIT").expect("released");
        let (committed, _) = place(&store, "q/a", "on/");
        assert!(matches!(committed, Claim::Committed(_)), "{committed:?}");
    }

    // Writers on separate handles claim one queued row and each places it by the same decision:
    // exactly one commits, the others read it already placed and decline (or find it held), and
    // the rows booked beside it, which every decision counts, number one. Catches two writers
    // that both place a job, which an optimistic read before the commit cannot rule out.
    #[test]
    fn writers_racing_for_one_row_place_it_once() {
        let database = test_database();
        let url = database.url();
        put(&open(&url), "q/a", "queued");
        let committed = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let store = open(&url);
                    let (outcome, _) = place(&store, "q/a", "on/");
                    if matches!(outcome, Claim::Committed(_)) {
                        committed.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(
            committed.load(Ordering::SeqCst),
            1,
            "one writer placed the job"
        );
        let store = open(&url);
        assert_eq!(text(&store, "q/a").as_deref(), Some("placed"));
        assert_eq!(store.scan_prefix("on/").expect("scans").len(), 1);
    }

    // Writers racing for different rows each count what the others have booked when they decide:
    // with room for three of eight, three commit and five decline, whatever the interleaving,
    // because a decision reads the booked rows in its own transaction. Catches a count read before
    // the transaction, which lets more than the room commit.
    #[test]
    fn writers_deciding_on_a_count_never_pass_it() {
        let database = test_database();
        let url = database.url();
        let setup = open(&url);
        for n in 0..8 {
            put(&setup, &format!("q/{n}"), "queued");
        }
        let committed = AtomicUsize::new(0);
        thread::scope(|scope| {
            for n in 0..8 {
                let (url, committed) = (&url, &committed);
                scope.spawn(move || {
                    let store = open(url);
                    let key = format!("q/{n}");
                    let claim = [key.clone()];
                    let outcome = store
                        .claim_decide(&claim, &mut |read| {
                            let booked = read.scan_prefix("on/").expect("scans").len();
                            if booked >= 3 {
                                return None;
                            }
                            Some(Txn::new().put(format!("on/{n}"), "1", Cond::Any))
                        })
                        .expect("answers");
                    if matches!(outcome, Claim::Committed(_)) {
                        committed.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(committed.load(Ordering::SeqCst), 3);
        assert_eq!(setup.scan_prefix("on/").expect("scans").len(), 3);
    }

    // A decision that reads under a prefix sees the rows its own transaction holds: the claimed row,
    // and a key it read as absent stays absent in the scan, as one it holds present stays present.
    // Catches a scan that answers from the table alone, which would show a row the transaction
    // saw deleted or miss one it loaded.
    #[test]
    fn a_decision_sees_the_rows_its_own_transaction_holds_in_a_scan() {
        let database = test_database();
        let store = open(&database.url());
        put(&store, "q/a", "queued");
        put(&store, "q/b", "queued");
        let claim = ["q/a".to_owned()];
        let mut seen = Vec::new();
        let outcome = store
            .claim_decide(&claim, &mut |read| {
                assert!(read.get("q/none").expect("reads").is_none());
                assert!(read.get("q/b").expect("reads").is_some());
                seen = read
                    .scan_prefix("q/")
                    .expect("scans")
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                None
            })
            .expect("answers");
        assert_eq!(outcome, Claim::Declined);
        assert_eq!(seen, ["q/a", "q/b"], "the absent key is not in the scan");
    }

    // A failed read inside the decision fails the call with the store's error and commits nothing.
    #[test]
    fn a_decision_whose_read_fails_fails_the_call() {
        let database = test_database();
        let store = open(&database.url());
        put(&store, "q/a", "queued");
        let claim = ["q/a".to_owned()];
        // A scan over a prefix the server cannot be asked for: a NUL byte is not text.
        let failed = store.claim_decide(&claim, &mut |read| {
            assert!(read.scan_prefix("bad\0prefix").is_err());
            Some(Txn::new().put("never", "1", Cond::Any))
        });
        assert!(matches!(failed, Err(StoreError::Failed(_))), "{failed:?}");
        assert_eq!(text(&store, "never"), None);
    }

    #[test]
    fn the_head_of_a_sequence_store_is_unknown() {
        let database = test_database();
        let store = open(&database.url()).with_flaw(js_state::postgres::Flaw::SequenceRevisions);
        assert_eq!(store.head().expect("answers"), None);
    }
}
