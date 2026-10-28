//! A lease given up: the operation is queued again, with its loss count and fate, and what
//! the give-up leaves for an operation that has ended meanwhile. The memory floors a give-up
//! raises are in `floors.rs`; the races between frontends are in `transitions.rs` and
//! `owner.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::{Fate, FloorChange, Request, Requeue, RequeueReason};
use js_state::{Cond, Store, Txn};
use js_store::{Ending, Rows, StateError, keys};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    FarmFault, FarmRun, LeaseGrant, LeaseId, LeaseKind, LostWhy, MemoryKill, MemoryRun,
    OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, open};

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

/// A store and a handle on it.
fn handle() -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let (rows, _) = open(store.clone(), js_store::EpochPolicy::Keep, 100).expect("open");
    (store, rows)
}

/// Queues operation `operation` for the work of `tag`.
fn submit(rows: &Rows, operation: u64, tag: u8) {
    let submitted = rows.submit(OperationId(operation), &request(tag), &[], None);
    submitted.expect("submit");
}

/// The grant of lease `(5, seq)` of `operation` to node-a.
fn grant(rows: &Rows, operation: u64, seq: u64) {
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("grant");
}

/// An ending with a result of `message`.
fn ended(message: &str) -> Ending {
    Ending::new(message.as_bytes().to_vec(), Duration::from_secs(1))
}

fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

fn killed(operation: u64, seq: u64) -> Requeue {
    Requeue {
        reason: RequeueReason::OutOfMemory {
            booked: 4 << 30,
            raised: 16 << 30,
        },
        ..requeue(operation, seq)
    }
}

/// `rows` gives the lease `(5, seq)` of `operation` up, for no reason that changes a floor.
fn give_up(rows: &Rows, operation: u64, seq: u64, losses: Option<u32>) {
    let kept = FloorChange::default();
    rows.requeued(&requeue(operation, seq), losses, None, &kept, None)
        .expect("given up");
}

fn count(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

/// A fate with every kind of fault and kill in it.
fn a_fate() -> Fate {
    Fate {
        booked_bytes: 16 << 30,
        memory_runs: vec![
            MemoryRun {
                worker: WorkerId::new("node-a"),
                booked: 4 << 30,
                kill: MemoryKill::OwnLimit { cap: 64 << 30 },
            },
            MemoryRun {
                worker: WorkerId::new("node-b"),
                booked: 16 << 30,
                kill: MemoryKill::NodePressure,
            },
        ],
        farm_runs: vec![
            FarmRun {
                worker: WorkerId::new("node-a"),
                fault: FarmFault::NodeMemoryPressure,
            },
            FarmRun {
                worker: WorkerId::new("node-b"),
                fault: FarmFault::Lost(LostWhy::DaemonRestarted),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Lost(LostWhy::Fenced),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Infra,
            },
        ],
        avoid: ["node-a", "node-b", "node-c"]
            .map(WorkerId::new)
            .into_iter()
            .collect(),
        not_started: 3,
    }
}

/// Catches: a lease given up that stays in the store (a restart re-binds a run nobody
/// holds), an operation not queued again after it, and a give-up that cannot be written
/// again: the scheduler repeats a refused one until the store takes it, so a repeat after
/// it was taken changes nothing, and one for an operation that finished writes nothing.
#[test]
fn a_given_up_lease_requeues_its_operation_once() {
    let (store, rows) = handle();
    submit(&rows, 4, 1);
    grant(&rows, 4, 0);
    give_up(&rows, 4, 0, None);
    let stored = rows.load().expect("load");
    assert_eq!(stored.operations[0].lease, None);
    assert_eq!(count(&store, "queue/"), 1);
    let txns = store.commit_stats().txns;

    give_up(&rows, 4, 0, None);

    assert_eq!(rows.load().expect("load"), stored);
    assert_eq!(store.commit_stats().txns, txns, "a repeat wrote");
    rows.refused(OperationId(4), &ended("refused"))
        .expect("refused");
    give_up(&rows, 4, 0, None);
    assert_eq!(
        count(&store, "queue/"),
        0,
        "a finished operation was queued"
    );
}

/// Catches: a loss count that is not written with the give-up that made it (a restart
/// forgets the losses and a poison action loops), one that a later give-up that is not a
/// loss erases, and one that outlives its operation (a row nobody removes).
#[test]
fn a_loss_count_lives_with_its_operation() {
    let (store, rows) = handle();
    submit(&rows, 4, 1);
    grant(&rows, 4, 0);
    give_up(&rows, 4, 0, Some(2));
    assert_eq!(rows.load().expect("load").operations[0].losses, 2);
    grant(&rows, 4, 1);
    give_up(&rows, 4, 1, None);
    assert_eq!(rows.load().expect("load").operations[0].losses, 2);
    grant(&rows, 4, 2);

    rows.finished(OperationId(4), LeaseId::new(5, 2), &ended("ran"))
        .expect("finished");

    assert_eq!(count(&store, "loss/"), 0);
}

/// Catches: a refused operation whose loss count stays (a row nobody removes).
#[test]
fn a_refusal_removes_the_loss_count() {
    let (store, rows) = handle();
    submit(&rows, 4, 1);
    grant(&rows, 4, 0);
    give_up(&rows, 4, 0, Some(3));
    assert_eq!(count(&store, "loss/"), 1);

    rows.refused(OperationId(4), &ended("lost"))
        .expect("refused");

    assert_eq!(count(&store, "loss/"), 0);
}

/// Catches: a fate that is written apart from the queue row (a crash between them leaves
/// a queued operation at its old booking, or a fate for an operation not queued), one the
/// load does not return with its operation, and one that outlives its operation's end by
/// a finish or by a refusal.
#[test]
fn a_fate_lives_and_dies_with_its_operation() {
    let (store, rows) = handle();
    for operation in [4_u64, 5] {
        let tag = u8::try_from(operation).expect("small") + 1;
        submit(&rows, operation, tag);
        grant(&rows, operation, 0);
        let kept = FloorChange::default();
        rows.requeued(&killed(operation, 0), None, Some(&a_fate()), &kept, None)
            .expect("a kill");
    }
    let stored = rows.load().expect("load");
    assert!(stored.operations.iter().all(|op| op.fate == Some(a_fate())));
    assert_eq!(count(&store, "fate/"), 2);

    // Finished by a result, and refused: neither leaves the row.
    grant(&rows, 4, 1);
    rows.finished(OperationId(4), LeaseId::new(5, 1), &ended("ran"))
        .expect("finished");
    rows.refused(OperationId(5), &ended("no worker"))
        .expect("refused");
    assert_eq!(count(&store, "fate/"), 0);
}

/// Catches: a fate too large to write encoded anyway (a truncated length), a requeue whose
/// fate cannot be written reported as success or leaving the queue row it was written with,
/// and a load that skips a fate row it cannot read instead of refusing the store.
#[test]
fn a_fate_that_cannot_be_written_or_read_is_refused() {
    let (store, rows) = handle();
    submit(&rows, 4, 5);
    grant(&rows, 4, 0);
    let mut huge = a_fate();
    huge.memory_runs[0].worker = WorkerId::new("n".repeat(usize::from(u16::MAX) + 1));

    let kept = FloorChange::default();
    let error = rows
        .requeued(&killed(4, 0), None, Some(&huge), &kept, None)
        .expect_err("too large");

    assert!(error.to_string().contains("fate/0000000000000004"));
    assert!(matches!(error, StateError::Corrupt { .. }), "{error:?}");
    assert_eq!(count(&store, "queue/"), 0, "nothing of the requeue landed");
    let put = Txn::new().put("fate/0000000000000004", vec![3], Cond::Any);
    store.txn(put).expect("store").expect("put");
    let error = rows.load().expect_err("a fate of another version");
    assert!(error.to_string().contains("not a version 1 or 2 fate"));
}

/// Catches: a floor, a loss count or a fate written for a lease the store does not hold: a
/// grant the scheduler gave up before the store took it has no lease row, the operation is
/// still queued there, and nothing of that lease is written, so a restart does not count it.
#[test]
fn a_lease_the_store_never_held_writes_no_loss_and_no_fate() {
    let (store, rows) = handle();
    submit(&rows, 4, 1);
    let txns = store.commit_stats().txns;

    let kept = FloorChange::default();
    rows.requeued(&killed(4, 0), Some(1), Some(&a_fate()), &kept, None)
        .expect("nothing to give up");

    assert_eq!(store.commit_stats().txns, txns, "a give-up wrote");
    assert_eq!((count(&store, "loss/"), count(&store, "fate/")), (0, 0));
    assert_eq!(count(&store, "queue/"), 1);
}

/// Catches: a loss count or a fate written over a row another frontend wrote between the
/// read and the write: the give-up is refused whole, and written again from the rows as they
/// are, the newest fate standing.
#[test]
fn a_loss_count_and_a_fate_are_written_at_the_version_read() {
    let (store, rows) = handle();
    submit(&rows, 4, 1);
    grant(&rows, 4, 0);
    let loss = keys::loss(OperationId(4));
    let other = Txn::new().put(loss.clone(), 7_u64.to_be_bytes(), Cond::Any);
    store.land(&loss, other);

    let kept = FloorChange::default();
    let lost = rows.requeued(&killed(4, 0), Some(8), Some(&a_fate()), &kept, None);

    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    assert_eq!(count(&store, "queue/"), 0, "a lost give-up wrote");
    let fate = keys::fate(OperationId(4));
    let other = Txn::new().put(fate.clone(), vec![9], Cond::Any);
    store.land(&fate, other);
    let lost = rows.requeued(&killed(4, 0), Some(8), Some(&a_fate()), &kept, None);
    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    assert_eq!(count(&store, "queue/"), 0, "a lost give-up wrote");

    rows.requeued(&killed(4, 0), Some(8), Some(&a_fate()), &kept, None)
        .expect("written again from the rows as they are");
    assert_eq!(count(&store, "queue/"), 1);
    let stored = rows.load().expect("load");
    assert_eq!(stored.operations[0].losses, 8);
    assert_eq!(stored.operations[0].fate, Some(a_fate()));
}
