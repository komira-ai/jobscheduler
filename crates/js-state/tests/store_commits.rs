//! How the SQLite store groups writes, seen from its callers. A write with no other queued
//! behind it is committed at once; writes queued behind one another share a group, which
//! holds at most `max_group` transactions. Which writes of concurrent callers share a group
//! depends on the order the threads run in, so the concurrent test asserts only what holds
//! for every order. The exact counts at the target write rate
//! ([`js_state::TARGET_WRITES_PER_SECOND`]), and that a lone write never waits the window,
//! are shown with the requests queued in a fixed order, by the unit tests in
//! `src/sqlite/group_tests.rs`. No test here asserts a latency: speed depends on the
//! hardware.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};
use std::thread;

use common::{open_manual, scratch};
use js_state::sqlite::Options;
use js_state::{Cond, Store, Txn};

const WRITERS: usize = 8;
const WRITES: usize = 100;
const MAX_GROUP: usize = 4;

// Catches a group that takes more than `max_group` transactions, a write that commits
// twice or not at all, or a sync with nothing in it: whatever order the callers' threads run
// in, every acknowledged write is readable and was committed once, no group is larger than
// the bound, and every sync is a group.
#[test]
fn concurrent_writers_commit_every_write_once_in_groups_no_larger_than_the_bound() {
    let dir = scratch("store-commits");
    let options = Options {
        max_group: MAX_GROUP,
        ..Options::default()
    };
    let (store, _clock) = open_manual(&dir, options);
    let store = Arc::new(store);
    let before = store.commit_stats();

    let start = Arc::new(Barrier::new(WRITERS));
    let callers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                (0..WRITES)
                    .map(|n| {
                        store
                            .txn(Txn::new().put(format!("c/{writer}/{n:03}"), "x", Cond::Any))
                            .expect("the store is reachable")
                            .expect("an unconditional put commits")
                            .revision
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut revisions = BTreeSet::new();
    for caller in callers {
        for revision in caller.join().expect("a caller finished") {
            assert!(
                revisions.insert(revision),
                "{revision:?} acknowledged twice"
            );
        }
    }

    let writes = (WRITERS * WRITES) as u64;
    assert_eq!(revisions.len() as u64, writes, "one revision per write");
    let stats = store.commit_stats();
    assert_eq!(stats.txns - before.txns, writes, "every write committed");
    let syncs = stats.syncs - before.syncs;
    let metrics = store.commit_metrics();
    assert_eq!(
        metrics.groups, syncs,
        "every group wrote, so each is one sync"
    );
    assert!(
        syncs * MAX_GROUP as u64 >= writes,
        "{writes} writes in {syncs} groups: some group took more than {MAX_GROUP}"
    );
    assert!(metrics.largest_group <= MAX_GROUP as u64);
    assert_eq!(
        store.scan_prefix("c/").expect("scan").len() as u64,
        writes,
        "every acknowledged write is readable"
    );
}

// A batch as large as `max_group` is a group of its own, and so is a lone write after it:
// one sync each. This takes, in the integration build, the arm of a group its first request
// fills; that such a group does not wait the window though another request is queued behind
// it is shown by the unit test `a_first_request_that_fills_the_group_does_not_wait`.
#[test]
fn a_write_that_fills_its_group_commits_at_once() {
    let dir = scratch("store-commits-full");
    let options = Options {
        max_group: 2,
        ..Options::default()
    };
    let (store, _clock) = open_manual(&dir, options);
    let before = store.commit_stats();
    let batch = vec![
        Txn::new().put("full/a", "x", Cond::Any),
        Txn::new().put("full/b", "x", Cond::Any),
    ];
    let results = store.apply_batch(batch).expect("the batch commits");
    assert!(results.iter().all(Result::is_ok));
    store
        .txn(Txn::new().put("full/c", "x", Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
    let stats = store.commit_stats();
    assert_eq!(stats.txns - before.txns, 3);
    assert_eq!(
        stats.syncs - before.syncs,
        2,
        "the batch and the write, one sync each"
    );
}
