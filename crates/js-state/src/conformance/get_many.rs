//! [`Store::get_many`]: the live rows of a set of keys, each once, in key order, as
//! [`Store::get`] answers each.

use std::sync::Arc;
use std::time::Duration;

use super::{Backend, run};
use crate::clock::ManualClock;
use crate::store::Store;
use crate::types::{Cond, Txn};

/// Seeds `gm/a`, `gm/b` (with a time to live of one second) and `gm/c`, then asks for
/// `gm/c`, `gm/a`, a key with no row, `gm/b` and `gm/a` again: the answer is each live key
/// once, in key order, with the value, version and expiry [`Store::get`] gives. Once the clock
/// passes `gm/b`'s expiry it is left out, reaped or not, and an empty list reads nothing.
pub fn get_many_reads_each_live_key_once_in_key_order<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let store = backend.open(Arc::clone(&clock));
    run(
        &store,
        Txn::new()
            .put("gm/a", b"1".to_vec(), Cond::Absent)
            .put_ttl("gm/b", b"2".to_vec(), Duration::from_secs(1), Cond::Absent)
            .put("gm/c", b"3".to_vec(), Cond::Absent),
    )
    .expect("the seed commits");
    let wanted: Vec<String> = ["gm/c", "gm/a", "gm/none", "gm/b", "gm/a"]
        .map(str::to_owned)
        .to_vec();
    let rows = store.get_many(&wanted).expect("the store is reachable");
    let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        ["gm/a", "gm/b", "gm/c"],
        "get_many keys: each live key once, in key order, and no key without a row"
    );
    for (key, row) in &rows {
        assert_eq!(
            Some(row),
            store.get(key).expect("the store is reachable").as_ref(),
            "get_many rows: a row is what get answers for its key"
        );
    }
    clock.advance(Duration::from_secs(1));
    let rows = store.get_many(&wanted).expect("the store is reachable");
    let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        ["gm/a", "gm/c"],
        "get_many expiry: a key whose expiry the clock reached is left out, reaped or not"
    );
    assert_eq!(
        store.get_many(&[]).expect("the store is reachable"),
        Vec::new(),
        "get_many empty: no key, no row"
    );
}
