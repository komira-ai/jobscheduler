//! The write that tells whether the store takes writes again.

mod common;

use common::{Racing, open, only};
use js_state::Store;
use js_store::{EpochPolicy, StateError};

/// Asserts that a ping is one empty-valued put of `meta/alive`, over whatever is there, and
/// that it fails with the store's error while the store takes no writes. Catches: a ping
/// that is a read (a store that reads and refuses writes would look back), or one that
/// conditions on the row (the second ping would conflict with the first).
#[test]
fn a_ping_is_an_unconditional_write_that_fails_while_the_store_is_down() {
    let store = Racing::new();
    let (rows, _) = open(store.clone(), EpochPolicy::Keep, 1).expect("open");
    store.asked();

    rows.ping().expect("the store takes it");
    assert_eq!(only(&store), ["put meta/alive cond=any ttl=-"]);
    rows.ping().expect("and again");
    let row = store.get("meta/alive").expect("read").expect("a row");
    assert!(row.value.is_empty());

    store.set_down(true);
    let refused = rows.ping().expect_err("the store is down");
    assert!(matches!(refused, StateError::Store(_)), "{refused:?}");
}
