//! The memory floors and key floors: written with the give-up that raises them, never lowered
//! by a later writer, forgotten with the give-up that evicts them, and read at a start.

mod common;

use std::sync::Arc;

use js_sched::{Floor, FloorChange, KeyFloor, Request, Requeue, RequeueReason};
use js_state::{Cond, Store, Txn};
use js_store::test_support::TestCodec;
use js_store::{Ending, Rows, StateError, keys};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with, open};

fn request(tag: u8) -> Request {
    Request {
        key: key_of(tag),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn key_of(tag: u8) -> WorkKey {
    WorkKey::new("main", format!("{tag:02x}/{tag}"))
}

/// A store and a handle on it.
fn handle() -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let (rows, _) = open(store.clone(), js_store::EpochPolicy::Keep, 100).expect("open");
    (store, rows)
}

/// The lease `(5, seq)` of `operation` given up because the action passed its own limit.
fn killed(operation: u64, seq: u64) -> Requeue {
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

fn a_floor(tag: u8, stamp: u64) -> Floor {
    Floor {
        key: key_of(tag),
        ask: 16 << 30,
        stamp,
    }
}

fn a_key_floor(key: u64, stamp: u64) -> KeyFloor {
    KeyFloor {
        key,
        ask: 16 << 30,
        stamp,
    }
}

/// A change that raises the floor of the work `tag` at `stamp` and forgets the floors of the
/// works `evicted`.
fn raising(tag: u8, stamp: u64, evicted: &[u8]) -> FloorChange {
    FloorChange {
        raised: Some(a_floor(tag, stamp)),
        evicted: evicted.iter().map(|tag| key_of(*tag)).collect(),
        ..FloorChange::default()
    }
}

/// `change`, written by the give-up of an operation that is not in the store.
fn write(rows: &Rows, change: &FloorChange) {
    write_named(rows, change, None);
}

/// [`write`], naming the size summary of a raised key floor.
fn write_named(rows: &Rows, change: &FloorChange, summary: Option<&str>) {
    rows.requeued(&killed(7, 0), None, None, change, summary)
        .expect("an operation that is gone");
}

fn count(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

fn put(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// The value of the `floor/` row `change` makes in a store of its own, which a test lands in
/// another as a write of another frontend.
fn floor_row_of(change: &FloorChange) -> Vec<u8> {
    let (store, rows) = handle();
    write(&rows, change);
    value_of(
        &store,
        &keys::floor(&change.raised.as_ref().expect("a floor").key),
    )
}

/// [`floor_row_of`] for a key floor.
fn key_floor_row_of(floor: &KeyFloor, summary: Option<&str>) -> Vec<u8> {
    let (store, rows) = handle();
    rows.put_key_floor(floor, summary).expect("written");
    value_of(&store, &keys::key_floor(floor.key))
}

/// Queues operation `operation` for the work of `tag`, with a size key if `size_key` is
/// given, and grants it lease `(5, seq)`.
fn leased(rows: &Rows, operation: u64, tag: u8, seq: u64, size_key: Option<u64>) {
    let submitted = rows.submit(OperationId(operation), &request(tag), &[], size_key);
    submitted.expect("submit");
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("grant");
}

fn ended() -> Ending {
    Ending::new(b"ended".to_vec(), std::time::Duration::from_secs(1))
}

/// Catches: a floor written apart from the give-up that raised it, one that is not loaded
/// back whole (ask, stamp, key), a raise of the same action that adds a second row, an
/// evicted floor whose row stays, and a floor that ends with the operation that raised it
/// (finishing or refusing it). Several floors load in stamp order.
#[test]
fn a_floor_is_written_with_the_requeue_and_outlives_its_operation() {
    let (store, rows) = handle();
    for tag in [1, 2] {
        leased(&rows, u64::from(tag), tag, u64::from(tag) * 10, None);
    }
    let kill = |operation: u64, seq: u64, change: &FloorChange| {
        let given_up = rows.requeued(&killed(operation, seq), None, None, change, None);
        given_up.expect("a kill");
    };
    kill(1, 10, &raising(1, 0, &[]));
    assert_eq!(rows.load().expect("load").floors, vec![a_floor(1, 0)]);
    assert_eq!(count(&store, "queue/"), 1);

    // Raised again under a later stamp: the same row. A second action: a second row.
    leased_again(&rows, 1, 11);
    kill(1, 11, &raising(1, 2, &[]));
    kill(2, 20, &raising(2, 3, &[]));
    assert_eq!(
        rows.load().expect("load").floors,
        vec![a_floor(1, 2), a_floor(2, 3)]
    );

    // Action 3's raise forgets action 1 in the same write.
    kill(2, 20, &raising(3, 4, &[1]));
    let floors = rows.load().expect("load").floors;
    assert_eq!(floors, vec![a_floor(2, 3), a_floor(3, 4)]);

    // Neither ending an operation nor refusing it takes a floor.
    rows.refused(OperationId(2), &ended()).expect("refused");
    leased_again(&rows, 1, 12);
    rows.finished(OperationId(1), LeaseId::new(5, 12), &ended())
        .expect("finished");
    assert_eq!(count(&store, "floor/"), 2);
}

/// Grants operation `operation`, which is queued, lease `(5, seq)`.
fn leased_again(rows: &Rows, operation: u64, seq: u64) {
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("grant");
}

/// Catches: a floor lost because its operation ended before the requeue was written (the
/// write returns early, as it does for a finished operation), and a delete of a forgotten
/// floor lost the same way. A forgotten key that is raised again in the same change keeps
/// its row (a delete after the put would take it).
#[test]
fn a_floor_is_written_though_its_operation_has_ended() {
    let (store, rows) = handle();
    write(&rows, &raising(1, 0, &[]));
    assert_eq!(count(&store, "floor/"), 1);
    assert_eq!(count(&store, "queue/"), 0);

    write(&rows, &raising(2, 1, &[1]));
    assert_eq!(rows.load().expect("load").floors, vec![a_floor(2, 1)]);

    write(&rows, &raising(2, 2, &[2]));
    assert_eq!(rows.load().expect("load").floors, vec![a_floor(2, 2)]);

    write(&rows, &FloorChange::default());
    assert_eq!(count(&store, "floor/"), 1);

    let evicted_only = FloorChange {
        evicted: vec![key_of(2)],
        ..FloorChange::default()
    };
    write(&rows, &evicted_only);
    assert_eq!(count(&store, "floor/"), 0);
}

/// Catches: a floor row read as anything but what was written: another version, a row cut
/// short at each field, a class or a dedup text that is not text; and a floor too large to
/// write. The start refuses the store naming the row.
#[test]
fn a_floor_row_that_is_not_one_is_refused() {
    let good = floor_row_of(&raising(1, 5, &[]));
    let mut other = good.clone();
    other[0] = 2;
    let mut class = good.clone();
    class[19] = 0xff;
    let mut dedup = good.clone();
    *dedup.last_mut().expect("a row") = 0xff;
    let cases = [
        (other, "not a version 1 floor"),
        (Vec::new(), "the row ends early"),
        (good[..8].to_vec(), "the row ends early"),
        (good[..17].to_vec(), "the row ends early"),
        (good[..18].to_vec(), "the row ends early"),
        (good[..20].to_vec(), "the row ends early"),
        (class, "a class name is not text"),
        (dedup, "a dedup text is not text"),
    ];
    for (bad, why) in cases {
        let (store, rows) = handle();
        put(&store, &keys::floor(&key_of(1)), bad);
        let text = rows.load().expect_err(why).to_string();
        assert!(text.contains("floor/") && text.contains(why), "{text}");
    }

    let (store, rows) = handle();
    let long = Floor {
        key: WorkKey::new("i".repeat(usize::from(u16::MAX) + 1), "x/1"),
        ..a_floor(1, 5)
    };
    let change = FloorChange {
        raised: Some(long),
        ..FloorChange::default()
    };
    let error = rows
        .requeued(&killed(1, 0), None, None, &change, None)
        .expect_err("too large");
    assert!(error.to_string().contains("too large"), "{error}");
    assert_eq!(count(&store, "floor/"), 0);
}

/// Catches: a floor store the start loads on trust. A row whose key is not the key of the
/// floor in it (a copied or edited row would floor another action) is refused naming the row.
#[test]
fn a_floor_store_that_contradicts_itself_is_refused() {
    let good = floor_row_of(&raising(1, 5, &[]));
    let (store, rows) = handle();
    put(&store, &keys::floor(&key_of(2)), good);

    let text = rows.load().expect_err("under another key").to_string();

    assert!(text.contains("floor/"), "{text}");
    assert!(text.contains("not the key of the floor it holds"), "{text}");
}

/// Catches: floors that carry one stamp, which frontends that numbered their raises alone
/// write, loading in an order or with stamps that lose which was raised first, and floors with
/// stamps of their own losing them.
#[test]
fn floors_that_share_a_stamp_load_in_order_with_distinct_stamps() {
    let (_, rows) = handle();
    for (tag, stamp) in [(1, 5), (2, 5), (3, 9)] {
        write(&rows, &raising(tag, stamp, &[]));
    }
    for key in 1..=3 {
        let change = FloorChange {
            key_raised: Some(a_key_floor(key, 2)),
            ..FloorChange::default()
        };
        write(&rows, &change);
    }

    let stored = rows.load().expect("load");

    let stamps: Vec<u64> = stored.floors.iter().map(|floor| floor.stamp).collect();
    assert_eq!(stamps, [5, 6, 9], "a stamp of its own moved");
    let works: Vec<_> = stored
        .floors
        .iter()
        .map(|floor| floor.key.clone())
        .collect();
    assert_eq!(
        works,
        [key_of(1), key_of(2), key_of(3)],
        "ties are in key order"
    );
    let key_stamps: Vec<u64> = stored.key_floors.iter().map(|floor| floor.stamp).collect();
    assert_eq!(key_stamps, [2, 3, 4]);
    let order: Vec<u64> = stored.key_floors.iter().map(|floor| floor.key).collect();
    assert_eq!(order, [1, 2, 3], "ties are kept in key order");
}

/// Asserts that a floor is never lowered by a later writer: a second frontend whose kills
/// raised the floor less far leaves the row at the larger ask (with its own stamp), a raise
/// further than the row's is written, and a write that finds the row changed between its read
/// and its write loses, and written again from the row as it stands keeps the larger ask.
#[test]
fn a_floor_row_keeps_the_larger_ask_and_loses_to_a_write_in_between() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let (first, second) = (
        frontend_with(&store, 1, &codec),
        frontend_with(&store, 2, &codec),
    );
    let raise = |ask: u64, stamp: u64| FloorChange {
        raised: Some(Floor {
            key: key_of(1),
            ask,
            stamp,
        }),
        ..FloorChange::default()
    };
    let stored = |rows: &Rows| rows.load().expect("load").floors[0].clone();
    write(&first, &raise(16 << 30, 1));

    write(&second, &raise(8 << 30, 9));
    let held = stored(&second);
    assert_eq!((held.ask, held.stamp), (16 << 30, 9), "a floor was lowered");

    write(&second, &raise(32 << 30, 10));
    assert_eq!(stored(&first).ask, 32 << 30);

    let row = keys::floor(&key_of(1));
    let other = floor_row_of(&raise(64 << 30, 2));
    store.land(&row, Txn::new().put(row.clone(), other, Cond::Any));
    let lost = second.requeued(&killed(7, 0), None, None, &raise(48 << 30, 11), None);
    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    assert_eq!(stored(&first).ask, 64 << 30);

    write(&second, &raise(48 << 30, 11));
    let held = stored(&first);
    assert_eq!(
        (held.ask, held.stamp),
        (64 << 30, 11),
        "a floor was lowered"
    );
}

/// Asserts that a give-up that raises a key floor writes the larger of its ask and the row's,
/// keeps its own stamp, keeps the name of the size summary the row already had, and loses to
/// a write of another frontend between its read of the row and its write.
#[test]
fn a_key_floor_row_keeps_the_larger_ask_and_its_name() {
    let (store, rows) = handle();
    rows.put_key_floor(&a_key_floor(9, 1), Some("est/x"))
        .expect("written");
    let raise = |ask: u64, stamp: u64| FloorChange {
        key_raised: Some(KeyFloor { key: 9, ask, stamp }),
        ..FloorChange::default()
    };
    let stored = || {
        let stored = rows.load().expect("load");
        (stored.key_floors[0].clone(), stored.key_names.clone())
    };

    write(&rows, &raise(8 << 30, 9));
    let (held, names) = stored();
    assert_eq!((held.ask, held.stamp), (16 << 30, 9));
    assert_eq!(names, [(9, "est/x".to_owned())]);

    write(&rows, &raise(32 << 30, 10));
    assert_eq!(stored().0.ask, 32 << 30);

    let row = keys::key_floor(9);
    let other = key_floor_row_of(
        &KeyFloor {
            key: 9,
            ask: 64 << 30,
            stamp: 2,
        },
        Some("est/y"),
    );
    store.land(&row, Txn::new().put(row.clone(), other, Cond::Any));
    let lost = rows.requeued(&killed(7, 0), None, None, &raise(48 << 30, 11), None);
    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    write(&rows, &raise(48 << 30, 11));
    let (held, names) = stored();
    assert_eq!(held.ask, 64 << 30, "a key floor was lowered");
    assert_eq!(
        names,
        [(9, "est/y".to_owned())],
        "the other's name was lost"
    );
}

/// Asserts that a give-up that raises a floor, and one that raises a key floor, write over a
/// row they cannot read: it counts for nothing, and the row holds the floor raised.
#[test]
fn a_floor_row_that_cannot_be_read_is_written_over_by_a_raise() {
    let (store, rows) = handle();
    put(&store, &keys::floor(&key_of(1)), vec![9]);
    put(&store, &keys::key_floor(9), vec![9]);
    let change = FloorChange {
        raised: Some(Floor {
            key: key_of(1),
            ask: 8 << 30,
            stamp: 3,
        }),
        key_raised: Some(KeyFloor {
            key: 9,
            ask: 4 << 30,
            stamp: 4,
        }),
        ..FloorChange::default()
    };

    write(&rows, &change);

    let stored = rows.load().expect("load");
    assert_eq!(stored.floors, [change.raised.expect("raised")]);
    assert_eq!(stored.key_floors, [change.key_raised.expect("raised")]);
}

/// Catches: a key floor not written with the requeue that raises it (a successor would book
/// the old size again), one lost because its operation ended first, a forgotten key not
/// deleted (or deleted after being raised again), a change that only forgets a key writing
/// nothing, key floors read in another order than written, and a key floor row confused
/// with a digest floor's (an older binary reads the `floor/` prefix and must never meet one).
#[test]
fn a_key_floor_is_written_with_the_requeue_and_forgotten_with_its_eviction() {
    let (store, rows) = handle();
    leased(&rows, 1, 1, 10, Some(7));
    let change = FloorChange {
        raised: Some(a_floor(1, 0)),
        key_raised: Some(a_key_floor(7, 0)),
        ..FloorChange::default()
    };
    rows.requeued(&killed(1, 10), None, None, &change, None)
        .expect("a kill");
    let stored = rows.load().expect("load");
    assert_eq!(stored.key_floors, vec![a_key_floor(7, 0)]);
    assert_eq!(stored.floors, vec![a_floor(1, 0)]);
    assert_eq!(stored.operations[0].size_key, Some(7));
    assert_eq!(count(&store, "floor/"), 1, "one digest floor row");
    assert_eq!(count(&store, "keyfloor/"), 1);

    // Raised again, with another key: two rows; the operation has ended by now.
    let both = FloorChange {
        key_raised: Some(a_key_floor(8, 1)),
        ..FloorChange::default()
    };
    write(&rows, &both);
    assert_eq!(
        rows.load().expect("load").key_floors,
        vec![a_key_floor(7, 0), a_key_floor(8, 1)]
    );

    // Key 9 forgets key 7; key 8 is forgotten and raised again in one change, and keeps its
    // row.
    let forgetting = FloorChange {
        key_raised: Some(a_key_floor(8, 3)),
        key_evicted: vec![7, 8],
        ..FloorChange::default()
    };
    write(&rows, &forgetting);
    assert_eq!(
        rows.load().expect("load").key_floors,
        vec![a_key_floor(8, 3)]
    );
    // A change with nothing to write writes nothing.
    write(&rows, &FloorChange::default());
    assert_eq!(count(&store, "keyfloor/"), 1);
    // A change that only forgets a key deletes its row.
    let forgotten = FloorChange {
        key_evicted: vec![8],
        ..FloorChange::default()
    };
    write(&rows, &forgotten);
    assert_eq!(count(&store, "keyfloor/"), 0);
}

/// Catches: a key floor row read as anything but what was written: another version, a row cut
/// short or with bytes after it, a name that is not text, and a row under another key's name.
#[test]
fn a_key_floor_row_that_is_not_one_is_refused() {
    let named = key_floor_row_of(&a_key_floor(7, 5), Some("est/6d61696e/c/1"));
    let bare = key_floor_row_of(&a_key_floor(7, 5), None);
    let mut other = named.clone();
    other[0] = 2;
    let mut longer = named.clone();
    longer.push(0);
    let mut mislen = bare.clone();
    mislen[25..27].copy_from_slice(&5_u16.to_be_bytes());
    let mut text = named.clone();
    *text.last_mut().expect("a row") = 0xff;
    let mut cases = vec![
        (other, "not a version 1 key floor"),
        (longer, "the row is not the length it says"),
        (mislen, "the row is not the length it says"),
        (text, "a summary name is not text"),
    ];
    for cut in 0..bare.len() {
        cases.push((named[..cut].to_vec(), "the row"));
    }
    cases.push((
        named[..named.len() - 1].to_vec(),
        "the row is not the length",
    ));
    for (bad, why) in cases {
        let (store, rows) = handle();
        put(&store, &keys::key_floor(7), bad);
        let text = rows.load().expect_err(why).to_string();
        assert!(text.contains("keyfloor/") && text.contains(why), "{text}");
    }

    let (store, rows) = handle();
    put(&store, &keys::key_floor(8), named);
    let error = rows.load().expect_err("under another key's name");
    assert!(error.to_string().contains("not the key"), "{error}");
}

/// Asserts that a write that does not know the size summary a key floor decays with leaves
/// the name its row already holds, so a floor raised by a path that cannot name it does not
/// lose the name; a row never named stays unnamed; a key floor whose ask is zero is gone.
#[test]
fn a_write_that_does_not_know_the_summary_keeps_the_name_the_row_has() {
    let (store, rows) = handle();
    rows.put_key_floor(&a_key_floor(7, 0), Some("est/x"))
        .expect("write");
    rows.put_key_floor(&a_key_floor(7, 1), None).expect("write");
    let stored = rows.load().expect("load");
    assert_eq!(stored.key_floors, vec![a_key_floor(7, 1)]);
    assert_eq!(stored.key_names, [(7, "est/x".to_owned())]);

    rows.put_key_floor(&a_key_floor(8, 2), None).expect("write");
    assert_eq!(rows.load().expect("load").key_names.len(), 1);

    let forgotten = KeyFloor {
        ask: 0,
        ..a_key_floor(7, 3)
    };
    rows.put_key_floor(&forgotten, None).expect("write");
    assert_eq!(count(&store, "keyfloor/"), 1);
}

/// A summary name too long for the row is left out of it: the row still reads back, so a
/// requeue is never refused for it.
#[test]
fn a_summary_name_that_does_not_fit_is_left_out_of_the_row() {
    let (_, rows) = handle();
    let long = "x".repeat(usize::from(u16::MAX) + 1);
    let change = FloorChange {
        key_raised: Some(a_key_floor(7, 0)),
        ..FloorChange::default()
    };

    write_named(&rows, &change, Some(&long));

    let stored = rows.load().expect("the row reads back");
    assert_eq!(stored.key_floors, vec![a_key_floor(7, 0)]);
    assert!(stored.key_names.is_empty());
}

/// Asserts that a start refuses a floor row, or a key floor row, the scheduler did not write
/// and loads once the row is gone, and that with size keys off the key floors are neither read
/// nor allowed to fail the start.
#[test]
fn a_start_that_cannot_read_a_floor_loads_nothing() {
    let (store, rows) = handle();
    put(&store, "floor/zz", vec![9]);
    assert!(rows.load().is_err(), "a floor was read on a guess");
    store
        .txn(Txn::new().delete("floor/zz", Cond::Any))
        .expect("store")
        .expect("deleted");

    put(&store, "keyfloor/zz", vec![9]);
    assert!(rows.load().is_err(), "a key floor was read on a guess");
    rows.set_read_size_keys(false);
    let stored = rows.load().expect("size keys are off");
    assert!(stored.key_floors.is_empty() && stored.key_names.is_empty());
    rows.set_read_size_keys(true);
    store
        .txn(Txn::new().delete("keyfloor/zz", Cond::Any))
        .expect("store")
        .expect("deleted");
    rows.load().expect("loads once the row is gone");
}

/// Asserts that the floors the store holds now are read as their rows say, without a start's
/// checks: all of them, a row that is not a floor refused naming its key, and a floor under
/// another floor's key still read. Catches: a frontend that cannot see the floors the others
/// raised, and one that fails on a row a start would have repaired.
#[test]
fn the_floors_held_now_are_read_as_their_rows_say() {
    let (store, rows) = handle();
    assert_eq!(rows.floors().expect("read"), []);
    write(&rows, &raising(1, 0, &[]));
    write(&rows, &raising(2, 1, &[]));
    let mut held = rows.floors().expect("read");
    held.sort_by_key(|floor| floor.stamp);
    assert_eq!(held, [a_floor(1, 0), a_floor(2, 1)]);

    let row = value_of(&store, &keys::floor(&key_of(1)));
    put(&store, "floor/misnamed", row);
    assert_eq!(rows.floors().expect("read").len(), 3);
    assert!(rows.load().is_err(), "a start checks the key");

    put(&store, "floor/zz", vec![9]);
    let error = rows.floors().expect_err("not a floor");
    assert!(error.to_string().contains("floor/zz"), "{error}");
}

/// The key floor `ask` of size key 9, as a decay lowered it.
fn lowered_to(ask: u64) -> KeyFloor {
    KeyFloor {
        key: 9,
        ask,
        stamp: 1,
    }
}

/// The ask and name of the key floor row of size key 9.
fn held_9(rows: &Rows) -> (u64, Option<String>) {
    let stored = rows.load().expect("load");
    let name = stored.key_names.first().map(|(_, name)| name.clone());
    (stored.key_floors[0].ask, name)
}

/// Asserts that a lowering writes over the row it read when the row holds no more than the
/// floor the estimator lowered from, keeps the name the row has, and deletes the row of a
/// forgotten floor. Catches: a lowering that is not written, one that loses the summary
/// name, and a forgotten floor whose row stays.
#[test]
fn a_lowering_writes_the_row_it_read_and_keeps_its_name() {
    let (store, rows) = handle();
    rows.put_key_floor(&a_key_floor(9, 1), Some("est/x"))
        .expect("written");

    rows.lower_key_floor(&lowered_to(4 << 30), 16 << 30, None)
        .expect("lowered");
    assert_eq!(held_9(&rows), (4 << 30, Some("est/x".to_owned())));

    rows.lower_key_floor(&lowered_to(2 << 30), 4 << 30, Some("est/y"))
        .expect("lowered");
    assert_eq!(held_9(&rows), (2 << 30, Some("est/y".to_owned())));

    rows.lower_key_floor(&lowered_to(0), 2 << 30, None)
        .expect("forgotten");
    assert_eq!(count(&store, "keyfloor/"), 0);
}

/// Asserts that a lowering leaves the row alone when it holds more than the floor the
/// estimator lowered from (another frontend raised it), when it already holds the ask or
/// less (another frontend lowered it as far), for a forgotten floor too; with no row it
/// writes the floor (an earlier write of it was refused) and deletes nothing. Catches: a
/// lowering that undoes a raise it never saw, one that raises a row, a floor the store never
/// took that stays unwritten, and a forgotten floor that writes a row.
#[test]
fn a_lowering_leaves_a_row_that_holds_more_than_it_saw_or_less_than_it_asks() {
    let (store, rows) = handle();

    rows.lower_key_floor(&lowered_to(0), 8 << 30, None)
        .expect("no row");
    assert_eq!(count(&store, "keyfloor/"), 0, "nothing to delete");
    rows.lower_key_floor(&lowered_to(4 << 30), 8 << 30, None)
        .expect("no row");
    assert_eq!(held_9(&rows).0, 4 << 30, "written where there was no row");
    rows.put_key_floor(&a_key_floor(9, 1), Some("est/x"))
        .expect("written");
    rows.lower_key_floor(&lowered_to(4 << 30), 8 << 30, None)
        .expect("kept");
    rows.lower_key_floor(&lowered_to(0), 8 << 30, None)
        .expect("kept");
    assert_eq!(held_9(&rows).0, 16 << 30, "raised past what the decay saw");

    rows.lower_key_floor(&lowered_to(32 << 30), 64 << 30, None)
        .expect("kept");
    rows.lower_key_floor(&lowered_to(16 << 30), 64 << 30, None)
        .expect("kept");
    assert_eq!(held_9(&rows).0, 16 << 30, "never raised by a lowering");
}

/// Asserts that a lowering writes over a row that cannot be read. Catches: a lowering
/// refused for good by a damaged row.
#[test]
fn a_lowering_writes_over_a_row_that_cannot_be_read() {
    let (store, rows) = handle();
    put(&store, &keys::key_floor(9), vec![9]);

    rows.lower_key_floor(&lowered_to(4 << 30), 16 << 30, None)
        .expect("written over");

    assert_eq!(held_9(&rows).0, 4 << 30);
}

/// Asserts that a lowering whose row another frontend changed between the read and the write
/// reads it again: a row the other raised past the floor the estimator lowered from is kept,
/// and one it changed within that is lowered all the same. Catches: a lowering written
/// without a condition on the row it read (it overwrites the other's higher floor), and one
/// that gives up at the first conflict.
#[test]
fn a_lowering_that_loses_a_race_reads_the_row_again() {
    let (store, rows) = handle();
    rows.put_key_floor(&a_key_floor(9, 1), None)
        .expect("written");
    let row = keys::key_floor(9);
    let other = |ask: u64| {
        let value = key_floor_row_of(
            &KeyFloor {
                ask,
                ..a_key_floor(9, 2)
            },
            None,
        );
        Txn::new().put(row.clone(), value, Cond::Any)
    };

    store.land(&row, other(64 << 30));
    rows.lower_key_floor(&lowered_to(4 << 30), 16 << 30, None)
        .expect("the other's floor is kept");
    assert_eq!(held_9(&rows).0, 64 << 30);

    store.land(&row, other(12 << 30));
    rows.lower_key_floor(&lowered_to(4 << 30), 64 << 30, None)
        .expect("lowered over the other's row");
    assert_eq!(held_9(&rows).0, 4 << 30);
}
