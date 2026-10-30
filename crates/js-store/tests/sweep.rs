//! The orphan sweep: a lease whose node no frontend hears is requeued once two looks at the
//! store, [`ORPHAN_WAIT`] apart on the store's clock, found it so. Each test stands a handle
//! over a store whose clock the test moves.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{
    ACK_WINDOW, ORPHAN_WAIT, Orphans, Rows, SESSION_RENEWAL, StateError, WorkerUp, keys,
};
use js_types::sched::fence::LEASE_GRACE;
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

const WAIT: Duration = Duration::from_secs(30);

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

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

fn lease_row_key() -> String {
    keys::lease(LeaseId::new(5, 0))
}

/// Queues operation `operation` for the work of `tag` and grants it lease `(5, seq)` on
/// `worker`.
fn leased_on(rows: &Rows, operation: u64, tag: u8, seq: u64, worker: &str) {
    let submitted = rows.submit(OperationId(operation), &request(tag), &[], None);
    submitted.expect("submit");
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("a grant");
}

/// A store holding operation 4, leased to `node-a` under lease `(5, 0)`, the codec its
/// handles share, and a handle.
fn leased() -> (Arc<Racing>, Arc<TestCodec>, Rows) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows = frontend_with(&store, 100, &codec);
    leased_on(&rows, 4, 1, 0, "node-a");
    (store, codec, rows)
}

fn nobody() -> BTreeSet<WorkerId> {
    BTreeSet::new()
}

fn count(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("deleted");
}

/// The registration of `node-a` by the daemon process `daemon`.
fn up(daemon: &str) -> WorkerUp {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    WorkerUp {
        worker: node(),
        instance: DaemonInstance::new(daemon),
        predecessor_stopped: false,
        capacity: Resources::new(2_000, 1 << 30),
        caps,
    }
}

/// Asserts the margins the sweep's wait rests on: the wait is a renewal interval of the session
/// row and the fence's margin, and a frontend acknowledges heartbeats for the part of the
/// row's life the daemon's work does not take.
#[test]
fn the_wait_and_the_ack_window_are_the_renewal_interval_and_the_fence_margins() {
    assert_eq!(SESSION_RENEWAL, Duration::from_secs(20));
    assert_eq!(ORPHAN_WAIT, Duration::from_secs(25));
    assert_eq!(ACK_WINDOW, Duration::from_secs(30));
}

/// Asserts that a lease whose node has no session row is requeued by the second look that
/// finds it so, [`WAIT`] after the first on the store's clock and not before: the lease and
/// its index row are gone, the operation is queued, and the loss is counted once. Nothing is
/// left to sweep afterwards.
#[test]
fn a_lease_without_a_session_is_requeued_at_the_second_look_after_the_wait() {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();

    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1, "the first look remembers the lease");
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT - Duration::from_secs(1));
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    store.advance(Duration::from_secs(1));
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (0, 1));
    assert_eq!(count(&store, "lease-by-op/"), 0);
    assert_eq!(
        value_of(&store, "loss/0000000000000004"),
        1u64.to_be_bytes()
    );
    assert!(seen.is_empty(), "a requeued lease is remembered");
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(count(&store, "queue/"), 1);
}

/// Asserts that a wait of nothing still takes two looks: the first only remembers the lease.
#[test]
fn the_first_look_at_a_lease_never_requeues_it() {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();

    rows.sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    rows.sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (0, 1));
}

/// Asserts that a lease whose node has a session row, or is connected to the frontend that
/// looks, is left alone whatever the store's clock says, and that the lease of a node whose
/// session expires is remembered from the look that finds it so.
#[test]
fn a_lease_whose_node_is_heard_is_left_alone() {
    let (store, _, rows) = leased();
    rows.session(&up("daemon")).expect("a session");
    let mut seen = Orphans::default();

    store.advance(WAIT / 2);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT / 2 - Duration::from_secs(1));
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert!(seen.is_empty(), "a node with a session was remembered");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    store.advance(LEASE_GRACE);
    let connected = BTreeSet::from([node()]);
    rows.sweep(WAIT, &connected, &mut seen).expect("a look");
    rows.sweep(WAIT, &connected, &mut seen).expect("a look");
    assert!(seen.is_empty(), "a connected node was remembered");
    assert_eq!(count(&store, "session/"), 0, "the session expired");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1);
}

/// Asserts that a lease remembered as an orphan is forgotten when its node is heard again.
#[test]
fn a_lease_whose_node_comes_back_is_forgotten() {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1);

    rows.session(&up("daemon")).expect("a session");
    store.advance(WAIT);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert!(seen.is_empty());
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));
}

/// Asserts that the wait starts over when the lease row is another write than the one first
/// seen: the lease is not requeued by a look made a wait after the first.
#[test]
fn a_lease_that_changed_starts_the_wait_over() {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    let same = value_of(&store, &lease_row_key());
    overwrite(&store, &lease_row_key(), same);

    store.advance(WAIT);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    store.advance(WAIT);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (0, 1));
}

/// Asserts that the wait starts over when a daemon has said `Hello` to a frontend since the
/// first look (the node's owner row is another write), and that the requeue is then made
/// with that owner row checked.
#[test]
fn a_node_claimed_since_the_first_look_starts_the_wait_over() {
    let (store, codec, rows) = leased();
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    let claimer = frontend_with(&store, 9, &codec);
    claimer
        .arrive(&node(), FarmTime::from_millis(0))
        .expect("claimed");

    store.advance(WAIT);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));
    store.advance(WAIT);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (0, 1));
}

/// Asserts that a second look, whose requeue meets a write of another frontend of the bytes
/// `value` makes of the row `key` between the look and the transaction, reports nothing,
/// leaves the lease and its index row where they are, counts no loss, and forgets the lease,
/// so the wait starts over.
fn assert_requeue_loses_to_a_write(key: &str, value: impl FnOnce(&Racing) -> Vec<u8>) {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT);
    let landing = Txn::new().put(key, value(store.as_ref()), Cond::Any);
    store.land(key, landing);

    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert!(seen.is_empty(), "the lease was remembered");
    assert_eq!(count(&store, "lease/"), 1);
    assert_eq!(count(&store, "lease-by-op/"), 1);
    assert_eq!(count(&store, "loss/"), 0);
}

/// Asserts that a requeue loses to a write of another frontend to the lease row between the
/// look and the transaction: nothing is written, and the sweep reports no error.
#[test]
fn a_requeue_that_loses_the_lease_row_to_another_write_changes_nothing() {
    let same = |store: &Racing| value_of(store, &lease_row_key());
    assert_requeue_loses_to_a_write(&lease_row_key(), same);
}

/// Asserts that a requeue loses to a write of another frontend to the operation's row (the
/// operation changed since the look): nothing is written.
#[test]
fn a_requeue_that_loses_the_operation_row_to_another_write_changes_nothing() {
    let same = |store: &Racing| value_of(store, "op/0000000000000004");
    assert_requeue_loses_to_a_write("op/0000000000000004", same);
}

/// Asserts that a requeue loses to the first claim of the node by a frontend, which writes
/// the node's owner row where the look found none: nothing is written.
#[test]
fn a_requeue_that_loses_the_owner_row_to_a_claim_changes_nothing() {
    let claim = |_: &Racing| [2_u64.to_be_bytes(), 1_u64.to_be_bytes()].concat();
    assert_requeue_loses_to_a_write("owner/node-a", claim);
}

/// Asserts that a requeue loses to a write of another frontend to the operation's queue row,
/// which the requeue writes where there is none: nothing is written.
#[test]
fn a_requeue_that_loses_the_queue_row_to_another_write_changes_nothing() {
    assert_requeue_loses_to_a_write("queue/0000000000000004", |_| Vec::new());
}

/// Asserts that a lease whose requeue lost to another write does not stop the look: the leases
/// after it are requeued by the same look.
#[test]
fn a_lost_requeue_does_not_stop_the_look_at_the_other_leases() {
    let (store, _, rows) = leased();
    leased_on(&rows, 5, 2, 1, "node-b");
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 2);
    store.advance(WAIT);
    let same = value_of(&store, &lease_row_key());
    store.land(
        &lease_row_key(),
        Txn::new().put(lease_row_key(), same, Cond::Any),
    );

    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 1));
    assert!(seen.is_empty(), "a lease was remembered");
}

/// Asserts that a requeue the store cannot take is reported and leaves the lease remembered,
/// so the next look that reaches the store requeues it at once.
#[test]
fn a_requeue_the_store_cannot_take_is_tried_again_by_the_next_look() {
    let (store, _, rows) = leased();
    let mut seen = Orphans::default();
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT);

    store.set_down(true);
    let failed = rows.sweep(WAIT, &nobody(), &mut seen);
    assert!(matches!(failed, Err(StateError::Store(_))), "{failed:?}");
    assert_eq!(seen.len(), 1, "the lease was forgotten");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));

    store.set_down(false);
    rows.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (0, 1));
}

/// Asserts that the loss the sweep counts is added to the count the store holds, and that a
/// lease whose operation has no row any more (it finished) puts nothing in the queue.
#[test]
fn the_loss_is_added_to_the_count_and_a_finished_operation_is_not_queued() {
    let (store, _, rows) = leased();
    overwrite(&store, "loss/0000000000000004", 2u64.to_be_bytes().to_vec());
    let mut seen = Orphans::default();
    for _ in 0..2 {
        rows.sweep(Duration::ZERO, &nobody(), &mut seen)
            .expect("a look");
    }
    assert_eq!(
        value_of(&store, "loss/0000000000000004"),
        3u64.to_be_bytes()
    );

    let (store, _, rows) = leased();
    remove(&store, "op/0000000000000004");
    let mut seen = Orphans::default();
    for _ in 0..2 {
        rows.sweep(Duration::ZERO, &nobody(), &mut seen)
            .expect("a look");
    }
    assert_eq!((count(&store, "lease/"), count(&store, "queue/")), (1, 0));
    assert_eq!(count(&store, "loss/"), 0);
}

/// Asserts that a lease row the scheduler did not write fails the look and changes nothing.
#[test]
fn a_lease_row_that_cannot_be_read_fails_the_look() {
    let (store, _, rows) = leased();
    overwrite(&store, &lease_row_key(), vec![0xff]);

    let failed = rows.sweep(WAIT, &nobody(), &mut Orphans::default());

    assert!(
        matches!(failed, Err(StateError::Corrupt { .. })),
        "{failed:?}"
    );
    assert_eq!(count(&store, "queue/"), 0);
}
