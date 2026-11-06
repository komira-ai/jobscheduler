//! The fake's two-step commit (`prepare` then `finish`) and its ungrouped-sync flaw, on
//! the answers the conformance suite never asks for: a transaction that loses its
//! precondition between the two steps, and a conflict under a store that syncs per
//! transaction.

use std::sync::Arc;

use js_state::fake::{FAKE_SYNC_COST, Fake, Flaw};
use js_state::{Clock, Cond, ManualClock, Store, StoreTime, Txn};

fn fake(flaw: Flaw) -> (Arc<ManualClock>, Fake) {
    let clock = Arc::new(ManualClock::default());
    (Arc::clone(&clock), Fake::with_flaw(clock, flaw))
}

// Catches `finish` writing a prepared transaction whose precondition another writer
// broke after `prepare`: it must answer with the conflict, leave the other writer's row,
// and pay no sync for a write that did not happen.
#[test]
fn finish_answers_the_conflict_of_a_precondition_lost_since_prepare() {
    let (clock, store) = fake(Flaw::None);
    let deferred = store.prepare(Txn::new().put("k", "late", Cond::Absent));
    store
        .txn(Txn::new().put("k", "first", Cond::Any))
        .expect("store")
        .expect("commits");
    let synced = clock.now();
    let conflict = store.finish(deferred).expect_err("k exists now");
    assert_eq!((conflict.index, conflict.key.as_str()), (0, "k"));
    assert_eq!(
        store.get("k").expect("store").expect("row").value,
        b"first",
        "the lost write must not land"
    );
    assert_eq!(clock.now(), synced, "a conflict pays no sync");
}

// Catches an ungrouped store that syncs after a transaction that did not apply: only a
// commit costs store time.
#[test]
fn an_ungrouped_store_syncs_for_a_commit_and_not_for_a_conflict() {
    let (clock, store) = fake(Flaw::Ungrouped);
    store
        .txn(Txn::new().put("k", "v", Cond::Absent))
        .expect("store")
        .expect("commits");
    let after_commit = clock.now();
    assert_eq!(
        after_commit,
        StoreTime::ZERO.saturating_add(FAKE_SYNC_COST),
        "one sync for the commit"
    );
    store
        .txn(Txn::new().put("k", "w", Cond::Absent))
        .expect("store")
        .expect_err("k exists");
    assert_eq!(clock.now(), after_commit, "a conflict pays no sync");
}

// Catches a delete of a key that is not there writing a feed row or taking a revision: it
// commits, changes nothing, and the next write gets the next revision.
#[test]
fn deleting_a_key_that_is_not_there_commits_and_writes_nothing() {
    let (_clock, store) = fake(Flaw::None);
    let first = store
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("store")
        .expect("commits");
    store
        .txn(Txn::new().delete("absent", Cond::Any))
        .expect("store")
        .expect("commits");
    let rows = store
        .changes(js_state::Revision::ZERO, 10)
        .expect("feed")
        .rows;
    assert_eq!(rows.len(), 1, "only the put is in the feed: {rows:?}");
    assert_eq!(rows[0].revision, first.revision);
}

// Catches a trim that passes a revision a prepared transaction still holds: a trim bounded
// past the head stops below the first revision that can still commit, so the prepared row is
// read after its commit from the floor the trim left.
#[test]
fn a_trim_stops_below_a_revision_that_can_still_commit() {
    let (_clock, store) = fake(Flaw::None);
    for key in ["a", "b"] {
        store
            .txn(Txn::new().put(key, "v", Cond::Any))
            .expect("store")
            .expect("commits");
    }
    let deferred = store.prepare(Txn::new().put("late", "v", Cond::Absent));
    store
        .txn(Txn::new().put("c", "v", Cond::Any))
        .expect("store")
        .expect("commits");
    assert_eq!(store.trim_feed(js_state::Revision(u64::MAX)), Ok(2));
    store.finish(deferred).expect("commits");
    let page = store.changes(js_state::Revision(2), 10).expect("store");
    let keys: Vec<&str> = page.rows.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(keys, ["late", "c"], "a row the trim must keep was lost");
    assert_eq!(
        store.changes(js_state::Revision(1), 10),
        Err(js_state::StoreError::StaleCursor {
            after: js_state::Revision(1),
            floor: js_state::Revision(2),
        })
    );
}
