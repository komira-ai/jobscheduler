//! The transitions of an operation in the state store when several frontends share it: a
//! frontend that loses a race to another finds its rows changed or gone and writes
//! nothing, and the other's work stands. Each test stands two handles over one store for
//! two frontends, and lands one frontend's write between the other's reads and its write
//! where the race is in that gap.
//!
//! The tests of the precondition of each write name the row the write is conditional on, put
//! the second writer's change to exactly that row, and assert that the first writer loses.
//!
//! A lease given up belongs to a later row of this crate, so the tests that need one make a
//! stand-in that touches the same rows: the lease rows go, and the operation is queued again
//! (with a loss count, if the test says one).

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::Request;
use js_state::{Cond, Store, Txn};
use js_store::keys;
use js_store::test_support::TestCodec;
use js_store::{Ending, Progress, RowCodec, Rows, StateError};
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

/// A store, the codec its frontends share, and a handle for each of the frontends numbered
/// `ids`.
fn frontends<const N: usize>(ids: [u64; N]) -> (Arc<Racing>, Arc<TestCodec>, [Rows; N]) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let handles = ids.map(|id| frontend_with(&store, id, &codec));
    (store, codec, handles)
}

/// The request for the work `tag`: no two tags are the same work.
fn request(tag: u8) -> Request {
    Request {
        key: WorkKey::new("main", format!("{tag:02x}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

/// Queues operation `operation`, which runs the work of the tag of its number.
fn submit(rows: &Rows, operation: u64) {
    let tag = u8::try_from(operation).expect("a small id");
    let submitted = rows.submit(OperationId(operation), &request(tag), &[7], Some(3));
    submitted.expect("submit");
}

/// The grant of lease `seq` of operation `operation` to `worker`.
fn lease_grant(operation: u64, seq: u64, worker: &str) -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
    }
}

fn grant(rows: &Rows, operation: u64, seq: u64, worker: &str) -> Result<(), StateError> {
    rows.granted(&lease_grant(operation, seq, worker))
}

/// An ending whose result is `message`, so two endings tell apart.
fn ending(message: &str) -> Ending {
    Ending::new(message.as_bytes().to_vec(), Duration::from_secs(1)).with_parcel(b"main".to_vec())
}

fn finish(rows: &Rows, operation: u64, seq: u64, message: &str) -> Result<(), StateError> {
    rows.finished(
        OperationId(operation),
        LeaseId::new(5, seq),
        &ending(message),
    )
}

/// What `progress` shows for an operation that ended with `message`.
fn ended(message: &str) -> Progress {
    Progress::Ended(message.as_bytes().to_vec())
}

fn count(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

/// A stand-in for the lease `(5, seq)` of `operation` given up: the lease rows go and the
/// operation is queued again, with `losses` in its `loss/` row if there are any.
fn give_up(store: &Racing, operation: u64, seq: u64, losses: Option<u64>) {
    let operation = OperationId(operation);
    let mut txn = Txn::new()
        .delete(keys::lease(LeaseId::new(5, seq)), Cond::Any)
        .delete(keys::lease_by_op(operation), Cond::Any)
        .put(keys::queue(operation), Vec::new(), Cond::Absent);
    if let Some(losses) = losses {
        txn = txn.put(keys::loss(operation), losses.to_be_bytes(), Cond::Any);
    }
    store.txn(txn).expect("store").expect("given up");
}

/// What another frontend writes to grant lease `seq` of `operation` to `node-b` over
/// whatever the rows hold.
fn another_grant(codec: &TestCodec, operation: u64, seq: u64) -> Txn {
    let value = codec
        .lease(&lease_grant(operation, seq, "node-b"))
        .expect("encodes");
    Txn::new()
        .delete(keys::queue(OperationId(operation)), Cond::Any)
        .put(keys::lease(LeaseId::new(5, seq)), value, Cond::Any)
}

/// `txn`, then what a frontend writes to end `operation` with the result `message`: the
/// operation's rows go and its result is put over whatever is there.
fn settled(txn: Txn, operation: u64, message: &str) -> Txn {
    let operation = OperationId(operation);
    txn.delete(keys::operation(operation), Cond::Any)
        .delete(keys::queue(operation), Cond::Any)
        .delete(keys::loss(operation), Cond::Any)
        .delete(keys::fate(operation), Cond::Any)
        .delete(format!("parcel/{:016x}", operation.0), Cond::Any)
        .delete(keys::opkey(operation), Cond::Any)
        .delete(keys::lease_by_op(operation), Cond::Any)
        .put_ttl(
            keys::done(operation),
            message.as_bytes(),
            Duration::from_secs(600),
            Cond::Any,
        )
}

fn is_missing<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Missing(_)))
}

/// Whether `result` is a conflict on the row `key`.
fn conflicts_on<T>(result: &Result<T, StateError>, key: &str) -> bool {
    matches!(result, Err(StateError::Conflict(conflict)) if conflict.key == key)
}

/// Asserts that a grant the store applied before and did not acknowledge is taken as
/// written when it is tried again, and that a grant is not taken as written when the lease
/// row names another worker or another lease.
#[test]
fn a_grant_written_before_is_taken_as_written_unless_the_lease_is_another() {
    let (store, _, [rows]) = frontends([1]);
    submit(&rows, 4);
    grant(&rows, 4, 0, "node-a").expect("grant");
    let txns = store.commit_stats().txns;

    grant(&rows, 4, 0, "node-a").expect("again");

    assert_eq!(
        store.commit_stats().txns,
        txns,
        "a written grant wrote again"
    );
    let stranger = grant(&rows, 4, 0, "node-b");
    assert!(is_missing(&stranger), "{stranger:?}");
    let second = grant(&rows, 4, 1, "node-a");
    assert!(is_missing(&second), "{second:?}");
}

/// Asserts that a result for a lease the farm gave up in the same feed finishes the queued
/// operation: its rows go, its loss count with them, and its result is the one row left.
#[test]
fn a_result_for_a_lease_the_farm_gave_up_finishes_the_queued_operation() {
    let (store, _, [rows]) = frontends([1]);
    submit(&rows, 4);
    grant(&rows, 4, 0, "node-a").expect("grant");
    give_up(&store, 4, 0, Some(1));

    finish(&rows, 4, 0, "ran").expect("finished");

    let gone = (count(&store, "op/"), count(&store, "queue/"));
    assert_eq!((gone, count(&store, "lease/")), ((0, 0), 0));
    assert_eq!((count(&store, "loss/"), count(&store, "done/")), (0, 1));
}

/// Asserts that a result for a lease the farm gave up, which loses the queue row to another
/// frontend's grant between its read and its write, is stale: the operation runs under
/// the new lease, and nothing of it is finished or removed.
#[test]
fn a_result_that_loses_to_a_grant_of_another_frontend_after_it_read_the_rows_is_stale() {
    let (store, codec, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    give_up(&store, 4, 0, None);
    store.land(&keys::queue(OperationId(4)), another_grant(&codec, 4, 1));

    let late = finish(&first, 4, 0, "late");

    assert!(is_missing(&late), "{late:?}");
    assert_eq!((count(&store, "op/"), count(&store, "lease/")), (1, 1));
    assert_eq!(count(&store, "done/"), 0);
}

/// Asserts that a finish repeated by another frontend (a daemon's result that reaches two
/// of them) is acknowledged as a duplicate, writes nothing, and leaves the first result
/// standing.
#[test]
fn a_finish_repeated_by_another_frontend_is_acknowledged_and_the_first_result_stands() {
    let (store, _, [first, second]) = frontends([1, 2]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    finish(&first, 4, 0, "first").expect("finished");
    let txns = store.commit_stats().txns;

    finish(&second, 4, 0, "second").expect("a duplicate is acknowledged");

    assert_eq!(store.commit_stats().txns, txns, "a duplicate wrote");
    assert_eq!(
        second.progress(OperationId(4)).expect("progress"),
        ended("first")
    );
}

/// Asserts that a result is accepted when another frontend's give-up of the lease lands
/// between its read of the lease row and its write: the operation, queued again by the
/// other, is finished, and its result is the row left.
#[test]
fn a_result_that_loses_the_lease_row_to_a_give_up_of_another_frontend_still_finishes() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    let lease = keys::lease(LeaseId::new(5, 0));
    let given_up = Txn::new().delete(lease.clone(), Cond::Any).put(
        keys::queue(OperationId(4)),
        Vec::new(),
        Cond::Absent,
    );
    store.land(&lease, given_up);

    finish(&first, 4, 0, "ran").expect("finished");

    assert_eq!((count(&store, "op/"), count(&store, "queue/")), (0, 0));
    assert_eq!((count(&store, "lease/"), count(&store, "done/")), (0, 1));
}

/// Asserts that a result is acknowledged as a duplicate when another frontend's finish
/// lands between its read of the lease row and its write: the first result stands.
#[test]
fn a_result_that_loses_the_lease_row_to_a_finish_of_another_frontend_is_a_duplicate() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    let lease = keys::lease(LeaseId::new(5, 0));
    let gone = Txn::new().delete(lease.clone(), Cond::Any);
    store.land(&lease, settled(gone, 4, "other"));

    finish(&first, 4, 0, "mine").expect("a duplicate is acknowledged");

    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        ended("other")
    );
}

/// Asserts that a finish whose queue and operation rows another frontend's finish removed
/// between its read and its write is acknowledged as a duplicate when a result is there,
/// the other's standing.
#[test]
fn a_finish_that_loses_the_rows_to_another_frontends_finish_is_a_duplicate() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    give_up(&store, 4, 0, None);
    store.land(
        &keys::queue(OperationId(4)),
        settled(Txn::new(), 4, "other"),
    );

    finish(&first, 4, 0, "mine").expect("a duplicate is acknowledged");

    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        ended("other")
    );
}

/// Asserts that a refusal of an operation another frontend has granted is stale and changes
/// nothing: the operation is not refused while another frontend runs it.
#[test]
fn a_refusal_of_an_operation_another_frontend_runs_is_stale_and_changes_nothing() {
    let (store, _, [first, second]) = frontends([1, 2]);
    submit(&first, 4);
    grant(&second, 4, 0, "node-b").expect("granted by the other");
    let txns = store.commit_stats().txns;

    let refused = first.refused(OperationId(4), &ending("refused"));

    assert!(is_missing(&refused), "{refused:?}");
    assert_eq!(store.commit_stats().txns, txns, "a refusal wrote");
    assert_eq!((count(&store, "op/"), count(&store, "lease/")), (1, 1));
    assert_eq!(count(&store, "done/"), 0);
}

/// Asserts that a refusal that loses the queue row to another frontend's grant between its
/// read and its write is stale too: the grant stands, and the operation is not refused.
#[test]
fn a_refusal_that_loses_to_a_grant_after_it_read_the_rows_is_stale() {
    let (store, codec, [first]) = frontends([1]);
    submit(&first, 4);
    store.land(&keys::queue(OperationId(4)), another_grant(&codec, 4, 0));

    let refused = first.refused(OperationId(4), &ending("refused"));

    assert!(is_missing(&refused), "{refused:?}");
    assert_eq!((count(&store, "op/"), count(&store, "lease/")), (1, 1));
    assert_eq!(count(&store, "done/"), 0);
}

/// Asserts that a refusal that finds another result for its operation is stale and that
/// result stands, and that a refusal repeated after it was written (the acknowledgement
/// was lost) is acknowledged and writes nothing.
#[test]
fn a_refusal_is_a_duplicate_only_of_itself() {
    let (store, _, [first, second]) = frontends([1, 2]);
    for operation in [4, 5] {
        submit(&first, operation);
    }
    grant(&second, 4, 0, "node-b").expect("granted by the other");
    finish(&second, 4, 0, "ran").expect("finished by the other");

    let late = first.refused(OperationId(4), &ending("refused"));

    assert!(is_missing(&late), "{late:?}");
    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        ended("ran")
    );
    first
        .refused(OperationId(5), &ending("refused"))
        .expect("refused");
    let txns = store.commit_stats().txns;
    first
        .refused(OperationId(5), &ending("refused"))
        .expect("refused again");
    assert_eq!(store.commit_stats().txns, txns, "a repeated refusal wrote");
}

/// Asserts that the grant is conditional on the version of the `queue/` row it read: a
/// frontend that loses the row to another's grant between its read and its write writes
/// nothing of its own, and the other's lease stands.
#[test]
fn a_grant_loses_the_queue_row_to_another_grant() {
    let (store, codec, [first]) = frontends([1]);
    submit(&first, 4);
    store.land(&keys::queue(OperationId(4)), another_grant(&codec, 4, 1));

    let lost = grant(&first, 4, 0, "node-a");

    assert!(
        conflicts_on(&lost, &keys::queue(OperationId(4))),
        "{lost:?}"
    );
    assert_eq!((count(&store, "queue/"), count(&store, "lease/")), (0, 1));
    assert_eq!(count(&store, "lease-by-op/"), 0);
    let again = grant(&first, 4, 0, "node-a");
    assert!(is_missing(&again), "{again:?}");
}

/// Asserts that the grant puts its `lease/` row only where there is none: a row another
/// writer put at the lease's key, with the queue row untouched, is not written over, and the
/// operation stays queued.
#[test]
fn a_grant_does_not_write_over_a_lease_row_another_writer_put() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    let lease = keys::lease(LeaseId::new(5, 0));
    store.land(
        &lease,
        Txn::new().put(lease.clone(), b"later".to_vec(), Cond::Any),
    );

    let lost = grant(&first, 4, 0, "node-a");

    assert!(conflicts_on(&lost, &lease), "{lost:?}");
    assert_eq!((count(&store, "queue/"), count(&store, "lease/")), (1, 1));
    assert_eq!(value_of(&store, &lease), b"later");
}

/// Asserts that a result is conditional on the version of the `lease/` row it read: another
/// writer's change to the row, which is still there, makes the result a conflict and writes
/// nothing; the result can be tried again.
#[test]
fn a_result_loses_a_lease_row_another_writer_rewrote() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    let lease = keys::lease(LeaseId::new(5, 0));
    let same = Txn::new().put(lease.clone(), value_of(&store, &lease), Cond::Any);
    store.land(&lease, same);

    let lost = finish(&first, 4, 0, "ran");

    assert!(conflicts_on(&lost, &lease), "{lost:?}");
    assert_eq!((count(&store, "op/"), count(&store, "lease/")), (1, 1));
    assert_eq!(count(&store, "done/"), 0);
    finish(&first, 4, 0, "ran").expect("tried again");
    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        ended("ran")
    );
}

/// Asserts that the removal of the in-flight row is conditional on the version read: a row
/// another writer rewrote between the read and the write makes the result a conflict and
/// leaves every row of the operation.
#[test]
fn a_result_loses_an_in_flight_row_another_writer_rewrote() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    grant(&first, 4, 0, "node-a").expect("grant");
    let pin = keys::inflight(&request(4).key);
    let same = Txn::new().put(pin.clone(), 4_u64.to_be_bytes(), Cond::Any);
    store.land(&pin, same);

    let lost = finish(&first, 4, 0, "ran");

    assert!(conflicts_on(&lost, &pin), "{lost:?}");
    assert_eq!((count(&store, "op/"), count(&store, "lease/")), (1, 1));
    assert_eq!(count(&store, "done/"), 0);
}

/// Asserts that the result of a queued operation is put only where there is none: a `done/`
/// row another writer put beside rows that did not change is not written over. The refusal is
/// stale and the other's result stands, and the other's own result, tried again, is
/// acknowledged as the duplicate it is.
#[test]
fn a_result_put_beside_the_rows_by_another_writer_is_not_written_over() {
    let (store, _, [first]) = frontends([1]);
    submit(&first, 4);
    let done = keys::done(OperationId(4));
    let put = Txn::new().put(done.clone(), b"other".to_vec(), Cond::Any);
    store.land(&done, put);

    let lost = first.refused(OperationId(4), &ending("mine"));

    assert!(is_missing(&lost), "{lost:?}");
    assert_eq!((count(&store, "op/"), count(&store, "queue/")), (1, 1));
    assert_eq!(value_of(&store, &done), b"other");
    first
        .refused(OperationId(4), &ending("other"))
        .expect("a duplicate of the result there");
    assert_eq!(value_of(&store, &done), b"other");
}

/// Asserts that a result accepted under a lease also removes the rows only the embedder
/// has: the parcel it submitted with goes with the operation, and the parcel kept as long as
/// the result is put, for the retention of the result and at least the floor.
#[test]
fn a_result_removes_the_submitted_parcel_and_keeps_the_ended_one_for_the_floor() {
    let (store, _, [rows]) = frontends([1]);
    submit(&rows, 4);
    grant(&rows, 4, 0, "node-a").expect("grant");
    assert_eq!(count(&store, "parcel/"), 1);

    finish(&rows, 4, 0, "ran").expect("finished");

    assert_eq!(count(&store, "parcel/"), 0);
    let kept = store.get("ended-parcel/0000000000000004").expect("read");
    let kept = kept.expect("the ended parcel");
    let done = store.get(&keys::done(OperationId(4))).expect("read");
    let (kept, done) = (kept.expires_at, done.expect("a result").expires_at);
    assert_eq!(kept, done, "the parcel outlives the result by nothing");
    assert!(done.is_some_and(|at| at.as_duration() >= js_store::DONE_FLOOR));
}

/// Asserts that an operation reads as queued, running, ended or gone as its rows say, and
/// that a result past its retention is gone.
#[test]
fn an_operation_is_queued_running_ended_or_gone_as_its_rows_say() {
    let (store, _, [rows]) = frontends([1]);
    let progress = |operation| rows.progress(OperationId(operation)).expect("progress");
    assert_eq!(progress(4), Progress::Gone);
    assert!(Progress::Gone.is_gone());

    submit(&rows, 4);
    assert_eq!(progress(4), Progress::Queued);
    grant(&rows, 4, 0, "node-a").expect("grant");
    assert_eq!(progress(4), Progress::Running);
    finish(&rows, 4, 0, "ran").expect("finished");
    assert_eq!(progress(4), ended("ran"));
    assert_eq!(
        rows.ended(OperationId(4)).expect("read"),
        Some(b"ran".to_vec())
    );

    store.advance(Duration::from_secs(601));
    store.reap_expired().expect("reaped");
    assert_eq!(progress(4), Progress::Gone);
    assert_eq!(rows.ended(OperationId(4)).expect("read"), None);
}

/// Asserts that the retention of a result is the finished retention, and never less than the
/// floor: an ending with a retention above the floor keeps its row that long, one below it
/// keeps it for the floor.
#[test]
fn a_result_is_kept_for_the_retention_and_at_least_the_floor() {
    let long = Ending::new(Vec::new(), Duration::from_secs(900));
    let short = Ending::new(Vec::new(), Duration::from_secs(1));
    assert_eq!(long.ttl(), Duration::from_secs(900));
    assert_eq!(short.ttl(), js_store::DONE_FLOOR);
}

/// Asserts that the value of a `done/` row is the bytes the embedder gave, whatever they
/// hold: the value is the embedder's, and is kept as it is.
#[test]
fn the_value_of_a_done_row_is_kept_as_it_was_given() {
    let (store, _, [rows]) = frontends([1]);
    submit(&rows, 4);
    let bytes: Vec<u8> = (0..=255).collect();
    let ended = Ending::new(bytes.clone(), Duration::from_secs(1));

    rows.refused(OperationId(4), &ended).expect("refused");

    assert_eq!(value_of(&store, &keys::done(OperationId(4))), bytes);
    assert_eq!(ended.done(), bytes.as_slice());
}
