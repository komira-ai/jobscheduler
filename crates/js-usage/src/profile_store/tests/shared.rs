//! Two servers' [`ProfileStore`]s over one state store: each evicts a row only at the version
//! it last saw the row at, so a row the other rewrote since is kept.

use super::*;

/// The bytes of a stored row of one run on `day`.
fn one_run_row(day: u64) -> Vec<u8> {
    let mut ring = crate::ring::Ring::new();
    ring.push(&run());
    codec::encode(day, &ring)
}

/// The size of a row of one run.
fn one() -> u64 {
    let mut probe = open(&TestStore::new(), 1 << 20, 10);
    learn_one(&mut probe, "x", 10);
    probe.total_bytes()
}

/// Asserts that a victim the other server rewrote after this one's scan is kept, and the next
/// least recently used keys go in its place: `b` scanned `a`, `b` and `d` (in that order of
/// use), `a` persisted a use of `a` since, and `b`'s write of `c` needs two evictions. The
/// delete of `a` at the scanned version fails, `a` is read again and counts as just used, a
/// victim still as scanned (`b`) keeps its place, and `b` and `d` go. It catches an eviction
/// that deletes a row whatever version it holds, which deletes `a` and `b` and keeps `d`.
#[test]
fn a_victim_the_other_server_rewrote_is_kept_and_the_next_ones_go() {
    let store = TestStore::new();
    let one = one();
    let mut first = open(&store, 1 << 20, 10);
    for key in ["a", "b", "d"] {
        learn_one(&mut first, key, 10);
    }
    let mut second = open(&store, 2 * one, 10);
    first.touch(&pk("a"), 11);
    assert_eq!(first.persist_touches().unwrap(), 1);

    learn_one(&mut second, "c", 11);
    assert_eq!(stored(&store), ["a", "c"], "b and d went, a stayed");
    assert_eq!(second.len(), 2);
    assert_eq!(second.total_bytes(), 2 * one);
    let a = second.profile(&pk("a")).unwrap().unwrap();
    assert_eq!(a.last_used_day, 11, "the other server's use is read");
}

/// Asserts that a victim the other server deleted since the scan leaves the index, and the
/// write is planned again without it: here the space it held is enough, so nothing else is
/// evicted. It catches a store that keeps counting a row that is gone, which would evict `b`
/// for space that is free.
#[test]
fn a_victim_the_other_server_deleted_leaves_the_index() {
    let store = TestStore::new();
    let one = one();
    let mut first = open(&store, 1 << 20, 10);
    learn_one(&mut first, "a", 10);
    learn_one(&mut first, "b", 10);
    let mut second = open(&store, 2 * one, 10);
    let gone = store
        .fake
        .txn(Txn::new().delete(format!("{KEY_PREFIX}a"), Cond::Any));
    gone.unwrap().unwrap();

    learn_one(&mut second, "c", 10);
    assert_eq!(stored(&store), ["b", "c"]);
    assert_eq!(second.len(), 2);
    assert_eq!(second.total_bytes(), 2 * one);
    assert_eq!(second.profile(&pk("a")).unwrap(), None);
}

/// Asserts that a victim the other server rewrote with a row that does not read is kept as
/// just used on the day this store knew, and the next key goes: the unreadable row is the
/// next run's of its key to replace. It catches a refresh that drops a row it cannot read
/// from the index while the row is still stored.
#[test]
fn a_victim_rewritten_unreadable_is_kept_on_the_day_known() {
    let store = TestStore::new();
    let one = one();
    let mut first = open(&store, 1 << 20, 10);
    learn_one(&mut first, "a", 10);
    learn_one(&mut first, "b", 10);
    let mut second = open(&store, 2 * one, 10);
    let garbage = vec![0xff; usize::try_from(one).unwrap()];
    let put = Txn::new().put(format!("{KEY_PREFIX}a"), garbage, Cond::Any);
    store.fake.txn(put).unwrap().unwrap();

    learn_one(&mut second, "c", 10);
    assert_eq!(stored(&store), ["a", "c"], "b went in place of a");
    assert_eq!(second.len(), 2);
}

/// Asserts that a writer that rewrites every victim after each read makes the write fail
/// with the key's conflict once the evictions were planned again eight times, with nothing of
/// the write stored and no row deleted; and that a read that fails while a victim is read
/// again fails the write with the store's error. It catches an eviction that plans again for
/// ever, and one that deletes a victim it could not read again.
#[test]
fn victims_that_keep_changing_fail_the_write() {
    let store = TestStore::new();
    let one = one();
    let mut first = open(&store, 1 << 20, 10);
    learn_one(&mut first, "a", 10);
    learn_one(&mut first, "b", 10);
    let mut second = open(&store, 2 * one, 10);
    store.on_get(|fake| {
        for key in ["a", "b"] {
            let put = Txn::new().put(format!("{KEY_PREFIX}{key}"), one_run_row(10), Cond::Any);
            fake.txn(put).unwrap().unwrap();
        }
    });
    let before = store.txn_count();
    let err = second.run_ended(&pk("c"), &run(), 10).unwrap_err();
    assert_eq!(
        err.to_string(),
        "profile row c was changed by another writer"
    );
    assert_eq!(
        store.txn_count(),
        before + 9,
        "the first plan and eight more"
    );
    assert_eq!(stored(&store), ["a", "b"]);

    // The read of the victims after the conflict fails.
    store.no_hook();
    let failing = Arc::clone(&store);
    let mut once = true;
    store.on_get(move |fake| {
        if std::mem::take(&mut once) {
            let put = Txn::new().put(format!("{KEY_PREFIX}a"), one_run_row(10), Cond::Any);
            fake.txn(put).unwrap().unwrap();
            let put = Txn::new().put(format!("{KEY_PREFIX}b"), one_run_row(10), Cond::Any);
            fake.txn(put).unwrap().unwrap();
            TestStore::set(&failing.fail_get, true);
        }
    });
    let err = second.run_ended(&pk("c"), &run(), 10).unwrap_err();
    assert!(matches!(err, ProfileError::Store(_)), "{err}");
    assert_eq!(stored(&store), ["a", "b"]);
}
