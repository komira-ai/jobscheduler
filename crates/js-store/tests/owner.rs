//! The ownership of a daemon's node when several frontends share one state store: the
//! `Hello` that makes a frontend the owner, the writes of a node's leases that only the
//! owner may make, and the look at the owner row that a heartbeat's acknowledgement waits
//! on. Each test stands two handles over one store for two frontends.
//!
//! The writes of a lease are the transactions of a grant, a lease given up and a result.
//! The grant and the result are this crate's ([`Rows::granted`], [`Rows::finished`]); the
//! lease given up belongs to a later row of this crate, so the tests make a stand-in that
//! touches the same rows under the same conditions: it is built, checked with
//! [`Rows::guard`] and committed with [`Rows::commit_owned`], as the transaction is.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
use js_state::{Cond, Store, Txn};
use js_store::keys;
use js_store::test_support::{TestCodec, TestExt};
use js_store::{
    CAS_TRIES, Ending, EpochPolicy, OWNER_CACHE_TTL, Owner, RowCodec, Rows, StateError, WorkerUp,
};
use js_types::sched::{FarmTime, LeaseGrant, LeaseId, OperationId, Resources, WorkKey, WorkerId};

use common::{Racing, frontend_with, open};

const OWNER_ROW: &str = "owner/node-a";
const SESSION_ROW: &str = "session/node-a";

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

fn at(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1_000)
}

fn named(frontend: u64, counter: u64) -> Owner {
    Owner { frontend, counter }
}

/// The bytes of an `owner/` row: the frontend and the counter, eight bytes each,
/// big-endian.
fn owner_row(frontend: u64, counter: u64) -> Vec<u8> {
    [frontend.to_be_bytes(), counter.to_be_bytes()].concat()
}

/// A store, the codec its frontends share, and a handle for each of the frontends numbered
/// `ids`.
fn frontends<const N: usize>(ids: [u64; N]) -> (Arc<Racing>, Arc<TestCodec>, [Rows; N]) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let handles = ids.map(|id| frontend_with(&store, id, &codec));
    (store, codec, handles)
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

/// Replaces the row at `key`, whatever it holds.
fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// Deletes the row at `key`.
fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("deleted");
}

/// How many rows the store holds under `prefix`.
fn rows(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

/// Queues operation `operation`: its `queue/` row.
fn queue(store: &Racing, operation: u64) {
    let put = Txn::new().put(
        keys::queue(OperationId(operation)),
        Vec::new(),
        Cond::Absent,
    );
    store.txn(put).expect("store").expect("queued");
}

/// The grant of `operation` under the lease `(5, seq)` on `worker`.
fn grant(rows: &Rows, operation: u64, seq: u64, worker: &str) -> Result<(), StateError> {
    rows.granted(&LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
    })
}

/// The result of the lease `(5, seq)` of `operation`, which is on node-a: the operation ends
/// with a `done/` row.
fn finish(rows: &Rows, operation: u64, seq: u64) -> Result<(), StateError> {
    let ending = Ending::new(b"done".to_vec(), Duration::from_secs(1));
    rows.finished(OperationId(operation), LeaseId::new(5, seq), &ending)
}

/// A stand-in for the lease `(5, seq)` of `operation` on node-a given up: the lease rows
/// go and the operation is queued again.
fn give_up(rows: &Rows, store: &Racing, operation: u64, seq: u64) -> Result<(), StateError> {
    let lease = store.get(&keys::lease(LeaseId::new(5, seq))).expect("read");
    let version = lease.expect("a lease").version;
    let txn = Txn::new()
        .delete(keys::lease(LeaseId::new(5, seq)), Cond::Version(version))
        .delete(keys::lease_by_op(OperationId(operation)), Cond::Any)
        .put(
            keys::queue(OperationId(operation)),
            Vec::new(),
            Cond::Absent,
        );
    let txn = rows.guard(txn, &node())?;
    rows.commit_owned(&node(), txn)
}

fn is_stale<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::StaleOwner(_)))
}

fn is_conflict<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Conflict(_)))
}

fn is_corrupt<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Corrupt { .. }))
}

/// The registration of `node` by the daemon process `daemon`.
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

fn request() -> Request {
    request_of(1)
}

/// A request for the work `tag`: no two tags are the same work.
fn request_of(tag: u64) -> Request {
    Request {
        key: WorkKey::new("main", format!("ab/{tag}")),
        qos: js_types::sched::Qos::Ci,
        kind: js_types::sched::LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

/// Asserts that a daemon's `Hello` makes the frontend the owner of the node at the next
/// counter, and says who owned it before. The row is the frontend's id and the counter,
/// eight bytes each, big-endian, pinned byte for byte because another release reads it.
#[test]
fn each_hello_makes_the_frontend_the_owner_at_the_next_counter() {
    let (store, _, [first, second]) = frontends([7, 9]);

    let arrival = first.arrive(&node(), at(0)).expect("a first Hello");
    assert_eq!((arrival.previous, arrival.frontend), (None, 7));
    let row = value_of(&store, OWNER_ROW);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 1]);

    let arrival = second.arrive(&node(), at(1)).expect("a second Hello");
    assert_eq!(arrival.previous, Some(named(7, 1)));
    let row = value_of(&store, OWNER_ROW);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2]);

    let arrival = second.arrive(&node(), at(2)).expect("a third Hello");
    assert_eq!(arrival.previous, Some(named(9, 2)));
}

/// Asserts that a grant for a node another frontend owns now is refused as stale and writes
/// nothing; that the node's later writes are refused too, whatever their rows say; that a
/// node the frontend never claimed is granted as before; and that the owner's grant stands.
#[test]
fn a_frontend_a_daemon_has_left_grants_its_node_nothing() {
    let (store, _, [left, taker]) = frontends([1, 2]);
    left.arrive(&node(), at(0)).expect("a Hello");
    taker.arrive(&node(), at(1)).expect("moved");
    queue(&store, 4);
    queue(&store, 5);
    let txns = store.commit_stats().txns;

    let refused = grant(&left, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!(store.commit_stats().txns, txns, "a stale grant wrote");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (2, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
    let again = grant(&left, 4, 1, "node-a");
    assert!(is_stale(&again), "{again:?}");

    grant(&left, 5, 2, "node-b").expect("a node never claimed");
    grant(&taker, 4, 3, "node-a").expect("the owner's grant");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (0, 2));
    assert_eq!(rows(&store, "lease-by-op/"), 2);
}

/// Asserts that a lease given up, and a result taken, by the frontend a daemon has left are
/// refused as stale and write nothing, and that the frontend the daemon moved to takes the
/// result as it would have.
#[test]
fn a_lease_is_given_up_and_finished_only_by_the_owner_of_its_node() {
    let (store, _, [left, taker]) = frontends([1, 2]);
    left.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    grant(&left, 4, 0, "node-a").expect("a grant");
    taker.arrive(&node(), at(1)).expect("moved");
    let txns = store.commit_stats().txns;

    let given_up = give_up(&left, &store, 4, 0);
    let finished = finish(&left, 4, 0);

    assert!(is_stale(&given_up), "{given_up:?}");
    assert!(is_stale(&finished), "{finished:?}");
    assert_eq!(store.commit_stats().txns, txns, "a stale write wrote");
    assert_eq!((rows(&store, "lease/"), rows(&store, "done/")), (1, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 1);

    finish(&taker, 4, 0).expect("the owner's result");
    assert_eq!((rows(&store, "lease/"), rows(&store, "done/")), (0, 1));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
}

/// Asserts that the owner of a node gives its lease up as it did: the operation is queued
/// again, and the lease and its index row are gone.
#[test]
fn the_owner_of_a_node_gives_its_lease_up() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    grant(&first, 4, 0, "node-a").expect("a grant");

    give_up(&first, &store, 4, 0).expect("a give-up");

    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (1, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
}

/// Asserts that a frontend whose node's owner row is gone writes nothing for the node: it
/// cannot show that it still owns it.
#[test]
fn a_frontend_whose_owner_row_is_gone_writes_nothing_for_the_node() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    remove(&store, OWNER_ROW);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!(rows(&store, "lease/"), 0);
}

/// Asserts that a grant that read the owner row as its own, and finds it changed when it
/// commits because the daemon said `Hello` to another frontend in between, writes nothing and
/// fails as stale.
#[test]
fn a_node_that_changes_hands_between_the_read_and_the_commit_is_stale() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    let moved = Txn::new().put(OWNER_ROW, owner_row(2, 2), Cond::Any);
    store.land(OWNER_ROW, moved);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (1, 0));
    let later = grant(&first, 4, 1, "node-a");
    assert!(is_stale(&later), "{later:?}");
}

/// Asserts that a conflict on a row other than the owner row stays a conflict, and leaves
/// the node the frontend's: its owner row still stands.
#[test]
fn a_conflict_on_another_row_is_not_a_change_of_owner() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    let queued = keys::queue(OperationId(4));
    let taken = Txn::new().delete(queued.clone(), Cond::Any);
    store.land(&queued, taken);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_conflict(&refused), "{refused:?}");
    assert!(first.owns(&node(), at(100)).expect("a look"));
}

/// Asserts that a frontend that loses every try to make itself a node's owner, to others
/// who write the row first, gives the node up: the error is the conflict, and nothing is
/// written for the node afterwards.
#[test]
fn a_frontend_that_loses_every_try_for_a_node_gives_it_up() {
    let (store, _, [first]) = frontends([1]);
    queue(&store, 4);
    let tries = u64::try_from(CAS_TRIES).expect("a small number");
    for counter in 1..=tries {
        let other = Txn::new().put(OWNER_ROW, owner_row(2, counter), Cond::Any);
        store.land(OWNER_ROW, other);
    }

    let lost = first.arrive(&node(), at(0));

    assert!(is_conflict(&lost), "{lost:?}");
    let refused = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");
}

/// Asserts that the owner row is read again when the frontend's last look is the cache TTL
/// old and not before, that a look that finds the row its own extends the cache, that one
/// that does not finds the node lost for good, and that a node never claimed is nobody's.
#[test]
fn the_owner_row_is_read_again_when_the_last_look_is_a_cache_ttl_old() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(10)).expect("a Hello");
    let just_before = at(10).saturating_add(OWNER_CACHE_TTL - std::time::Duration::from_millis(1));
    let due = at(10).saturating_add(OWNER_CACHE_TTL);
    remove(&store, OWNER_ROW);

    assert!(first.owns(&node(), just_before).expect("cached"));
    assert!(!first.owns(&node(), due).expect("read"));
    assert!(!first.owns(&node(), due).expect("lost"));

    let (store, _, [second]) = frontends([1]);
    second.arrive(&node(), at(10)).expect("a Hello");
    assert!(second.owns(&node(), due).expect("read"));
    remove(&store, OWNER_ROW);
    let later = due.saturating_add(OWNER_CACHE_TTL - std::time::Duration::from_millis(1));
    assert!(second.owns(&node(), later).expect("cached"));
    let after = due.saturating_add(OWNER_CACHE_TTL);
    assert!(!second.owns(&node(), after).expect("read"));

    let stranger = WorkerId::new("node-z");
    assert!(!second.owns(&stranger, at(0)).expect("never claimed"));
}

/// Asserts that a node whose owner row names another frontend is not owned, and that a
/// write for it is refused.
#[test]
fn a_node_another_frontend_claimed_is_not_owned() {
    let (store, _, [first, second]) = frontends([1, 2]);
    first.arrive(&node(), at(0)).expect("a Hello");
    second.arrive(&node(), at(1)).expect("moved");

    assert!(!first.owns(&node(), at(60)).expect("read"));
    assert!(second.owns(&node(), at(60)).expect("read"));
    queue(&store, 4);
    let refused = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");
}

/// Asserts that a `Hello` reads what it takes over: the owner before, the unfinished
/// operations whose lease rows name the node (not another node's, and not a lease whose
/// operation has no row), and the daemon process the node's session row names (none when
/// the row is missing, is not a registration, or holds a value of a later binary).
#[test]
fn a_hello_reads_the_leases_and_the_session_of_the_node_it_takes_over() {
    let (store, codec, [left, taker]) = frontends([1, 2]);
    let first = left.arrive(&node(), at(0)).expect("a Hello");
    assert_eq!(first.session, None);
    assert!(first.leases.is_empty());
    for tag in [4, 5] {
        let submitted = left.submit(OperationId(tag), &request_of(tag), &[], None);
        submitted.expect("submit");
        remove(&store, &keys::queue(OperationId(tag)));
    }
    let leased = |operation: u64, seq: u64, worker: &str| LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(worker),
    };
    // Operation 9 has no row: its lease is left out.
    for grant in [
        leased(4, 0, "node-a"),
        leased(5, 1, "node-b"),
        leased(9, 9, "node-a"),
    ] {
        overwrite(
            &store,
            &keys::lease(grant.lease),
            codec.lease(&grant).expect("row"),
        );
    }
    let daemon = DaemonInstance::new("daemon-1");
    left.session(&up("daemon-1")).expect("a session");

    let arrival = taker.arrive(&node(), at(1)).expect("moved");

    assert_eq!(arrival.previous, Some(named(1, 1)));
    assert_eq!(arrival.session, Some(daemon));
    let held: Vec<OperationId> = arrival.leases.iter().map(|op| op.operation).collect();
    assert_eq!(held, [OperationId(4)]);
    let lease = arrival.leases[0].lease.as_ref().map(|grant| grant.lease);
    assert_eq!(lease, Some(LeaseId::new(5, 0)));

    overwrite(&store, SESSION_ROW, vec![0xff]);
    let unreadable = taker.arrive(&node(), at(2)).expect("a Hello");
    assert_eq!(unreadable.session, None);
    let other = codec.submit(&request()).expect("row");
    overwrite(&store, SESSION_ROW, other);
    let not_a_session = taker.arrive(&node(), at(3)).expect("a Hello");
    assert_eq!(not_a_session.session, None);
    overwrite(&store, SESSION_ROW, b"later".to_vec());
    let from_later = taker.arrive(&node(), at(4)).expect("a Hello");
    assert_eq!(from_later.session, None);
}

/// Asserts that an owner row that is not one is refused naming the row: at a `Hello`, at a
/// write for a node the frontend claimed before the row was damaged, and at a look; and that
/// a node whose claim failed on it is lost, so that a write for it is refused as stale. A
/// row of any length but 16 bytes is not an owner.
#[test]
fn an_owner_row_that_is_not_one_is_refused_naming_it() {
    let (store, _, [first]) = frontends([1]);
    overwrite(&store, OWNER_ROW, vec![1, 2, 3]);
    let error = first.arrive(&node(), at(0)).expect_err("a damaged row");
    let said = error.to_string();
    assert!(said.contains(OWNER_ROW), "{said}");
    assert!(said.contains("not a 16-byte owner"), "{said}");
    queue(&store, 4);
    let unclaimed = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&unclaimed), "{unclaimed:?}");
    for length in [12, 17] {
        overwrite(&store, OWNER_ROW, vec![9; length]);
        let refused = first.arrive(&node(), at(0));
        assert!(is_corrupt(&refused), "{length}: {refused:?}");
    }

    let (store, _, [second]) = frontends([1]);
    second.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    overwrite(&store, OWNER_ROW, vec![1, 2, 3]);
    let refused = grant(&second, 4, 0, "node-a");
    assert!(is_corrupt(&refused), "{refused:?}");
    let look = second.owns(&node(), at(100));
    assert!(is_corrupt(&look), "{look:?}");
}

/// Asserts that a store holding `owner/` and `lease-by-op/` rows reads as it did without
/// them for a release that does not know them: every prefix it scans meets neither, and
/// neither begins with it.
#[test]
fn a_store_with_owner_and_index_rows_reads_as_it_did_without_them() {
    let (store, _, [first]) = frontends([1]);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&store, 4);
    grant(&first, 4, 0, "node-a").expect("a grant");
    assert_eq!(rows(&store, "owner/"), 1);
    assert_eq!(rows(&store, "lease-by-op/"), 1);

    let known = [
        "op/",
        "queue/",
        "lease/",
        "session/",
        "loss/",
        "fate/",
        "floor/",
        "subject/",
        "done/",
        "cordon/",
        "inflight/",
        "opkey/",
        "keyfloor/",
        "est/",
        "meta/",
    ];
    for prefix in known {
        let scanned = store.scan_prefix(prefix).expect("scan");
        for new in ["owner/", "lease-by-op/"] {
            let meets = scanned.iter().any(|(key, _)| key.starts_with(new));
            assert!(!meets, "a scan of {prefix} meets a {new} row");
        }
    }
}

/// Asserts that a claim whose arrival cannot be read leaves the node lost to the frontend (a
/// write for it is refused as stale), and that the claim made again, which finds the
/// frontend's own row, reports the owner the first claim replaced and not the frontend
/// itself, and the node is owned again.
#[test]
fn a_claim_made_again_after_an_unread_arrival_reports_the_owner_it_replaced() {
    let (store, _, [left, taker]) = frontends([1, 2]);
    left.arrive(&node(), at(0)).expect("a Hello");
    store.set_unreadable(Some(SESSION_ROW));

    let unread = taker.arrive(&node(), at(1));

    assert!(unread.is_err(), "{unread:?}");
    queue(&store, 4);
    let refused = grant(&taker, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");

    store.set_unreadable(None);
    let again = taker.arrive(&node(), at(2)).expect("the claim again");
    assert_eq!(again.previous, Some(named(1, 1)));
    grant(&taker, 4, 1, "node-a").expect("owned again");
    let later = taker.arrive(&node(), at(3)).expect("a Hello");
    assert_eq!(later.previous, Some(named(2, 3)));
}

/// Asserts that the owner an unread claim replaced is not reported when the row names
/// another frontend by the time the claim is made again: that frontend is the one replaced.
#[test]
fn a_claim_made_again_over_another_frontends_row_reports_that_frontend() {
    let (store, _, [left, taker, other]) = frontends([1, 2, 3]);
    left.arrive(&node(), at(0)).expect("a Hello");
    store.set_unreadable(Some(SESSION_ROW));
    let unread = taker.arrive(&node(), at(1));
    assert!(unread.is_err(), "{unread:?}");
    store.set_unreadable(None);
    other.arrive(&node(), at(2)).expect("moved");

    let again = taker.arrive(&node(), at(3)).expect("the claim again");

    assert_eq!(again.previous, Some(named(3, 3)));
}

/// Asserts that two writers on one store, each of them the owner by its own belief, cannot
/// both write the node's lease: the frontend the daemon said `Hello` to last writes it, and
/// the other, which reads the owner row after that arrival, is refused as stale and leaves
/// the store as the owner has it. The two handles run one after the other on a plain fake;
/// a change of hands between one writer's read and its commit is the job of
/// `a_node_that_changes_hands_between_the_read_and_the_commit_is_stale`.
#[test]
fn two_writers_on_one_fake_the_loser_gets_stale_owner() {
    let store = Arc::new(js_state::fake::Fake::with_flaw(
        Arc::new(js_state::ManualClock::default()),
        js_state::fake::Flaw::None,
    ));
    let codec = Arc::new(TestCodec::new());
    let handle = |id: u64| {
        let store: Arc<dyn Store> = store.clone();
        let codec: Arc<dyn RowCodec> = codec.clone();
        Rows::open(store, EpochPolicy::Keep, id, codec, Arc::new(TestExt))
            .expect("open")
            .0
    };
    let (loser, winner) = (handle(1), handle(2));
    loser.arrive(&node(), at(0)).expect("a Hello");
    winner.arrive(&node(), at(1)).expect("moved");
    let lease = || Txn::new().put("lease/test", b"held".to_vec(), Cond::Absent);

    let guarded = loser.guard(lease(), &node());

    assert!(
        matches!(guarded, Err(StateError::StaleOwner(_))),
        "{guarded:?}"
    );
    let txn = winner
        .guard(lease(), &node())
        .expect("the owner is checked");
    winner
        .commit_owned(&node(), txn)
        .expect("the owner's write");
    assert!(store.get("lease/test").expect("read").is_some());
    let retry = loser.guard(lease(), &node());
    assert!(matches!(retry, Err(StateError::StaleOwner(ref who)) if *who == node()));
}

/// Asserts that a write for a node the handle never claimed has no counter to check: the
/// guard returns the transaction as it was, and it commits like any write.
#[test]
fn a_node_never_claimed_passes_the_guard_unchanged() {
    let store = Racing::new();
    let (first, _) = open(store.clone(), EpochPolicy::Keep, 1).expect("open");
    let txn = Txn::new().put("lease/test", b"held".to_vec(), Cond::Absent);
    let before = txn.clone();

    let guarded = first.guard(txn, &node()).expect("no claim to check");

    assert_eq!(guarded, before);
    first.commit_owned(&node(), guarded).expect("a write");
    assert_eq!(value_of(&store, "lease/test"), b"held");
}
