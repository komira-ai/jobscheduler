//! The ended-allocation rows on Postgres: written once per allocation whatever its end, read
//! back in time order within inclusive bounds, a row under a key its end and id do not give
//! skipped, and pruned with its index entry at the retention window. Every store is a schema
//! of its own in the database `KBF_TEST_POSTGRES_URL` names (the test harness's setting; the
//! tests fail, and do not skip, without it). The `state-postgres` CI job runs this file with
//! the `postgres` feature.
#![cfg(feature = "postgres")]

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::postgres::{Flaw, PostgresBackend};
use js_state::{Cond, ManualClock, Store, Txn};
use js_types::HostId;
use js_usage::{
    AllocationId, DrainOutcome, ENDED_ID_PREFIX, ENDED_PREFIX, EndedHistory, EndedReason, EndedRow,
    ended_id_key, ended_key,
};

fn store() -> impl Store {
    let backend = PostgresBackend::new(&js_testkit::database::server_url(), Flaw::None);
    backend.open(Arc::new(ManualClock::default()))
}

/// The row of allocation `id`, ended at `end`.
fn row(end: u64, id: &str) -> EndedRow {
    EndedRow {
        id: AllocationId::parse(id).unwrap(),
        profile: None,
        host: Some(HostId::new("h1").unwrap()),
        booked: None,
        queued_ms: Some(0),
        run_ms: Some(5),
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

fn ends(store: &dyn Store, from: u64, to: u64) -> (Vec<u64>, u64) {
    let limit = NonZeroUsize::new(1_000).unwrap();
    let read = EndedHistory::new(store).range(from, to, limit).unwrap();
    (
        read.rows.iter().map(|row| row.actual_end).collect(),
        read.skipped,
    )
}

/// Catches: a second record of one allocation under a later end, an index entry that
/// does not name the row, a range bound on Postgres that leaves an end out or lets one in
/// (keys of the first and last millisecond included), a row under a key its end and id do
/// not give read as a row, and a prune that keeps the row at the window's edge wrongly or
/// leaves the index entry of a row it deleted.
#[test]
fn ended_rows_on_postgres_are_written_once_read_in_order_and_pruned_with_their_index() {
    let store = store();
    let history = EndedHistory::new(&store);
    for (end, id) in [
        (0, "a"),
        (100, "b"),
        (100, "c"),
        (200, "d"),
        (u64::MAX, "e"),
    ] {
        assert_eq!(history.put(&row(end, id)), Ok(true));
    }
    assert_eq!(history.put(&row(300, "b")), Ok(false));
    let index = store
        .get(&ended_id_key(&AllocationId::parse("b").unwrap()))
        .unwrap();
    let b_key = ended_key(100, &AllocationId::parse("b").unwrap());
    assert_eq!(index.map(|held| held.value), Some(b_key.into_bytes()));
    let moved = format!("{ENDED_PREFIX}{:020}/z", 150);
    let txn = Txn::new().put(moved, b"unused".to_vec(), Cond::Any);
    store.txn(txn).unwrap().unwrap();
    assert_eq!(
        ends(&store, 0, u64::MAX),
        (vec![0, 100, 100, 200, u64::MAX], 1)
    );
    assert_eq!(ends(&store, 100, 200), (vec![100, 100, 200], 1));
    assert_eq!(ends(&store, 101, 199), (vec![], 1));
    // The row under the wrong key ended before the window by its key, and goes too.
    assert_eq!(history.prune(300, 100), Ok(4));
    assert_eq!(ends(&store, 0, u64::MAX), (vec![200, u64::MAX], 0));
    let indexes = store.scan_prefix(ENDED_ID_PREFIX).unwrap();
    assert_eq!(indexes.len(), 2);
}
