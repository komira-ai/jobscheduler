//! The ids a frontend numbers with, and the epoch it serves under, when several frontends
//! share one state store: blocks of operation ids and lease sequences reserved with a
//! compare-and-set on the counter rows, an epoch created once and kept, and the operator's
//! raise of it.

mod common;

use std::sync::Arc;

use js_state::{Cond, Conflict, Store, StoreError, Txn};
use js_store::keys::{EPOCH, NEXT_OP, NEXT_SEQ};
use js_store::{CAS_TRIES, EpochBump, EpochPolicy, ID_BLOCK, Rows, StateError};
use js_types::sched::{OperationId, WorkerId};

use common::{Racing, open};

/// A handle over `store` that kept the epoch it found or made (100 in an empty store).
fn rows_over(store: &Arc<Racing>) -> Rows {
    open(store.clone(), EpochPolicy::Keep, 100).expect("open").0
}

fn counter(store: &Racing, key: &str) -> u64 {
    let row = store.get(key).expect("read").expect("a row");
    u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
}

/// A write of `value` to the counter row `key`, whatever it holds.
fn set(key: &str, value: u64) -> Txn {
    Txn::new().put(key, value.to_be_bytes(), Cond::Any)
}

/// A write of bytes that are no number to the row `key`, whatever it holds.
fn spoil(store: &Racing, key: &str) {
    let junk = Txn::new().put(key, vec![1], Cond::Any);
    store.txn(junk).expect("store").expect("put");
}

fn bump(previous: u64, epoch: u64) -> EpochBump {
    EpochBump { previous, epoch }
}

/// Asserts that a reservation starts at the stored counter or at the frontend's own next
/// id, whichever is later, ends [`ID_BLOCK`] past its start and writes its end: the
/// stored counter never goes back, whatever the frontend's own next id is.
#[test]
fn a_reservation_starts_at_the_later_of_the_counter_and_the_own_next_id() {
    let store = Racing::new();
    let rows = rows_over(&store);
    let first = rows.reserve_seqs(0).expect("a first block");
    assert_eq!(first, 0..ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), ID_BLOCK);

    // An own next id behind the counter: the frontend's memory is stale.
    let stale = rows.reserve_seqs(3).expect("a block");
    assert_eq!(stale, ID_BLOCK..2 * ID_BLOCK);

    // An own next id ahead of it.
    let ahead = 5 * ID_BLOCK + 7;
    let block = rows.reserve_seqs(ahead).expect("a block");
    assert_eq!(block, ahead..ahead + ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), ahead + ID_BLOCK);

    let stale = rows.reserve_seqs(0).expect("a block");
    assert_eq!(stale.start, ahead + ID_BLOCK);
    assert!(store.get(NEXT_OP).expect("read").is_none());
}

/// Asserts that blocks reserved through several handles over one store never overlap, in
/// whatever order they ask and whatever their own next ids are: each starts where the one
/// before it ended.
#[test]
fn blocks_of_several_handles_over_one_store_never_overlap() {
    let store = Racing::new();
    let handles = [rows_over(&store), rows_over(&store), rows_over(&store)];
    let mut blocks = Vec::new();
    for round in 0..4_u64 {
        for (own, handle) in (0..).zip(&handles) {
            blocks.push(handle.reserve_seqs(round * own).expect("a block"));
        }
    }
    assert_eq!(blocks.len(), 12);
    for pair in blocks.windows(2) {
        assert_eq!(pair[1].start, pair[0].end, "{blocks:?}");
    }
    assert_eq!(counter(&store, NEXT_SEQ), 12 * ID_BLOCK);
}

/// Asserts that a reservation which loses the compare-and-set to another frontend reads
/// the counter again and reserves past the block the other took, whether the counter did
/// not exist when it read it or held an older value.
#[test]
fn a_reservation_that_loses_a_race_reserves_past_the_winners_block() {
    let store = Racing::new();
    let rows = rows_over(&store);
    store.land(NEXT_SEQ, set(NEXT_SEQ, 5_000));

    let block = rows.reserve_seqs(0).expect("a block");

    assert_eq!(block, 5_000..5_000 + ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), 5_000 + ID_BLOCK);
    store.land(NEXT_SEQ, set(NEXT_SEQ, 9_000));

    let block = rows.reserve_seqs(0).expect("a block");

    assert_eq!(block, 9_000..9_000 + ID_BLOCK);
}

/// Asserts that a reservation makes a bounded number of tries: it is taken on its last
/// try, and refused with the conflict after losing every one, having written nothing of
/// its own.
#[test]
fn a_reservation_gives_up_after_its_tries() {
    for (lost, taken) in [(CAS_TRIES - 1, true), (CAS_TRIES, false)] {
        let store = Racing::new();
        let rows = rows_over(&store);
        let mut last = 0;
        for n in 1..=lost {
            last = 100 * u64::try_from(n).expect("a small number");
            store.land(NEXT_SEQ, set(NEXT_SEQ, last));
        }

        let reserved = rows.reserve_seqs(0);

        if taken {
            assert_eq!(reserved.expect("taken"), last..last + ID_BLOCK);
        } else {
            assert!(
                matches!(reserved, Err(StateError::Conflict(_))),
                "{lost} lost: {reserved:?}"
            );
            assert_eq!(counter(&store, NEXT_SEQ), last, "a refused try wrote");
        }
    }
}

/// Asserts that a block never runs past the largest id: a frontend whose next id is within a
/// block of it is given the ids that are left, and the counter stops there.
#[test]
fn a_block_ends_at_the_largest_id_and_not_past_it() {
    let store = Racing::new();
    let rows = rows_over(&store);
    let near = u64::MAX - 5;

    let block = rows.reserve_seqs(near).expect("a block");

    assert_eq!(block, near..u64::MAX);
    assert_eq!(counter(&store, NEXT_SEQ), u64::MAX);
}

/// Asserts that a counter row that is not a number refuses the reservation, as corrupt and
/// naming the row, and is not written over.
#[test]
fn a_counter_that_is_not_a_number_refuses_the_reservation() {
    for (key, operation) in [(NEXT_SEQ, false), (NEXT_OP, true)] {
        let store = Racing::new();
        let rows = rows_over(&store);
        spoil(&store, key);

        let refused = if operation {
            rows.reserve_operation(OperationId(0)).map(drop)
        } else {
            rows.reserve_seqs(0).map(drop)
        };

        assert!(
            matches!(&refused, Err(StateError::Corrupt { key: named, .. }) if named == key),
            "{key}: {refused:?}"
        );
        let row = store.get(key).expect("read").expect("a row");
        assert_eq!(row.value, [1], "{key} was written over");
    }
}

/// Asserts that an operation id inside the block the handle reserved is used as it is, with
/// no write; that one ahead of the block is numbered in a block reserved at it; and that one
/// behind the block (a scheduler that did not move on after a submit that failed) is
/// numbered at the start of the block the handle holds, with no write and no new block.
#[test]
fn an_operation_id_outside_the_reserved_block_is_numbered_in_a_new_block() {
    let store = Racing::new();
    let rows = rows_over(&store);
    let first = rows.reserve_operation(OperationId(4)).expect("an id");
    assert_eq!(first, OperationId(4));
    let writes = store.commit_stats().txns;

    let inside = rows
        .reserve_operation(OperationId(ID_BLOCK + 3))
        .expect("an id");

    assert_eq!(inside, OperationId(ID_BLOCK + 3));
    let now = store.commit_stats().txns;
    assert_eq!(now, writes, "an id in the block wrote");
    let ahead = rows
        .reserve_operation(OperationId(3 * ID_BLOCK))
        .expect("an id");
    assert_eq!(ahead, OperationId(3 * ID_BLOCK));
    let writes = store.commit_stats().txns;
    let behind = rows.reserve_operation(OperationId(4)).expect("an id");
    assert_eq!(behind, OperationId(3 * ID_BLOCK));
    let now = store.commit_stats().txns;
    assert_eq!(now, writes, "an id behind the block wrote");
    assert_eq!(counter(&store, NEXT_OP), 4 * ID_BLOCK);
}

/// Asserts that an operation id is refused, not guessed, when the block cannot be
/// reserved: another frontend wins every try, and the error is the conflict.
#[test]
fn an_operation_id_is_refused_when_every_try_to_reserve_its_block_is_lost() {
    let store = Racing::new();
    let rows = rows_over(&store);
    for n in 1..=CAS_TRIES {
        let taken = ID_BLOCK * u64::try_from(n).expect("a small number");
        store.land(NEXT_OP, set(NEXT_OP, taken));
    }

    let refused = rows.reserve_operation(OperationId(0));

    assert!(
        matches!(refused, Err(StateError::Conflict(_))),
        "{refused:?}"
    );
}

/// Asserts that a frontend that loses the race to create the epoch of an empty store
/// reads the winner's and keeps it: the two end with one epoch.
#[test]
fn the_frontend_that_loses_the_race_to_make_the_epoch_keeps_the_winners() {
    let store = Racing::new();
    store.land(EPOCH, set(EPOCH, 777));

    let (_, epoch) = open(store.clone(), EpochPolicy::Keep, 100).expect("open");

    assert_eq!(epoch, 777);
    assert_eq!(counter(&store, EPOCH), 777);
}

/// Asserts that a bump that loses to another writer bumps what it reads next, so the
/// epoch is raised by one from the value the other left, for the operator's bump and for
/// an open that bumps alike.
#[test]
fn a_bump_that_loses_a_race_bumps_what_it_reads_next() {
    let store = Racing::new();
    let rows = rows_over(&store);
    store.land(EPOCH, set(EPOCH, 500));

    let bumped = rows.bump_epoch().expect("a bump");

    assert_eq!(bumped, bump(500, 501));
    assert_eq!(counter(&store, EPOCH), 501);
    store.land(EPOCH, set(EPOCH, 900));

    let (_, epoch) = open(store.clone(), EpochPolicy::Bump, 1).expect("open");

    assert_eq!(epoch, 901);
}

/// Asserts that an open that loses every try to settle the epoch is refused with the
/// conflict, not left to try for ever.
#[test]
fn an_open_that_loses_every_try_is_refused_with_the_conflict() {
    let store = Racing::new();
    drop(rows_over(&store));
    for n in 0..CAS_TRIES {
        let raised = 200 + u64::try_from(n).expect("a small number");
        store.land(EPOCH, set(EPOCH, raised));
    }

    let refused = open(store.clone(), EpochPolicy::Bump, 1).err();

    assert!(
        matches!(refused, Some(StateError::Conflict(_))),
        "{refused:?}"
    );
}

/// Asserts that a bump raises the stored epoch by one each time and is read by the next
/// handle, and that a store whose epoch row is not a number, or is gone, refuses it.
#[test]
fn a_bump_raises_the_stored_epoch_by_one_each_time() {
    let store = Racing::new();
    let rows = rows_over(&store);

    let first = rows.bump_epoch().expect("a bump");
    let second = rows.bump_epoch().expect("a bump");

    assert_eq!(first, bump(100, 101));
    assert_eq!(second, bump(101, 102));
    let (_, epoch) = open(store.clone(), EpochPolicy::Keep, 1).expect("open");
    assert_eq!(epoch, 102);
    spoil(&store, EPOCH);
    let corrupt = rows.bump_epoch();
    assert!(
        matches!(corrupt, Err(StateError::Corrupt { .. })),
        "{corrupt:?}"
    );
    let gone = Txn::new().delete(EPOCH, Cond::Any);
    store.txn(gone).expect("store").expect("delete");
    let missing = rows.bump_epoch();
    assert!(
        matches!(missing, Err(StateError::Missing(_))),
        "{missing:?}"
    );
}

/// Asserts that an epoch row that is not a number refuses the open, as corrupt, whichever
/// policy is asked, and is left as it was.
#[test]
fn an_epoch_that_is_not_a_number_refuses_the_open() {
    for policy in [EpochPolicy::Keep, EpochPolicy::Bump] {
        let store = Racing::new();
        spoil(&store, EPOCH);

        let refused = open(store.clone(), policy, 1).err();

        assert!(
            matches!(&refused, Some(StateError::Corrupt { key, .. }) if key == EPOCH),
            "{policy:?}: {refused:?}"
        );
        let row = store.get(EPOCH).expect("read").expect("a row");
        assert_eq!(row.value, [1]);
    }
}

/// Asserts that a store that cannot be written refuses the open, the bump and a reservation
/// with the backend's own error, and that the handle works again when the store does.
#[test]
fn a_store_that_is_down_refuses_every_write_with_its_own_error() {
    let store = Racing::new();
    let rows = rows_over(&store);
    store.set_down(true);

    let bumped = rows.bump_epoch().map(drop);
    let reserved = rows.reserve_seqs(0).map(drop);
    let operation = rows.reserve_operation(OperationId(0)).map(drop);
    for refused in [bumped, reserved, operation] {
        assert!(
            matches!(refused, Err(StateError::Store(StoreError::Failed(_)))),
            "{refused:?}"
        );
    }
    let fresh = Racing::new();
    fresh.set_down(true);
    let refused = open(fresh, EpochPolicy::Keep, 1).err();
    assert!(matches!(refused, Some(StateError::Store(_))), "{refused:?}");

    store.set_down(false);
    assert_eq!(rows.reserve_seqs(0).expect("a block"), 0..ID_BLOCK);
}

/// Asserts that a handle prints as its name and no more: it holds a store that has no text
/// of its own.
#[test]
fn a_handle_prints_as_its_name() {
    let store = Racing::new();
    assert_eq!(format!("{:?}", rows_over(&store)), "Rows");
}

/// Asserts that each way a row can be refused says which row and why.
#[test]
fn an_error_says_which_row_and_why() {
    let conflict = Conflict {
        index: 2,
        key: "meta/epoch".to_owned(),
    };
    let texts = [
        (
            StateError::Store(StoreError::Failed("disk full".to_owned())),
            "state store failure: disk full",
        ),
        (
            StateError::Conflict(conflict),
            "a row changed under the scheduler: operation 2 on \"meta/epoch\" failed its precondition",
        ),
        (
            StateError::Missing("op/0000000000000004".to_owned()),
            "the state store has no op/0000000000000004: the write is stale",
        ),
        (
            StateError::Corrupt {
                key: "meta/epoch".to_owned(),
                why: "not an 8-byte number".to_owned(),
            },
            "the state store row meta/epoch cannot be read: not an 8-byte number",
        ),
        (
            StateError::StaleOwner(WorkerId::new("node-a")),
            "the node node-a belongs to another frontend: the write is stale",
        ),
        (
            StateError::Codec("a request is too large".to_owned()),
            "a state store value cannot be encoded: a request is too large",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}
