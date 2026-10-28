//! The bytes of the `fate/`, `floor/`, `keyfloor/`, `loss/` and `queue/` rows a frontend writes
//! when a lease is given up, as a file (`tests/golden/giveup-rows.txt`), and the order of the
//! operations in the transaction of each give-up and of the orphan sweep.
//!
//! The order of the operations in a transaction is the order the farm server writes them in
//! (its trace of the store, `check`, `del` and `put` for each, with the condition and the
//! lifetime): the order is part of what a store written by either server holds to. Where the
//! farm server's recorded traces hold the transaction (a lost lease, a memory kill, a second
//! kill that raises the floor again) the lines below are those traces; the transactions they
//! hold no case of (an evicted floor, a key floor, a give-up whose floors are written alone,
//! the orphan sweep) are in the order of the farm server's code.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_sched::{Fate, Floor, FloorChange, KeyFloor, Request, Requeue, RequeueReason};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Store, Txn, share};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, Orphans, Rows, StateError};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    FarmFault, FarmRun, FarmTime, LeaseGrant, LeaseId, LeaseKind, LostWhy, MemoryKill, MemoryRun,
    OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, dump, frontend_with, only, shape, unhex};

const GOLDEN: &str = include_str!("golden/giveup-rows.txt");

/// The term the frontend opened its store under.
const TERM: u64 = 0x0102_0304_0506_0708;

/// The name the key floor of the golden file gives its size summary.
const SUMMARY: &str = "est/6d61696e/6c696e75782f7838365f3634/c/0123456789abcdef/0000000000000007";

/// The request for the action whose digest is `tag` thirty-two times, of size `tag`: the
/// dedup text is the digest as `<hash>/<size>`, as the farm server's front names an action.
fn action(tag: u8) -> Request {
    let hash = format!("{tag:02x}").repeat(32);
    Request {
        key: WorkKey::new("main", format!("{hash}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn grant_of(operation: u64, seq: u64) -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost {
            cpu_millis: 1_000,
            ..LeaseCost::default()
        },
    }
}

/// The give-up of lease `(5, seq)` of `operation` on node-a for a memory kill.
fn kill(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::OutOfMemory {
            booked: 4 << 30,
            raised: 16 << 30,
        },
    }
}

/// The give-up of lease `(5, seq)` of `operation` on node-a because the node went silent.
fn silent(operation: u64, seq: u64) -> Requeue {
    Requeue {
        reason: RequeueReason::Silent,
        ..kill(operation, seq)
    }
}

/// A fate with every kind of fault and kill in it.
fn fate() -> Fate {
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
        avoid: ["node-a", "node-b"]
            .map(WorkerId::new)
            .into_iter()
            .collect(),
        not_started: 3,
    }
}

/// The floors a memory kill of the action `tag` raises: its floor, and the key floor of the
/// size key 7.
fn raised(tag: u8, stamp: u64) -> FloorChange {
    FloorChange {
        raised: Some(Floor {
            key: action(tag).key,
            ask: 16 << 30,
            stamp,
        }),
        key_raised: Some(KeyFloor {
            key: 7,
            ask: 16 << 30,
            stamp,
        }),
        ..FloorChange::default()
    }
}

/// The lines of `section` of the golden file, without its comments.
fn lines(section: &str) -> String {
    section
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Asserts that a frontend which gives a lease up as the farm server's golden file does
/// leaves exactly the `fate/`, `floor/`, `keyfloor/`, `loss/` and `queue/` rows of the golden
/// file, byte for byte: a version 2 fate with every kind of fault and kill, the floor of the
/// action (class in hexadecimal, ask, stamp, class and digest text), the key floor with the
/// name of its size summary, the loss count and the queue row; none of them expires.
#[test]
fn the_rows_of_a_give_up_are_the_rows_of_the_golden_file() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    share(&*store).expect("the shared marker");
    let (rows, _) = Rows::open(
        store.clone(),
        EpochPolicy::Keep,
        TERM,
        Arc::new(TestCodec::new()),
        Arc::new(TestExt),
    )
    .expect("open");
    rows.reserve_operation(OperationId(4))
        .expect("operation ids");
    rows.reserve_seqs(0).expect("lease sequences");
    rows.submit(OperationId(4), &action(1), &[], Some(7))
        .expect("submit");
    rows.granted(&grant_of(4, 0)).expect("grant");

    rows.requeued(
        &kill(4, 0),
        Some(2),
        Some(&fate()),
        &raised(1, 2),
        Some(SUMMARY),
    )
    .expect("a kill");

    let (written, _) = GOLDEN
        .split_once("# Rows an earlier release wrote")
        .expect("a section of earlier rows");
    let prefixes = ["fate/", "floor/", "keyfloor/", "loss/", "queue/"];
    assert_eq!(dump(&store, &prefixes), lines(written));
}

/// Asserts that a start reads the rows of the golden file back as the scheduler's own values
/// (the fate with its budgets, the floor, the key floor and the name of its summary), and a
/// version 1 fate, which an earlier release wrote, with every budget empty; and that a
/// give-up over a version 1 row replaces it, at the version read, with a version 2 row.
#[test]
fn the_rows_of_the_golden_file_load_and_a_version_1_fate_is_replaced_by_a_version_2() {
    let store = Racing::new();
    let rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    let (written, legacy) = GOLDEN
        .split_once("# Rows an earlier release wrote")
        .expect("a section of earlier rows");
    for (operation, tag) in [(4, 1), (5, 2)] {
        rows.submit(OperationId(operation), &action(tag), &[], Some(7))
            .expect("submit");
    }
    let mut kept = Vec::new();
    for line in lines(written).lines().chain(lines(legacy).lines()) {
        let (key, rest) = line.split_once(' ').expect("a key");
        let (_, value) = rest.split_once("value=").expect("a value");
        kept.push((key.to_owned(), unhex(value)));
    }
    for (key, value) in &kept {
        let put = Txn::new().put(key.clone(), value.clone(), Cond::Any);
        store.txn(put).expect("store").expect("put");
    }

    let stored = rows.load().expect("load");

    assert_eq!(stored.operations[0].fate, Some(fate()));
    assert_eq!(stored.operations[0].losses, 2);
    let version_1 = Fate {
        booked_bytes: 16 << 30,
        memory_runs: vec![MemoryRun {
            worker: WorkerId::new("node-a"),
            booked: 4 << 30,
            kill: MemoryKill::NodePressure,
        }],
        ..Fate::default()
    };
    assert_eq!(stored.operations[1].fate, Some(version_1));
    let floors = raised(1, 2);
    assert_eq!(stored.floors, [floors.raised.expect("a floor")]);
    assert_eq!(stored.key_floors, [floors.key_raised.expect("a key floor")]);
    assert_eq!(stored.key_names, [(7, SUMMARY.to_owned())]);

    rows.granted(&grant_of(5, 1)).expect("grant");
    store.asked();
    let none = FloorChange::default();
    rows.requeued(&silent(5, 1), None, Some(&fate()), &none, None)
        .expect("a give-up over a version 1 fate");
    let replaced = store.get("fate/0000000000000005").expect("read");
    let version_2 = kept
        .iter()
        .find(|(key, _)| key == "fate/0000000000000004")
        .map(|(_, value)| value.clone());
    assert_eq!(Some(replaced.expect("a row").value), version_2);
    let asked = store.asked();
    assert!(
        shape(&asked[0]).contains(&"put fate/0000000000000005 cond=version ttl=-".to_owned()),
        "{:?}",
        shape(&asked[0])
    );
}

/// A frontend over a fresh store, which owns node-a, with operation 4 submitted (of the action
/// 1, with a size key) and granted lease 0 of term 5. The transactions so far are not counted.
fn leased_to_node_a() -> (Arc<Racing>, Rows) {
    leased_with(&Arc::new(TestCodec::new()))
}

/// [`leased_to_node_a`], over rows `codec` encodes (so that a second frontend can read them).
fn leased_with(codec: &Arc<TestCodec>) -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let rows = frontend_with(&store, 1, codec);
    rows.arrive(&WorkerId::new("node-a"), FarmTime::from_millis(0))
        .expect("a Hello");
    let submitted = rows.submit(OperationId(4), &action(1), &[], Some(7));
    submitted.expect("submit");
    rows.granted(&grant_of(4, 0)).expect("grant");
    store.asked();
    (store, rows)
}

const FLOOR_1: &str =
    "floor/6d61696e/0101010101010101010101010101010101010101010101010101010101010101/1";
const FLOOR_2: &str =
    "floor/6d61696e/0202020202020202020202020202020202020202020202020202020202020202/2";

/// The rows of operation 4's lease a give-up checks and takes: the node's owner, the operation,
/// the lease at the version read and its index row.
const LEASE_OF_4: [&str; 4] = [
    "check owner/node-a cond=version",
    "check op/0000000000000004 cond=version",
    "del lease/0000000000000005.0000000000000000 cond=version",
    "del lease-by-op/0000000000000004 cond=any",
];

/// Asserts the order of the operations of the give-up of a lost lease: the owner check, the
/// operation check, the lease's delete at the version read, the index row's delete, the loss
/// count, the fate and the queue row put where there is none (the traces of a silent node and
/// of a restarted daemon: the second has no fate).
#[test]
fn a_lost_lease_is_given_up_in_this_order() {
    let (store, rows) = leased_to_node_a();
    let none = FloorChange::default();
    rows.requeued(&silent(4, 0), Some(1), Some(&fate()), &none, None)
        .expect("given up");
    let mut want: Vec<_> = LEASE_OF_4.map(str::to_owned).to_vec();
    want.extend([
        "put loss/0000000000000004 cond=absent ttl=-".to_owned(),
        "put fate/0000000000000004 cond=absent ttl=-".to_owned(),
        "put queue/0000000000000004 cond=absent ttl=-".to_owned(),
    ]);
    assert_eq!(only(&store), want);

    let (store, rows) = leased_to_node_a();
    rows.requeued(&silent(4, 0), Some(1), None, &none, None)
        .expect("given up");
    let mut want: Vec<_> = LEASE_OF_4.map(str::to_owned).to_vec();
    want.extend([
        "put loss/0000000000000004 cond=absent ttl=-".to_owned(),
        "put queue/0000000000000004 cond=absent ttl=-".to_owned(),
    ]);
    assert_eq!(only(&store), want);
}

/// Asserts the order of the operations of the give-up of a memory kill: the floor first (put
/// where there is none, then over the version read when it is raised again), then the owner
/// check and the rest as for a lost lease, the fate over the version read the second time.
#[test]
fn a_memory_kill_is_given_up_in_this_order() {
    let (store, rows) = leased_to_node_a();
    let floors = FloorChange {
        raised: raised(1, 0).raised,
        ..FloorChange::default()
    };
    rows.requeued(&kill(4, 0), None, Some(&fate()), &floors, None)
        .expect("a kill");
    let mut want = vec![format!("put {FLOOR_1} cond=absent ttl=-")];
    want.extend(LEASE_OF_4.map(str::to_owned));
    want.extend([
        "put fate/0000000000000004 cond=absent ttl=-".to_owned(),
        "put queue/0000000000000004 cond=absent ttl=-".to_owned(),
    ]);
    assert_eq!(only(&store), want);

    rows.granted(&grant_of(4, 1)).expect("grant");
    store.asked();
    rows.requeued(&kill(4, 1), None, Some(&fate()), &floors, None)
        .expect("a second kill");
    let mut want = vec![format!("put {FLOOR_1} cond=version ttl=-")];
    want.extend(
        [
            "check owner/node-a cond=version",
            "check op/0000000000000004 cond=version",
            "del lease/0000000000000005.0000000000000001 cond=version",
            "del lease-by-op/0000000000000004 cond=any",
            "put fate/0000000000000004 cond=version ttl=-",
            "put queue/0000000000000004 cond=absent ttl=-",
        ]
        .map(str::to_owned),
    );
    assert_eq!(only(&store), want);
}

/// Asserts the order of the operations of a give-up that changes every kind of floor: the floor
/// put, the floors forgotten, the key floor put, the key floors forgotten, and then the owner
/// check, the lease's rows, the loss count, the fate and the queue row.
#[test]
fn a_give_up_that_changes_every_floor_writes_them_first_in_this_order() {
    let (store, rows) = leased_to_node_a();
    let change = FloorChange {
        evicted: vec![action(2).key],
        key_evicted: vec![9],
        ..raised(1, 0)
    };

    rows.requeued(&kill(4, 0), Some(1), Some(&fate()), &change, Some(SUMMARY))
        .expect("a kill");

    let mut want = vec![
        format!("put {FLOOR_1} cond=absent ttl=-"),
        format!("del {FLOOR_2} cond=any"),
        "put keyfloor/0000000000000007 cond=absent ttl=-".to_owned(),
        "del keyfloor/0000000000000009 cond=any".to_owned(),
    ];
    want.extend(LEASE_OF_4.map(str::to_owned));
    want.extend([
        "put loss/0000000000000004 cond=absent ttl=-".to_owned(),
        "put fate/0000000000000004 cond=absent ttl=-".to_owned(),
        "put queue/0000000000000004 cond=absent ttl=-".to_owned(),
    ]);
    assert_eq!(only(&store), want);
}

/// Asserts that the floors of a give-up whose lease the store does not hold are written alone,
/// in one transaction with no owner check, and that a give-up that changes no floor and holds no
/// lease writes nothing; and that the floors of a give-up refused as stale for the owner are
/// committed alone, before the refusal, and nothing of the lease is.
#[test]
fn floors_are_written_alone_when_there_is_no_lease_or_the_owner_is_another() {
    let codec = Arc::new(TestCodec::new());
    let (store, rows) = leased_with(&codec);
    let change = FloorChange {
        evicted: vec![action(2).key],
        ..raised(1, 0)
    };
    rows.requeued(&kill(9, 0), None, None, &change, None)
        .expect("an operation that is gone");
    assert_eq!(
        only(&store),
        [
            format!("put {FLOOR_1} cond=absent ttl=-"),
            format!("del {FLOOR_2} cond=any"),
            "put keyfloor/0000000000000007 cond=absent ttl=-".to_owned(),
        ]
    );
    rows.requeued(&kill(9, 0), None, None, &FloorChange::default(), None)
        .expect("nothing to write");
    assert!(store.asked().is_empty(), "a give-up of nothing wrote");

    let taker = frontend_with(&store, 2, &codec);
    taker
        .arrive(&WorkerId::new("node-a"), FarmTime::from_millis(1))
        .expect("moved");
    store.asked();
    let refused = rows.requeued(&kill(4, 0), Some(1), Some(&fate()), &change, None);
    assert!(
        matches!(refused, Err(StateError::StaleOwner(_))),
        "{refused:?}"
    );
    assert_eq!(
        only(&store),
        [
            format!("put {FLOOR_1} cond=version ttl=-"),
            format!("del {FLOOR_2} cond=any"),
            "put keyfloor/0000000000000007 cond=version ttl=-".to_owned(),
        ]
    );
}

/// Asserts the order of the operations of the write of a key floor outside a give-up (after the
/// estimator's decay): one put over whatever is there, or one delete when the floor was
/// forgotten.
#[test]
fn a_key_floor_written_alone_is_one_put_or_one_delete() {
    let (store, rows) = leased_to_node_a();
    let floor = KeyFloor {
        key: 7,
        ask: 1 << 30,
        stamp: 4,
    };

    rows.put_key_floor(&floor, Some(SUMMARY)).expect("written");
    assert_eq!(
        only(&store),
        ["put keyfloor/0000000000000007 cond=any ttl=-"]
    );

    let forgotten = KeyFloor { ask: 0, ..floor };
    rows.put_key_floor(&forgotten, None).expect("deleted");
    assert_eq!(only(&store), ["del keyfloor/0000000000000007 cond=any"]);
}

/// Asserts the order of the operations of the orphan sweep's requeue: the operation checked at
/// the version read, the node's owner row checked (at the version read, or absent when the node
/// was never claimed), the lease deleted at the version read, its index row, the loss count
/// (put where there is none, over the version read after) and the queue row put where there is
/// none.
#[test]
fn an_orphaned_lease_is_requeued_in_this_order() {
    let wait = Duration::from_secs(30);
    let (store, rows) = leased_to_node_a();
    let (gone, nobody) = (Orphans::default(), std::collections::BTreeSet::new());
    let mut seen = gone;
    rows.sweep(wait, &nobody, &mut seen).expect("a look");
    store.advance(wait);
    store.asked();

    rows.sweep(wait, &nobody, &mut seen).expect("a requeue");

    assert_eq!(
        only(&store),
        [
            "check op/0000000000000004 cond=version",
            "check owner/node-a cond=version",
            "del lease/0000000000000005.0000000000000000 cond=version",
            "del lease-by-op/0000000000000004 cond=any",
            "put loss/0000000000000004 cond=absent ttl=-",
            "put queue/0000000000000004 cond=absent ttl=-",
        ]
    );

    rows.granted(&grant_of(4, 1)).expect("grant");
    rows.sweep(wait, &nobody, &mut seen).expect("a look");
    store.advance(wait);
    store.asked();
    rows.sweep(wait, &nobody, &mut seen).expect("a requeue");
    let written = only(&store);
    assert_eq!(written[4], "put loss/0000000000000004 cond=version ttl=-");
}

/// Asserts that the node of an orphaned lease that was never claimed is checked as absent.
#[test]
fn the_owner_of_an_unclaimed_node_is_checked_as_absent() {
    let wait = Duration::from_secs(30);
    let store = Racing::new();
    let rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    rows.submit(OperationId(4), &action(1), &[], None)
        .expect("submit");
    rows.granted(&grant_of(4, 0)).expect("grant");
    let nobody = std::collections::BTreeSet::new();
    let mut seen = Orphans::default();
    rows.sweep(wait, &nobody, &mut seen).expect("a look");
    store.advance(wait);
    store.asked();

    rows.sweep(wait, &nobody, &mut seen).expect("a requeue");

    let written = only(&store);
    assert_eq!(written[1], "check owner/node-a cond=absent");
}
