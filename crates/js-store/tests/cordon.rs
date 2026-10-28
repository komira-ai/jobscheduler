//! How an operator placed a node: the `cordon/` rows, their bytes, and their reading back.

mod common;

use js_state::{Cond, Store, StoreError, Txn};
use js_store::{StateError, StoredCordon};
use js_types::sched::WorkerId;

use common::{Racing, frontend_with};

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn drain(deadline_unix_ms: u64) -> StoredCordon {
    StoredCordon::Draining { deadline_unix_ms }
}

fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn opened() -> (std::sync::Arc<Racing>, js_store::Rows) {
    let store = Racing::new();
    let codec = std::sync::Arc::new(js_store::test_support::TestCodec::new());
    let rows = frontend_with(&store, 1, &codec);
    (store, rows)
}

/// Asserts the bytes of a cordon row (a tag 0, and for a drain a tag 1 and the deadline,
/// eight bytes big-endian, which the farm server's rows hold byte for byte), that the
/// rows read back in node order with the node's name as it is (a `/` in it too), and that
/// returning a node to placement removes its row and nothing else.
#[test]
fn a_cordon_is_a_row_that_reads_back_until_the_node_is_returned_to_placement() {
    let (store, rows) = opened();

    rows.cordoned(&node("node-c"), Some(StoredCordon::Cordoned))
        .expect("a cordon");
    rows.cordoned(&node("node-b"), Some(drain(1_234_567)))
        .expect("a drain");
    rows.cordoned(&node("a/b"), Some(drain(u64::MAX)))
        .expect("a drain");

    assert_eq!(value_of(&store, "cordon/node-c"), [0]);
    assert_eq!(
        value_of(&store, "cordon/node-b"),
        [1, 0, 0, 0, 0, 0, 0x12, 0xd6, 0x87]
    );
    assert_eq!(
        rows.cordons().expect("cordons"),
        [
            (node("a/b"), drain(u64::MAX)),
            (node("node-b"), drain(1_234_567)),
            (node("node-c"), StoredCordon::Cordoned),
        ]
    );
    rows.cordoned(&node("node-b"), None).expect("returned");
    let left: Vec<WorkerId> = rows
        .cordons()
        .expect("cordons")
        .into_iter()
        .map(|(worker, _)| worker)
        .collect();
    assert_eq!(left, [node("a/b"), node("node-c")]);
    rows.cordoned(&node("node-z"), None)
        .expect("a node never placed");
}

/// Asserts that a cordon placed again replaces the one before, a drain by a plain cordon and
/// back.
#[test]
fn a_node_placed_again_keeps_the_newest_placement() {
    let (_, rows) = opened();
    rows.cordoned(&node("node-a"), Some(drain(5)))
        .expect("a drain");
    rows.cordoned(&node("node-a"), Some(StoredCordon::Cordoned))
        .expect("a cordon");
    assert_eq!(
        rows.cordons().expect("cordons"),
        [(node("node-a"), StoredCordon::Cordoned)]
    );
    rows.cordoned(&node("node-a"), Some(drain(9)))
        .expect("a drain");
    assert_eq!(
        rows.cordons().expect("cordons"),
        [(node("node-a"), drain(9))]
    );
}

/// Asserts that a row under `cordon/` that is not a cordon refuses the read, naming the row
/// and saying why: a tag no placement has, an empty row, a plain cordon with more bytes, and
/// a drain whose deadline is not eight bytes (short or long).
#[test]
fn a_cordon_row_that_is_not_one_refuses_the_read_naming_it() {
    for (junk, why) in [
        (vec![], "not a cordon"),
        (vec![2], "not a cordon"),
        (vec![0, 0], "not a cordon"),
        (vec![1], "a drain deadline is not 8 bytes"),
        (vec![1, 0, 0, 0], "a drain deadline is not 8 bytes"),
        (vec![1; 10], "a drain deadline is not 8 bytes"),
    ] {
        let (store, rows) = opened();
        rows.cordoned(&node("node-a"), Some(StoredCordon::Cordoned))
            .expect("a cordon");
        overwrite(&store, "cordon/node-b", junk.clone());

        let refused = rows.cordons();

        let Err(StateError::Corrupt { key, why: said }) = refused else {
            panic!("{junk:?}: {refused:?}");
        };
        assert_eq!((key.as_str(), said.as_str()), ("cordon/node-b", why));
    }
}

/// Asserts that a store that cannot be written refuses a placement with the backend's own
/// error, and writes nothing.
#[test]
fn a_store_that_is_down_refuses_a_placement() {
    let (store, rows) = opened();
    store.set_down(true);

    let refused = rows.cordoned(&node("node-a"), Some(StoredCordon::Cordoned));

    assert!(
        matches!(refused, Err(StateError::Store(StoreError::Failed(_)))),
        "{refused:?}"
    );
    store.set_down(false);
    assert!(rows.cordons().expect("cordons").is_empty());
}
