//! The node series over the in-memory fake and over a real SQLite file.
//!
//! Every test names the defect it would catch. The error arms are driven by a store that
//! delegates and fails or interferes on cue (`common::Wrapped`).

mod common;

use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;

use common::{Wrapped, fake, host, on_both, put, row};
use js_state::Store;
use js_usage::store::{
    MAX_BUFFERED_PER_HOST, MAX_NODE_HOUR, MAX_NODE_MINUTE, NODE_PREFIX, node_key,
};
use js_usage::{NodeBuffer, NodeMinute, NodeSeries, UsageError};

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("nonzero")
}

fn minutes(rows: &[NodeMinute]) -> Vec<u64> {
    rows.iter().map(|r| r.minute).collect()
}

fn keys(store: &dyn Store) -> Vec<String> {
    store
        .scan_prefix(NODE_PREFIX)
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

// Asserts what is appended is read back with every field, in time order, across the hour
// 9 / hour 10 boundary: the keys are zero-padded, so hour 10 sorts after hour 9. Catches an
// unpadded hour (10 sorts before 9), a field mixed up in the row, and an unordered range.
#[test]
fn rows_round_trip_in_time_order() {
    on_both("round-trip", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let rows = [
            row(10 * 60 + 5),
            row(9 * 60 + 59),
            row(9 * 60),
            row(10 * 60),
        ];
        series.append(&h, &rows).unwrap();
        let got = series.range(&h, 0, u64::MAX, limit(100)).unwrap();
        assert_eq!(minutes(&got), vec![540, 599, 600, 605]);
        assert_eq!(got[0], row(540));
        assert_eq!(got[3], row(605));
        assert_eq!(keys(store), vec![node_key(&h, 9), node_key(&h, 10)],);
        assert_eq!(node_key(&h, 9), "usage/node/n1/0000000009");
    });
}

// Asserts the range is inclusive at both ends and clips inside an hour, that a reversed
// range is empty, and that the limit cuts the oldest-first answer; catches `<` for `<=`
// on either end, a range that returns whole hours, and a limit applied to hours.
#[test]
fn a_range_is_inclusive_and_clipped() {
    on_both("range", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let all: Vec<NodeMinute> = (120..240).map(row).collect();
        series.append(&h, &all).unwrap();
        assert_eq!(
            minutes(&series.range(&h, 130, 132, limit(10)).unwrap()),
            vec![130, 131, 132]
        );
        assert_eq!(
            minutes(&series.range(&h, 150, 150, limit(10)).unwrap()),
            vec![150]
        );
        assert_eq!(
            minutes(&series.range(&h, 119, 120, limit(10)).unwrap()),
            vec![120]
        );
        assert_eq!(
            minutes(&series.range(&h, 239, 500, limit(10)).unwrap()),
            vec![239]
        );
        assert!(series.range(&h, 132, 130, limit(10)).unwrap().is_empty());
        assert_eq!(
            minutes(&series.range(&h, 0, 999, limit(2)).unwrap()),
            vec![120, 121]
        );
        assert!(series.range(&h, 0, 119, limit(10)).unwrap().is_empty());
        assert!(series.range(&h, 240, 400, limit(10)).unwrap().is_empty());
    });
}

// Asserts a range over more hours than one page follows the resume token and stops as soon
// as the limit is met; catches a range that reads only the first page and one that keeps
// paging past the limit.
#[test]
fn a_long_range_pages_and_stops_at_its_limit() {
    on_both("paging", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let rows: Vec<NodeMinute> = (0..70).map(|hour| row(hour * 60 + 1)).collect();
        series.append(&h, &rows).unwrap();
        let all = series.range(&h, 0, u64::MAX, limit(1000)).unwrap();
        assert_eq!(all.len(), 70);
        assert_eq!(all[69].minute, 69 * 60 + 1);
        assert_eq!(series.range(&h, 0, u64::MAX, limit(66)).unwrap().len(), 66);
        assert_eq!(series.range(&h, 0, u64::MAX, limit(3)).unwrap().len(), 3);
    });
}

// Asserts one node's series does not include another's, including a host whose name starts
// with the other's; catches a prefix without the trailing separator.
#[test]
fn nodes_do_not_share_rows() {
    on_both("hosts", |store| {
        let series = NodeSeries::new(store);
        series.append(&host("a"), &[row(10)]).unwrap();
        series.append(&host("a-b"), &[row(20)]).unwrap();
        series.append(&host("a.b"), &[row(30)]).unwrap();
        assert_eq!(
            minutes(&series.range(&host("a"), 0, 99, limit(9)).unwrap()),
            vec![10]
        );
        assert_eq!(
            minutes(&series.range(&host("a-b"), 0, 99, limit(9)).unwrap()),
            vec![20]
        );
    });
}

// Asserts an append merges into the hour already stored, that a minute written again is
// replaced by the new figures, and that of two rows for one minute in one call the later
// wins; catches an append that overwrites the hour (losing earlier minutes) and one that
// keeps the old row.
#[test]
fn an_append_merges_and_a_repeat_replaces() {
    on_both("merge", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        series.append(&h, &[row(61), row(62)]).unwrap();
        let newer = NodeMinute {
            cpu_millis: 9999,
            ..row(62)
        };
        let latest = NodeMinute {
            cpu_millis: 4242,
            ..row(63)
        };
        series.append(&h, &[row(63), newer, latest]).unwrap();
        let got = series.range(&h, 0, 200, limit(10)).unwrap();
        assert_eq!(got, vec![row(61), newer, latest]);
        assert_eq!(keys(store).len(), 1);
    });
}

// Asserts an append of several hours is one transaction, an empty append writes nothing,
// and an hour of minutes is one key, not sixty. Catches a write per minute or per hour.
#[test]
fn an_append_is_one_transaction() {
    on_both("one-txn", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let before = store.commit_stats().txns;
        series.append(&h, &[]).unwrap();
        assert_eq!(store.commit_stats().txns, before);
        let rows: Vec<NodeMinute> = (0..180).map(row).collect();
        series.append(&h, &rows).unwrap();
        assert_eq!(store.commit_stats().txns, before + 1);
        assert_eq!(keys(store).len(), 3);
    });
}

// Asserts the last keyable minute is accepted and the next is refused before anything is
// written, even when valid rows precede it; catches an unchecked 10-digit overflow that
// would sort a long key out of order.
#[test]
fn a_minute_past_the_key_width_is_refused() {
    assert_eq!(MAX_NODE_MINUTE, MAX_NODE_HOUR * 60 + 59);
    on_both("range-limit", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let last = NodeMinute {
            minute: MAX_NODE_MINUTE,
            ..row(1)
        };
        series.append(&h, &[last]).unwrap();
        assert_eq!(
            series
                .range(&h, MAX_NODE_MINUTE, u64::MAX, limit(5))
                .unwrap(),
            vec![last]
        );
        let past = NodeMinute {
            minute: MAX_NODE_MINUTE + 1,
            ..row(1)
        };
        let refused = series.append(&host("n2"), &[row(5), past]).unwrap_err();
        assert_eq!(refused, UsageError::MinuteOutOfRange(MAX_NODE_MINUTE + 1));
        assert!(
            series
                .range(&host("n2"), 0, 100, limit(5))
                .unwrap()
                .is_empty()
        );
    });
}

// Asserts a stored value that does not read is reported with its key by both append and
// range, and left in place: a wrong version byte, a truncated value. Catches a reader that
// treats a bad row as empty (and so overwrites it).
#[test]
fn a_corrupt_row_is_an_error_and_is_left_alone() {
    on_both("corrupt", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        let key = node_key(&h, 1);
        for bad in [vec![2u8, 1, 0], vec![1u8, 1, 0, 0]] {
            put(store, &key, &bad);
            let mut error = series.append(&h, &[row(61)]).unwrap_err();
            assert!(
                matches!(&error, UsageError::Corrupt { key: k, .. } if *k == key),
                "{error}"
            );
            error = series.range(&h, 0, 200, limit(5)).unwrap_err();
            assert!(matches!(error, UsageError::Corrupt { .. }));
            assert_eq!(store.get(&key).unwrap().unwrap().value, bad);
        }
    });
}

// Asserts a key under the node prefix that is not an hour row is skipped by a range
// instead of failing it.
#[test]
fn a_foreign_key_inside_the_range_is_skipped() {
    on_both("foreign", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        series.append(&h, &[row(61)]).unwrap();
        put(store, &format!("{}x", node_key(&h, 1)), b"not a row");
        assert_eq!(
            minutes(&series.range(&h, 0, 200, limit(5)).unwrap()),
            vec![61]
        );
    });
}

// Asserts a second writer is detected: a row rewritten between the read and the write
// makes the append fail with a conflict naming the row, and the interloper's value stays;
// catches a blind put that would silently lose its minutes.
#[test]
fn a_second_writer_is_a_conflict() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let series = NodeSeries::new(&store);
    let h = host("n1");
    series.append(&h, &[row(61)]).unwrap();
    let key = node_key(&h, 1);
    let other = js_usage::minute::encode_hour(&[(62, row(62))].into_iter().collect());
    *store.after_get.lock().unwrap() = Some((key.clone(), other));
    let error = series.append(&h, &[row(63)]).unwrap_err();
    assert_eq!(error, UsageError::Conflict { key: key.clone() });
    assert_eq!(
        minutes(&series.range(&h, 0, 200, limit(5)).unwrap()),
        vec![62]
    );
    // A first write racing another first write conflicts as well.
    let key2 = node_key(&h, 5);
    let racing = js_usage::minute::encode_hour(&[(300, row(300))].into_iter().collect());
    *store.after_get.lock().unwrap() = Some((key2.clone(), racing));
    assert_eq!(
        series.append(&h, &[row(301)]).unwrap_err(),
        UsageError::Conflict { key: key2 }
    );
}

// Asserts a failing store surfaces as a store error from every call and changes nothing.
#[test]
fn a_failing_store_is_a_store_error() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let series = NodeSeries::new(&store);
    let h = host("n1");
    series.append(&h, &[row(61)]).unwrap();
    store.fail_get.store(true, Ordering::SeqCst);
    assert!(matches!(
        series.append(&h, &[row(62)]),
        Err(UsageError::Store(_))
    ));
    store.fail_get.store(false, Ordering::SeqCst);
    store.fail_batch.store(true, Ordering::SeqCst);
    assert!(matches!(
        series.append(&h, &[row(62)]),
        Err(UsageError::Store(_))
    ));
    store.fail_batch.store(false, Ordering::SeqCst);
    store.fail_scan.store(true, Ordering::SeqCst);
    assert!(matches!(
        series.range(&h, 0, 200, limit(5)),
        Err(UsageError::Store(_))
    ));
    store.fail_scan.store(false, Ordering::SeqCst);
    assert_eq!(
        minutes(&series.range(&h, 0, 200, limit(5)).unwrap()),
        vec![61]
    );
}

// Asserts the buffer writes what it holds once: after a flush it is empty and a second
// flush writes nothing new; catches a flush that forgets to clear (duplicate writes).
#[test]
fn a_flush_empties_the_buffer() {
    on_both("flush", |store| {
        let series = NodeSeries::new(store);
        let mut buffer = NodeBuffer::new();
        assert!(buffer.is_empty());
        buffer.push(host("a"), row(61));
        buffer.push(host("a"), row(62));
        buffer.push(host("b"), row(61));
        assert_eq!(buffer.len(), 3);
        assert!(!buffer.is_empty());
        assert_eq!(buffer.flush(&series).unwrap(), 3);
        assert_eq!(buffer.len(), 0);
        let txns = store.commit_stats().txns;
        assert_eq!(buffer.flush(&series).unwrap(), 0);
        assert_eq!(store.commit_stats().txns, txns);
        assert_eq!(
            minutes(&series.range(&host("a"), 0, 200, limit(9)).unwrap()),
            vec![61, 62]
        );
        assert_eq!(
            minutes(&series.range(&host("b"), 0, 200, limit(9)).unwrap()),
            vec![61]
        );
    });
}

// Asserts a failed flush keeps every minute for the next one and a later flush writes each
// once; catches a buffer cleared before the write is known to have succeeded.
#[test]
fn a_failed_flush_keeps_its_minutes() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let series = NodeSeries::new(&store);
    let mut buffer = NodeBuffer::new();
    buffer.push(host("a"), row(61));
    store.fail_batch.store(true, Ordering::SeqCst);
    assert!(matches!(buffer.flush(&series), Err(UsageError::Store(_))));
    assert_eq!(buffer.len(), 1);
    store.fail_batch.store(false, Ordering::SeqCst);
    assert_eq!(buffer.flush(&series).unwrap(), 1);
    assert_eq!(
        series.range(&host("a"), 0, 200, limit(9)).unwrap(),
        vec![row(61)]
    );
}

// Asserts one node whose stored row is corrupt does not stop the others from being written:
// its minutes stay buffered, the other node's are written, and the error is the first one.
#[test]
fn one_bad_node_does_not_block_the_rest() {
    let store = fake();
    let series = NodeSeries::new(&store);
    put(&store, &node_key(&host("a"), 1), b"\x09");
    let mut buffer = NodeBuffer::new();
    buffer.push(host("a"), row(61));
    buffer.push(host("b"), row(61));
    assert!(matches!(
        buffer.flush(&series),
        Err(UsageError::Corrupt { .. })
    ));
    assert_eq!(buffer.len(), 1);
    assert_eq!(
        series.range(&host("b"), 0, 200, limit(9)).unwrap(),
        vec![row(61)]
    );
}

// Asserts a node's buffer is capped while the store is down: the oldest minutes go, are
// counted, and the newest are the ones written; catches an unbounded buffer and one that
// drops the newest.
#[test]
fn the_buffer_drops_the_oldest_past_its_cap() {
    let store = fake();
    let series = NodeSeries::new(&store);
    let mut buffer = NodeBuffer::new();
    let total = u64::try_from(MAX_BUFFERED_PER_HOST).unwrap() + 3;
    for minute in 0..total {
        buffer.push(host("a"), row(minute));
    }
    assert_eq!(buffer.len(), MAX_BUFFERED_PER_HOST);
    assert_eq!(buffer.dropped(), 3);
    buffer.flush(&series).unwrap();
    let got = series.range(&host("a"), 0, u64::MAX, limit(5000)).unwrap();
    assert_eq!(got.first().map(|r| r.minute), Some(3));
    assert_eq!(got.last().map(|r| r.minute), Some(total - 1));
}
