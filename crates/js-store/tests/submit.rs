//! Submitting a request is safe to repeat: a store commit whose outcome the caller never
//! learned (the connection died after COMMIT) leaves the operation written, and the retry must
//! find it, not queue the work a second time. A request for work another frontend already
//! queued over the same store attaches to that operation.

mod common;

use std::sync::Arc;

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::keys;
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, RowCodec, Rows, StateError, Submitted, WorkerUp};
use js_types::sched::{LeaseGrant, LeaseKind, OperationId, Qos, Resources, WorkKey};

use common::{Racing, frontend_with};

fn request(tag: u8) -> Request {
    Request {
        key: WorkKey::new("main", format!("{tag:02x}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    }
}

/// A store, the codec its frontends share, and a handle for each of the frontends numbered
/// `ids`.
fn frontends<const N: usize>(ids: [u64; N]) -> (Arc<Racing>, Arc<TestCodec>, [Rows; N]) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let handles = ids.map(|id| frontend_with(&store, id, &codec));
    (store, codec, handles)
}

fn rows(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("delete");
}

/// The value of the in-flight row of `key`, if there is one.
fn pin_row(store: &Racing, key: &WorkKey) -> Option<Vec<u8>> {
    store
        .get(&keys::inflight(key))
        .expect("read")
        .map(|row| row.value)
}

/// The end of operation `operation` as a result ends it, without the result: its `op/` and
/// `queue/` rows go, and its in-flight row with them.
fn finish(rows: &Rows, store: &Racing, operation: u64) {
    let operation = OperationId(operation);
    let txn = Txn::new()
        .delete(keys::operation(operation), Cond::Any)
        .delete(keys::queue(operation), Cond::Any);
    let txn = rows.unpin(txn, operation).expect("unpin");
    store.txn(txn).expect("store").expect("the finish");
}

fn queued(submitted: Result<Submitted, StateError>) -> OperationId {
    match submitted {
        Ok(Submitted::Queued(id)) => id,
        other => panic!("not queued: {other:?}"),
    }
}

/// Catches: a retry after a commit whose outcome was unknown that is refused for good (the
/// op row it wrote is in the way of its own retry) or that queues the work a second time
/// (two op rows, two runs). After the lost acknowledgement and one retry there is exactly one
/// op row, one queue row and one in-flight row, and the retry joins the operation the first
/// try wrote.
#[test]
fn a_retry_after_a_commit_of_unknown_outcome_joins_the_operation_it_wrote() {
    let (store, _, [rows_of]) = frontends([1]);
    let id = rows_of.reserve_operation(OperationId(0)).expect("an id");
    store.lose_next_ack();

    let first = rows_of.submit(id, &request(1), &[7], None);
    assert!(matches!(first, Err(StateError::Store(_))), "{first:?}");
    let retried = rows_of.submit(id, &request(1), &[7], None);

    let Ok(Submitted::Joined(op)) = retried else {
        panic!("the retry did not join: {retried:?}");
    };
    assert_eq!(op.operation, id);
    assert_eq!(op.parcel.as_deref(), Some(&[7][..]));
    assert_eq!(rows(&store, "op/"), 1, "the work was written twice");
    assert_eq!(rows(&store, "queue/"), 1);
    assert_eq!(rows(&store, "inflight/"), 1);
}

/// Catches: a second frontend over the same store (a successor that read the store before the
/// first queued the work, or a retry that reaches the other child) that queues the work again
/// under its own id, or is refused. The second attaches to the operation the store holds: one
/// op row, the same id.
#[test]
fn a_second_frontend_attaches_to_the_operation_the_store_holds() {
    let (store, _, [first, second]) = frontends([1, 2]);
    let id = first.reserve_operation(OperationId(0)).expect("an id");
    assert_eq!(queued(first.submit(id, &request(1), &[], None)), id);
    let own = second.reserve_operation(OperationId(0)).expect("an id");
    assert_ne!(own, id, "two frontends numbered one id");

    let attached = second.submit(own, &request(1), &[], None);

    let Ok(Submitted::Joined(op)) = attached else {
        panic!("the second submit did not join: {attached:?}");
    };
    assert_eq!(op.operation, id);
    assert_eq!(op.lease, None);
    assert_eq!(rows(&store, "op/"), 1, "the work was queued twice");
    assert_eq!(rows(&store, "queue/"), 1);
}

/// Catches: an in-flight row left behind by the finish (the next request for the work would
/// join an operation that is gone, and never run), and a finish that takes the row of a later
/// operation of the same work. Also that the row is the id of the operation it pins.
#[test]
fn the_finish_removes_the_pin_of_its_own_operation_only() {
    let (store, _, [handle]) = frontends([1]);
    let key = request(1).key;
    let first = handle.submit(OperationId(0), &request(1), &[], None);
    assert!(matches!(first, Ok(Submitted::Queued(OperationId(0)))));
    assert_eq!(pin_row(&store, &key), Some(0_u64.to_be_bytes().to_vec()));
    finish(&handle, &store, 0);
    assert_eq!(rows(&store, "inflight/"), 0, "the finish left the pin");

    // A pin that names another operation than the one finishing stays.
    let second = handle.submit(OperationId(1), &request(1), &[], None);
    assert!(matches!(second, Ok(Submitted::Queued(OperationId(1)))));
    let unpinned = handle.unpin(Txn::new(), OperationId(1)).expect("unpin");
    assert_eq!(unpinned.ops().len(), 1, "the owner's finish keeps its pin");
    overwrite(&store, &keys::inflight(&key), 9_u64.to_be_bytes().to_vec());
    let unpinned = handle.unpin(Txn::new(), OperationId(1)).expect("unpin");
    assert!(
        unpinned.ops().is_empty(),
        "operation 1 took operation 9's pin"
    );
    finish(&handle, &store, 1);
    assert_eq!(rows(&store, "inflight/"), 1, "the other's pin went");
}

/// Catches: a pin whose operation is gone (a writer that died between the two, an older
/// build's finish) that keeps the work from ever being queued again.
#[test]
fn a_pin_of_a_finished_operation_is_replaced() {
    let (store, _, [handle]) = frontends([1]);
    overwrite(
        &store,
        &keys::inflight(&request(1).key),
        7_u64.to_be_bytes().to_vec(),
    );

    let submitted = handle.submit(OperationId(0), &request(1), &[], None);

    assert!(
        matches!(submitted, Ok(Submitted::Queued(OperationId(0)))),
        "{submitted:?}"
    );
    assert_eq!(
        pin_row(&store, &request(1).key),
        Some(0_u64.to_be_bytes().to_vec())
    );
    assert_eq!(rows(&store, "op/"), 1);
}

/// Catches: a request that may not join a twin (networked, or told not to cache) being pinned,
/// so a second caller joins a run that is not interchangeable with its own.
#[test]
fn a_request_that_cannot_join_has_no_pin() {
    let (store, _, [handle]) = frontends([1]);
    let mut networked = request(1);
    networked.hermetic = false;
    let mut uncached = request(1);
    uncached.do_not_cache = true;
    for (id, request) in [(0, &networked), (1, &networked), (2, &uncached)] {
        let submitted = handle.submit(OperationId(id), request, &[], None);
        assert!(
            matches!(submitted, Ok(Submitted::Queued(_))),
            "{submitted:?}"
        );
    }
    assert_eq!(rows(&store, "inflight/"), 0);
    assert_eq!(rows(&store, "op/"), 3);
}

/// Asserts that a second write of the id of a queued operation, by a request that cannot join
/// (so that no in-flight row sends it to the next id), is refused over the first and changes
/// nothing: the operation's rows are those of the first.
#[test]
fn a_second_submission_of_one_id_is_refused_over_the_first() {
    let (store, _, [handle]) = frontends([1]);
    let mut networked = request(1);
    networked.hermetic = false;
    handle
        .submit(OperationId(4), &networked, &[1], Some(7))
        .expect("submit");
    let before = store.scan_prefix("").expect("scan");

    let again = handle.submit(OperationId(4), &networked, &[2], Some(8));

    assert!(matches!(again, Err(StateError::Conflict(_))), "{again:?}");
    assert_eq!(store.scan_prefix("").expect("scan"), before);
}

/// Asserts that one submission is one transaction, with every row of the operation in it:
/// the `op/`, `queue/`, `inflight/` and `opkey/` rows and the embedder's parcel.
#[test]
fn a_submission_is_one_transaction_with_every_row_of_the_operation() {
    let (store, _, [handle]) = frontends([1]);
    let id = handle.reserve_operation(OperationId(4)).expect("an id");
    let before = store.commit_stats().txns;

    handle
        .submit(id, &request(1), &[1, 2, 3], Some(7))
        .expect("submit");

    assert_eq!(store.commit_stats().txns, before + 1);
    for (prefix, held) in [
        ("op/", 1),
        ("queue/", 1),
        ("inflight/", 1),
        ("opkey/", 1),
        ("parcel/", 1),
    ] {
        assert_eq!(rows(&store, prefix), held, "{prefix}");
    }
}

/// Asserts that a request given no size key writes no `opkey/` row.
#[test]
fn an_operation_given_no_size_key_has_no_opkey_row() {
    let (store, _, [handle]) = frontends([1]);
    handle
        .submit(OperationId(1), &request(1), &[], None)
        .expect("submit");
    handle
        .submit(OperationId(2), &request(2), &[], Some(7))
        .expect("submit");
    let found: Vec<String> = store
        .scan_prefix("opkey/")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(found, ["opkey/0000000000000002"]);
}

/// Asserts that a submit whose next id has a row in the way (an earlier writer's, which it
/// never saw) is not refused for good and does not write over the other's operation: it takes
/// the next id of its block.
#[test]
fn a_submit_whose_next_id_has_a_row_takes_the_next_id() {
    let (store, _, [other, handle]) = frontends([1, 2]);
    let landed = other.submit(OperationId(0), &request(2), &[], None);
    assert!(landed.is_ok());

    let submitted = handle.submit(OperationId(0), &request(1), &[], None);

    assert_eq!(queued(submitted), OperationId(1));
    assert_eq!(rows(&store, "op/"), 2);
    let next = handle.reserve_operation(OperationId(2)).expect("an id");
    assert_eq!(next, OperationId(2), "the block moved past the id taken");
}

/// Catches: a pin written over another writer's (the condition that it be absent dropped),
/// which queues one piece of work twice when two writers look at the same moment. The other
/// writer's operation and pin land after this frontend looked and before it wrote; it is
/// refused the write, looks again and joins that operation.
#[test]
fn a_pin_that_lands_after_the_look_is_not_written_over() {
    let (store, codec, [handle]) = frontends([1]);
    let landing = Txn::new()
        .put(
            keys::operation(OperationId(5)),
            codec.submit(&request(1)).expect("encode"),
            Cond::Absent,
        )
        .put(keys::queue(OperationId(5)), Vec::new(), Cond::Absent)
        .put(
            keys::inflight(&request(1).key),
            5_u64.to_be_bytes(),
            Cond::Absent,
        );
    store.land(&keys::inflight(&request(1).key), landing);

    let submitted = handle.submit(OperationId(0), &request(1), &[], None);

    let Ok(Submitted::Joined(op)) = submitted else {
        panic!("the submit did not join: {submitted:?}");
    };
    assert_eq!(op.operation, OperationId(5));
    assert_eq!(rows(&store, "op/"), 1, "the work was queued twice");
}

/// Catches: a pin that names an operation of other work being joined (the caller would wait
/// on the wrong run and be handed its result), and a row of the operation that cannot be read
/// being taken up as if it were whole: each is refused as corrupt.
#[test]
fn a_pin_or_row_that_cannot_be_trusted_is_refused() {
    let (store, _, [handle]) = frontends([1]);
    assert!(
        handle
            .submit(OperationId(0), &request(2), &[], None)
            .is_ok()
    );
    overwrite(
        &store,
        &keys::inflight(&request(1).key),
        0_u64.to_be_bytes().to_vec(),
    );
    let refused = handle.submit(OperationId(1), &request(1), &[], None);
    assert!(
        matches!(refused, Err(StateError::Corrupt { .. })),
        "{refused:?}"
    );

    let bad_rows = [
        ("loss/0000000000000000", vec![1, 2, 3]),
        (
            "loss/0000000000000000",
            (1_u64 << 32).to_be_bytes().to_vec(),
        ),
        ("fate/0000000000000000", vec![255]),
        ("opkey/0000000000000000", vec![255]),
        ("parcel/0000000000000000", vec![255]),
    ];
    for (key, value) in bad_rows {
        overwrite(&store, key, value);
        let refused = handle.submit(OperationId(1), &request(2), &[], None);
        assert!(
            matches!(refused, Err(StateError::Corrupt { .. })),
            "{key}: {refused:?}"
        );
        remove(&store, key);
    }
}

/// Catches: an operation that was queued and then requeued with losses and a fate being
/// joined without them (the joiner's frontend would give it a fresh budget), and one with
/// none being refused.
#[test]
fn a_joined_operation_keeps_its_losses_and_fate() {
    let (store, _, [handle, other]) = frontends([1, 2]);
    handle
        .submit(OperationId(0), &request(1), &[9], None)
        .expect("submit");
    let mut fate = vec![2];
    fate.extend((1_u64 << 30).to_be_bytes());
    fate.extend([0; 12]);
    fate.extend(2_u32.to_be_bytes());
    overwrite(
        &store,
        "loss/0000000000000000",
        3_u64.to_be_bytes().to_vec(),
    );
    overwrite(&store, "fate/0000000000000000", fate);
    remove(&store, "parcel/0000000000000000");

    let joined = other.submit(OperationId(100), &request(1), &[], None);

    let Ok(Submitted::Joined(op)) = joined else {
        panic!("the request did not join: {joined:?}");
    };
    assert_eq!(op.losses, 3);
    let fate = op.fate.expect("a fate");
    assert_eq!((fate.booked_bytes, fate.not_started), (1 << 30, 2));
    assert_eq!(op.parcel, None, "a parcel the writer did not keep");
}

/// Catches: a store that is down at the write being taken for a refusal that stays (the retry
/// of a client after the outage is refused too) or leaving a row.
#[test]
fn a_store_that_is_down_refuses_the_submit_and_takes_the_retry() {
    let (store, _, [handle]) = frontends([1]);
    store.set_down(true);

    let refused = handle.submit(OperationId(0), &request(1), &[], None);

    assert!(matches!(refused, Err(StateError::Store(_))), "{refused:?}");
    assert_eq!(rows(&store, "op/"), 0);
    store.set_down(false);
    let retried = handle.submit(OperationId(0), &request(1), &[], None);
    assert_eq!(queued(retried), OperationId(0));
    assert_eq!(rows(&store, "op/"), 1);
}

/// Catches: a submit whose looks are not exactly three: one that gives up after the first
/// conflict (a busy store refuses work it could take), or one that looks without end or once
/// too often. Each of `landed` writers takes the id this submit tried just before its write,
/// so every try conflicts and the next takes the id after. Three conflicts and the fourth try
/// is taken, under the fourth id; four conflicts and the submit is refused with the conflict,
/// having written nothing of its own.
#[test]
fn a_submit_takes_three_conflicts_and_is_refused_at_the_fourth() {
    for (landed, taken) in [(3_u64, true), (4, false)] {
        let (store, _, [handle]) = frontends([1]);
        for id in 0..landed {
            let landing = Txn::new()
                .put(
                    keys::operation(OperationId(id)),
                    b"later".to_vec(),
                    Cond::Absent,
                )
                .put(keys::queue(OperationId(id)), Vec::new(), Cond::Absent);
            store.land(&keys::operation(OperationId(id)), landing);
        }

        let submitted = handle.submit(OperationId(0), &request(1), &[], None);

        if taken {
            assert!(
                matches!(submitted, Ok(Submitted::Queued(OperationId(3)))),
                "{landed} conflicts: {submitted:?}"
            );
            assert_eq!(rows(&store, "inflight/"), 1);
        } else {
            assert!(
                matches!(submitted, Err(StateError::Conflict(_))),
                "{landed} conflicts: {submitted:?}"
            );
            assert_eq!(rows(&store, "inflight/"), 0, "a refused submit pinned");
        }
        assert_eq!(rows(&store, "op/"), landed as usize + usize::from(taken));
    }
}

/// Asserts that a submit whose id is the largest there is, with a row in its way, looks again
/// at the same id instead of overflowing, and is refused with the conflict.
#[test]
fn the_id_after_the_largest_saturates() {
    let (store, _, [handle]) = frontends([1]);
    let key = keys::operation(OperationId(u64::MAX));
    overwrite(&store, &key, b"later".to_vec());

    let submitted = handle.submit(OperationId(u64::MAX), &request(1), &[], None);

    assert!(
        matches!(submitted, Err(StateError::Conflict(_))),
        "{submitted:?}"
    );
    assert_eq!(rows(&store, "inflight/"), 0);
}

/// Catches: an unpin of an operation whose rows are already gone failing the finish, or
/// putting a delete in its transaction.
#[test]
fn unpinning_an_operation_with_no_rows_changes_nothing() {
    let (_, _, [handle]) = frontends([1]);
    let unpinned = handle.unpin(Txn::new(), OperationId(7)).expect("unpin");
    assert!(unpinned.ops().is_empty());
}

/// A codec that cannot encode a request.
struct Failing;

impl RowCodec for Failing {
    fn submit(&self, _: &Request) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no request".to_owned()))
    }

    fn lease(&self, _: &LeaseGrant) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no".to_owned()))
    }

    fn session(&self, _: &WorkerUp) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no".to_owned()))
    }

    fn read(&self, key: &str, _: &[u8]) -> Result<js_store::Value, StateError> {
        Err(StateError::Codec(format!("cannot read {key}")))
    }
}

/// Asserts that a request the codec cannot encode writes no row, whether it can join or not:
/// the codec's own error is returned.
#[test]
fn a_request_the_codec_cannot_encode_is_not_queued() {
    let store = Racing::new();
    let handle: Arc<dyn Store> = store.clone();
    let (rows_of, _) = Rows::open(
        handle,
        EpochPolicy::Keep,
        1,
        Arc::new(Failing),
        Arc::new(TestExt),
    )
    .expect("open");
    let mut networked = request(1);
    networked.hermetic = false;

    for request in [request(1), networked] {
        let refused = rows_of.submit(OperationId(0), &request, &[], None);
        assert!(
            matches!(&refused, Err(StateError::Codec(why)) if why == "no request"),
            "{refused:?}"
        );
    }

    assert_eq!(rows(&store, "op/"), 0);
    assert_eq!(rows(&store, "inflight/"), 0);
}
