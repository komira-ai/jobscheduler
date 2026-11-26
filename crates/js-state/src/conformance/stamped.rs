//! [`Store::txn_stamped`]: one transaction built from the store's clock as read for it, applied
//! as [`Store::txn`] applies the transaction built.

use std::sync::Arc;
use std::time::Duration;

use super::{Backend, run};
use crate::clock::{Clock, ManualClock};
use crate::store::Store;
use crate::types::{Change, Commit, Cond, Conflict, Event, Revision, Txn};

/// `build` stamped on `store`: the answer, and the readings `build` was called with.
fn stamped<S: Store>(
    store: &S,
    mut build: impl FnMut(crate::StoreTime) -> Option<Txn>,
) -> (Option<Result<Commit, Conflict>>, Vec<crate::StoreTime>) {
    let mut seen = Vec::new();
    let answer = store
        .txn_stamped(&mut |now| {
            seen.push(now);
            build(now)
        })
        .expect("the store is reachable");
    (answer, seen)
}

/// The value of `key` on `store`, if it has one.
fn value_of<S: Store>(store: &S, key: &str) -> Option<Vec<u8>> {
    let row = store.get(key).expect("the store is reachable");
    row.map(|row| row.value)
}

/// The feed of `store` after `after`.
fn feed_after<S: Store>(store: &S, after: Revision) -> Vec<Change> {
    store.changes(after, 100).expect("the feed reads").rows
}

/// A transaction that creates keys is built once, from the store's clock as it reads before
/// the call's commit, and its puts commit in order at consecutive revisions, the last the
/// commit's; one whose build declines writes nothing and answers `None`; one whose key holds a
/// live row answers the conflict of that operation and writes none of its keys; one whose key
/// holds a row the clock expired puts over it; and one with other operations (a delete, a check)
/// is applied as [`Store::txn`] applies it.
pub fn a_stamped_txn_is_built_from_the_store_clock_and_applied_as_built<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(5));
    let store = backend.open(Arc::clone(&clock));
    let at = clock.now();
    let (made, seen) = stamped(&store, |now| {
        let stamp = now.since(crate::StoreTime::ZERO).as_micros().to_string();
        let txn = Txn::new()
            .put("st/a", stamp.into_bytes(), Cond::Absent)
            .put("st/b", b"b".to_vec(), Cond::Any);
        Some(txn)
    });
    assert_eq!(
        seen,
        [at],
        "stamped clock: build is called once, with the store's clock"
    );
    let commit = made
        .expect("stamped commit: a built transaction is applied")
        .expect("stamped commit: keys that hold no row are created");
    assert_eq!(
        value_of(&store, "st/a"),
        Some(b"5000000".to_vec()),
        "stamped commit: the row holds what was built from the reading"
    );
    let feed: Vec<(Revision, String, Event)> = feed_after(&store, Revision::ZERO)
        .into_iter()
        .map(|change| (change.revision, change.key, change.event))
        .collect();
    let first = Revision(commit.revision.0 - 1);
    assert_eq!(
        feed,
        [
            (first, "st/a".to_owned(), Event::Put(b"5000000".to_vec())),
            (
                commit.revision,
                "st/b".to_owned(),
                Event::Put(b"b".to_vec())
            ),
        ],
        "stamped commit: the puts commit in order, the last at the commit's revision"
    );

    let (declined, _) = stamped(&store, |_| None);
    assert_eq!(
        declined, None,
        "stamped decline: a declined build answers None"
    );
    assert_eq!(
        feed_after(&store, commit.revision),
        Vec::new(),
        "stamped decline: a declined build writes nothing"
    );

    let (taken, _) = stamped(&store, |_| {
        let txn = Txn::new().put("st/c", b"c".to_vec(), Cond::Absent).put(
            "st/a",
            b"again".to_vec(),
            Cond::Absent,
        );
        Some(txn)
    });
    assert_eq!(
        taken,
        Some(Err(Conflict {
            index: 1,
            key: "st/a".to_owned()
        })),
        "stamped conflict: a key that holds a live row is the conflict of its operation"
    );
    assert_eq!(
        (value_of(&store, "st/a"), value_of(&store, "st/c")),
        (Some(b"5000000".to_vec()), None),
        "stamped conflict: a transaction that conflicts writes none of its keys"
    );

    run(
        &store,
        Txn::new().put_ttl(
            "st/e",
            b"old".to_vec(),
            Duration::from_secs(1),
            Cond::Absent,
        ),
    )
    .expect("the expiring row commits");
    clock.advance(Duration::from_secs(1));
    let (over, _) = stamped(&store, |_| {
        Some(Txn::new().put("st/e", b"new".to_vec(), Cond::Absent))
    });
    assert!(
        matches!(over, Some(Ok(_))),
        "stamped expiry: a key whose row expired is absent, so it is created: {over:?}"
    );
    assert_eq!(
        value_of(&store, "st/e"),
        Some(b"new".to_vec()),
        "stamped expiry: the new row replaces the expired one"
    );

    let (mixed, _) = stamped(&store, |_| {
        let txn = Txn::new()
            .check("st/b", Cond::Any)
            .delete("st/b", Cond::Any)
            .put("st/d", b"d".to_vec(), Cond::Absent);
        Some(txn)
    });
    assert!(
        matches!(mixed, Some(Ok(_))),
        "stamped other operations: a check, a delete and a put commit: {mixed:?}"
    );
    assert_eq!(
        (value_of(&store, "st/b"), value_of(&store, "st/d")),
        (None, Some(b"d".to_vec())),
        "stamped other operations: the delete and the put are applied"
    );
}
