//! A daemon's session row: written at its registration, renewed while it heartbeats, put
//! again when it is gone, and expired by the store a lease grace after the last write.

mod common;

use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
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
