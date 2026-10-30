//! A daemon's session row: written at its registration, renewed while it heartbeats, put
//! again when it is gone, and expired by the store a lease grace after the last write.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, MemoryPercent, Request};
use js_state::{Cond, Store, Txn};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, RowCodec, Rows, SESSION_RENEWAL, StateError, Value, WorkerUp, keys};
use js_types::sched::fence::LEASE_GRACE;
use js_types::sched::{LeaseGrant, Resources, WorkerId};

use common::{Racing, frontend_with};

const SESSION_ROW: &str = "session/node-a";

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

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

fn opened() -> (Arc<Racing>, Arc<TestCodec>, Rows) {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows = frontend_with(&store, 1, &codec);
    (store, codec, rows)
}

fn held(store: &Racing) -> bool {
    store.get(SESSION_ROW).expect("read").is_some()
}

/// A codec that cannot encode a registration.
struct Failing;

impl RowCodec for Failing {
    fn submit(&self, _: &Request) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no".to_owned()))
    }

    fn lease(&self, _: &LeaseGrant) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no".to_owned()))
    }

    fn session(&self, _: &WorkerUp) -> Result<Vec<u8>, StateError> {
        Err(StateError::Codec("no registration".to_owned()))
    }

    fn read(&self, _: &str, _: &[u8]) -> Result<Value, StateError> {
        Ok(Value::Unknown)
    }
}

/// Asserts that a registration puts the row under the node's name with the codec's bytes
/// for it, and that the store expires it exactly a lease grace after the write.
#[test]
fn a_registration_writes_the_session_row_for_a_lease_grace() {
    let (store, codec, rows) = opened();

    rows.session(&up("daemon-1")).expect("a session");

    let bytes = store.get(SESSION_ROW).expect("read").expect("a row").value;
    let read = codec.read(SESSION_ROW, &bytes).expect("a value");
    assert_eq!(read, Value::Session(up("daemon-1")));
    store.advance(LEASE_GRACE - Duration::from_millis(100));
    assert!(held(&store), "the row expired early");
    store.advance(Duration::from_millis(100));
    assert!(!held(&store), "the row outlived the lease grace");
}

/// Asserts that a renewal extends the row to a lease grace from the renewal, not from the
/// registration, and that the renewal interval is a third of the grace, so a row outlives
/// two missed renewals.
#[test]
fn a_renewal_extends_the_row_by_a_lease_grace_from_now() {
    let (store, _, rows) = opened();
    rows.session(&up("daemon-1")).expect("a session");
    store.advance(SESSION_RENEWAL);

    rows.renew(&node()).expect("a renewal");

    store.advance(LEASE_GRACE - Duration::from_millis(100));
    assert!(held(&store), "the renewal did not extend the row");
    store.advance(Duration::from_millis(100));
    assert!(!held(&store));
    assert_eq!(SESSION_RENEWAL, Duration::from_secs(20));
    assert!(SESSION_RENEWAL * 3 <= LEASE_GRACE);
}

/// Asserts that a renewal puts the registration this handle wrote last again when the row
/// is gone (it expired, or the write was refused), as a session row that expires.
#[test]
fn a_renewal_puts_the_registration_again_when_its_row_is_gone() {
    let (store, _, rows) = opened();
    rows.session(&up("daemon-1")).expect("a session");
    let written = store.get(SESSION_ROW).expect("read").expect("a row").value;
    let gone = Txn::new().delete(SESSION_ROW, Cond::Any);
    store.txn(gone).expect("store").expect("deleted");

    rows.renew(&node()).expect("put again");

    let row = store.get(SESSION_ROW).expect("read").expect("a row");
    assert_eq!(row.value, written);
    assert!(row.expires_at.is_some());
}

/// Asserts that a renewal for a node this handle never wrote a registration for writes
/// nothing: the daemon's next registration writes the row.
#[test]
fn a_renewal_for_a_node_never_registered_here_writes_nothing() {
    let (store, _, rows) = opened();
    let txns = store.commit_stats().txns;

    rows.renew(&node()).expect("nothing to do");

    assert_eq!(store.commit_stats().txns, txns);
    assert!(!held(&store));
}

/// Asserts that a renewal which loses to another writer of the row is refused with the
/// conflict, and does not write over what the other wrote: when the row was there at the
/// read and changes before the commit, and when it was not there and appears.
#[test]
fn a_renewal_that_loses_a_race_is_a_conflict() {
    let (store, _, rows) = opened();
    rows.session(&up("daemon-1")).expect("a session");
    let other = Txn::new().put(SESSION_ROW, b"other".to_vec(), Cond::Any);
    store.land(SESSION_ROW, other);

    let refused = rows.renew(&node());

    assert!(
        matches!(refused, Err(StateError::Conflict(_))),
        "{refused:?}"
    );
    let row = store.get(SESSION_ROW).expect("read").expect("a row");
    assert_eq!(row.value, b"other");

    let gone = Txn::new().delete(SESSION_ROW, Cond::Any);
    store.txn(gone).expect("store").expect("deleted");
    let appears = Txn::new().put(SESSION_ROW, b"again".to_vec(), Cond::Any);
    store.land(SESSION_ROW, appears);
    let refused = rows.renew(&node());
    assert!(
        matches!(refused, Err(StateError::Conflict(_))),
        "{refused:?}"
    );
}

/// Asserts that a registration the codec cannot encode writes no row and is not renewed
/// afterwards: the codec's own error is returned.
#[test]
fn a_registration_that_cannot_be_encoded_writes_nothing() {
    let store = Racing::new();
    let handle: Arc<dyn Store> = store.clone();
    let (rows, _) = Rows::open(
        handle,
        EpochPolicy::Keep,
        1,
        Arc::new(Failing),
        Arc::new(TestExt),
    )
    .expect("open");

    let refused = rows.session(&up("daemon-1"));

    assert!(
        matches!(&refused, Err(StateError::Codec(why)) if why == "no registration"),
        "{refused:?}"
    );
    rows.renew(&node()).expect("nothing to renew");
    assert!(!held(&store));
    assert!(store.get(&keys::session(&node())).expect("read").is_none());
}

/// The registration of `worker` by a daemon: a Linux node with the container driver and
/// four cores and 8 GiB.
fn registered(worker: &str) -> WorkerUp {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    WorkerUp {
        worker: WorkerId::new(worker),
        instance: DaemonInstance::new("daemon"),
        predecessor_stopped: false,
        capacity: Resources::new(4_000, 8 << 30),
        caps,
    }
}

/// A request for an action that takes one core and 1 GiB.
fn asking() -> Request {
    Request {
        key: js_types::sched::WorkKey::new("main", "01/1"),
        qos: js_types::sched::Qos::Ci,
        kind: js_types::sched::LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    }
}

/// Asserts that an operation could run if any daemon session in the store could run it,
/// whichever frontend wrote the session; and that none could when the store holds no
/// session, or only sessions too small for it or of another platform.
#[test]
fn a_request_could_run_if_any_daemon_session_of_the_store_could_run_it() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let (mine, other) = (
        frontend_with(&store, 1, &codec),
        frontend_with(&store, 2, &codec),
    );
    assert!(
        !mine
            .session_could_run(&asking(), MemoryPercent::FULL)
            .expect("no sessions")
    );
    other
        .session(&registered("node-b"))
        .expect("a session of the other frontend");

    assert!(
        mine.session_could_run(&asking(), MemoryPercent::FULL)
            .expect("a session")
    );
    let mut big = asking();
    big.resources = Resources::new(1_000, 16 << 30);
    assert!(
        !mine
            .session_could_run(&big, MemoryPercent::FULL)
            .expect("a session")
    );
    let mut mac = asking();
    let needs = js_caps::Request::from_platform([("OSFamily", "macos")]);
    mac.needs = needs.expect("a platform");
    assert!(
        !mine
            .session_could_run(&mac, MemoryPercent::FULL)
            .expect("a session")
    );
}

/// Asserts that a session's capacity is cut to the percent before a request is tried
/// against it. Catches: a hold that judges the raw row, so a request above the cut but
/// within the raw capacity is held for ever.
#[test]
fn a_session_row_is_judged_at_the_cut_capacity() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows = frontend_with(&store, 1, &codec);
    rows.session(&registered("node-a")).expect("a session");
    let half = MemoryPercent::new(50).expect("in range");
    let mut between = asking();
    between.resources = Resources::new(1_000, 6 << 30);

    assert!(
        rows.session_could_run(&between, MemoryPercent::FULL)
            .expect("read")
    );
    assert!(!rows.session_could_run(&between, half).expect("read"));
    assert!(rows.session_could_run(&asking(), half).expect("read"));
}

/// Asserts that a session row that is not a daemon's registration is refused naming the
/// row, whether its bytes are not a value of the codec at all or are another kind of value.
#[test]
fn a_session_row_that_is_not_a_registration_is_refused_naming_it() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let rows = frontend_with(&store, 1, &codec);
    let submit = codec.submit(&asking()).expect("encodes");
    for (key, value) in [("session/a", vec![0xff]), ("session/b", submit)] {
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("put");

        let read = rows.session_could_run(&asking(), MemoryPercent::FULL);

        let error = read.expect_err("a corrupt row");
        assert!(error.to_string().contains(key), "{error}");
        store
            .txn(Txn::new().delete(key, Cond::Any))
            .expect("store")
            .expect("delete");
    }
}

/// Asserts that the daemons of the other frontends are the session rows of the nodes this
/// frontend does not hold: a node in `connected` is left out, the others are listed with
/// their capacity, and a row that is not a registration is refused naming it.
#[test]
fn the_remote_sessions_are_the_rows_of_nodes_this_frontend_does_not_hold() {
    let store = Racing::new();
    let codec = Arc::new(TestCodec::new());
    let (mine, other) = (
        frontend_with(&store, 1, &codec),
        frontend_with(&store, 2, &codec),
    );
    let none = std::collections::BTreeSet::new();
    assert!(mine.remote_sessions(&none).expect("no sessions").is_empty());
    for node in ["node-a", "node-b"] {
        other.session(&registered(node)).expect("a session");
    }

    let held = [WorkerId::new("node-a")].into();
    let remote = mine.remote_sessions(&held).expect("sessions");
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].1, Resources::new(4_000, 8 << 30));
    assert_eq!(mine.remote_sessions(&none).expect("sessions").len(), 2);

    store
        .txn(Txn::new().put("session/c", vec![0xff], Cond::Any))
        .expect("store")
        .expect("put");
    let error = mine.remote_sessions(&none).expect_err("a corrupt row");
    assert!(error.to_string().contains("session/c"), "{error}");
    // A row that decodes to another kind of value is refused as well.
    let submit = codec.submit(&asking()).expect("encodes");
    store
        .txn(Txn::new().put("session/c", submit, Cond::Any))
        .expect("store")
        .expect("put");
    let error = mine.remote_sessions(&none).expect_err("another value");
    assert!(error.to_string().contains("session/c"), "{error}");
}
