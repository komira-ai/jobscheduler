//! C5 trim: the change feed can be trimmed from the old end, and a reader whose cursor
//! the trim passed is told so instead of being given a feed with a hole in it.

use std::sync::Arc;

use super::{Backend, drain, run};
use crate::clock::ManualClock;
use crate::store::Store;
use crate::types::{Cond, Revision, StoreError, Txn};

/// How many changes the clause writes first: one feed row each, at revisions 1 to this.
const WRITES: u64 = 10;

/// The first trim's bound.
const FIRST: u64 = 4;

/// The second trim's bound, higher than the first.
const SECOND: u64 = 7;

fn put<S: Store>(store: &S, n: u64) {
    run(store, Txn::new().put(format!("c5t/{n:03}"), "v", Cond::Any)).expect("put");
}

/// The revisions of every feed row after `from`.
fn revisions<S: Store>(store: &S, from: u64) -> Vec<u64> {
    let (rows, _) = drain(store, Revision(from));
    rows.iter().map(|row| row.revision.0).collect()
}

fn stale<S: Store>(store: &S, after: u64, floor: u64, what: &str) {
    assert_eq!(
        store.changes(Revision(after), 64),
        Err(StoreError::StaleCursor {
            after: Revision(after),
            floor: Revision(floor),
        }),
        "C5 trim stale: {what}: a read after {after} must be refused with the floor {floor}, \
         or it returns a feed that skips the trimmed rows"
    );
}

fn fresh<S: Store>(store: &S, from: u64, through: u64, what: &str) {
    assert_eq!(
        revisions(store, from),
        ((from + 1)..=through).collect::<Vec<_>>(),
        "C5 trim kept: {what}: a read after {from} must return every row up to {through}, in order"
    );
}

/// C5 trim: `trim_feed` deletes the rows at or below its bound and no others, counts
/// them, never lowers the floor, refuses a read below the floor, serves a read at or past
/// it as before, and keeps the trim across a restart.
pub fn c5_feed_trim<B: Backend>(backend: &B) {
    let mut store = backend.open(Arc::new(ManualClock::default()));
    for n in 1..=WRITES {
        put(&store, n);
    }

    assert_eq!(
        store
            .changes(Revision::ZERO, 64)
            .map(|page| page.rows.len()),
        Ok(WRITES as usize),
        "C5 trim floor: a feed never trimmed has floor 0 and serves a read from the start"
    );

    let removed = store.trim_feed(Revision(FIRST)).expect("trim");
    assert_eq!(
        removed, FIRST,
        "C5 trim count: trim_feed({FIRST}) returns exactly the number of rows it deleted"
    );
    fresh(&store, FIRST, WRITES, "after the first trim");
    stale(&store, FIRST - 1, FIRST, "just below the floor");
    stale(&store, 0, FIRST, "from the start");

    assert_eq!(
        store.trim_feed(Revision(FIRST)).expect("trim"),
        0,
        "C5 trim again: trimming at the floor again deletes nothing"
    );
    assert_eq!(
        store.trim_feed(Revision(FIRST - 2)).expect("trim"),
        0,
        "C5 trim below: trimming below the floor deletes nothing"
    );
    stale(
        &store,
        FIRST - 2,
        FIRST,
        "the floor does not fall with a lower trim",
    );
    fresh(&store, FIRST, WRITES, "after trims that did nothing");

    put(&store, WRITES + 1);
    fresh(&store, FIRST, WRITES + 1, "a row written after a trim");

    store = backend.restart(store);
    stale(
        &store,
        FIRST - 1,
        FIRST,
        "after a restart, the first trim is durable",
    );
    fresh(&store, FIRST, WRITES + 1, "after a restart");

    let removed = store.trim_feed(Revision(SECOND)).expect("trim");
    assert_eq!(
        removed,
        SECOND - FIRST,
        "C5 trim count: a second trim counts only the rows between the floors"
    );
    store = backend.restart(store);
    stale(
        &store,
        SECOND - 1,
        SECOND,
        "after a restart, the second trim is durable",
    );
    fresh(
        &store,
        SECOND,
        WRITES + 1,
        "after the second trim and a restart",
    );

    // A bound past the head removes everything and stops at the head: nothing beyond it
    // exists, and the next write is read from the head.
    let removed = store.trim_feed(Revision(WRITES * 100)).expect("trim");
    assert_eq!(
        removed,
        WRITES + 1 - SECOND,
        "C5 trim head: a bound past the head deletes every remaining row and no more"
    );
    let at_head = store
        .changes(Revision(WRITES + 1), 64)
        .expect("read at head");
    assert_eq!(
        (at_head.rows, at_head.cursor),
        (Vec::new(), Revision(WRITES + 1)),
        "C5 trim head: the floor is the head, and a read from the head is served, empty"
    );
    stale(&store, WRITES, WRITES + 1, "just below a floor at the head");
    put(&store, WRITES + 2);
    fresh(
        &store,
        WRITES + 1,
        WRITES + 2,
        "a write after a trim to the head",
    );
}
