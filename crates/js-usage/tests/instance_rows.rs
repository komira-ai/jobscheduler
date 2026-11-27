//! The node series written by several server instances: a minute the hour's pack holds with
//! other figures goes into the writing instance's own row beside the pack, and a read folds
//! the pack and every instance row of the hour.
//!
//! Every test names the defect it would catch.

mod common;

use std::num::NonZeroUsize;

use common::{Wrapped, fake, host, on_both, put, row};
use js_state::Store;
use js_usage::store::{InstanceTag, NODE_PREFIX, instance_key, node_key};
use js_usage::{NODE_RETENTION_MINUTES, NodeMinute, NodeSeries, PruneStats, UsageError};

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("nonzero")
}

fn keys(store: &dyn Store) -> Vec<String> {
    store
        .scan_prefix(NODE_PREFIX)
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn tag(nth: u32) -> InstanceTag {
    InstanceTag::started(1_700_000_000_000, 42, nth)
}

/// `minute` as a second instance measured it: more CPU, less working set, one lease more.
fn other(minute: u64) -> NodeMinute {
    let first = row(minute);
    NodeMinute {
        cpu_millis: first.cpu_millis + 500,
        ws_max_bytes: first.ws_max_bytes - 1,
        leases: first.leases + 1,
        ..first
    }
}

/// The higher of each figure of [`row`] and [`other`].
fn folded(minute: u64) -> NodeMinute {
    NodeMinute {
        ws_max_bytes: row(minute).ws_max_bytes,
        ..other(minute)
    }
}

// Asserts that a minute two instances flushed keeps both parts: the first instance's goes
// into the hour's pack, the second's into its own row beside the pack, the pack keeps the
// first part (what a release before reads), and a read folds each figure to the higher of
// the two. Catches a later flush that replaces the minute (the first part lost), and a read
// that takes one part (a figure of the other lost) or sums them.
#[test]
fn a_minute_two_instances_flushed_keeps_both_parts() {
    on_both("two-parts", |store| {
        let (first, second) = (tag(0), tag(1));
        let h = host("n1");
        NodeSeries::new(store)
            .with_instance(&first)
            .append(&h, &[row(61), row(62)])
            .unwrap();
        NodeSeries::new(store)
            .with_instance(&second)
            .append(&h, &[other(62), other(63)])
            .unwrap();
        assert_eq!(
            keys(store),
            [node_key(&h, 1), instance_key(&h, 1, &second)],
            "the new minute 63 went into the pack, 62 into the second's row"
        );
        assert_eq!(
            instance_key(&h, 1, &second),
            "usage/node/n1/0000000001/1700000000000-42-1"
        );
        let pack = store.get(&node_key(&h, 1)).unwrap().unwrap().value;
        let pack = js_usage::minute::decode_hour(1, &pack).unwrap();
        assert_eq!(pack, [row(61), row(62), other(63)]);
        let reader = NodeSeries::new(store);
        assert_eq!(
            reader.range(&h, 0, 200, limit(10)).unwrap(),
            [row(61), folded(62), other(63)]
        );
    });
}

// Asserts that a write as an instance that changes nothing writes nothing: the minute the
// pack holds with the same figures (a flush retried after its reply was lost) costs no
// transaction and no instance row. Catches a retry that copies its own minute into its
// instance row, and an empty transaction committed.
#[test]
fn a_retried_flush_writes_nothing() {
    on_both("retry", |store| {
        let mine = tag(0);
        let h = host("n1");
        let series = NodeSeries::new(store).with_instance(&mine);
        series.append(&h, &[row(61), row(125)]).unwrap();
        let txns = store.commit_stats().txns;
        series.append(&h, &[row(61), row(125)]).unwrap();
        assert_eq!(store.commit_stats().txns, txns);
        assert_eq!(keys(store), [node_key(&h, 1), node_key(&h, 2)]);
    });
}

// Asserts that an instance's own row is merged by minute: a minute written there again is
// replaced with the newer figures and the row's other minutes stay, so a retried write of
// the row never counts a part twice; and that new minutes for the pack and parts for the own
// row are written in one transaction. Catches an own row overwritten whole, one that keeps
// the older figures, and a write per row.
#[test]
fn an_instance_row_merges_by_minute_in_one_transaction() {
    on_both("own-row", |store| {
        let mine = tag(3);
        let h = host("n1");
        NodeSeries::new(store)
            .append(&h, &[row(61), row(62)])
            .unwrap();
        let series = NodeSeries::new(store).with_instance(&mine);
        let txns = store.commit_stats().txns;
        series.append(&h, &[other(61), row(64)]).unwrap();
        assert_eq!(store.commit_stats().txns, txns + 1);
        let higher = NodeMinute {
            cpu_millis: other(62).cpu_millis + 1,
            ..other(62)
        };
        let again = NodeMinute {
            cpu_millis: other(61).cpu_millis + 1,
            ..other(61)
        };
        series.append(&h, &[higher, again]).unwrap();
        let own = store
            .get(&instance_key(&h, 1, &mine))
            .unwrap()
            .unwrap()
            .value;
        let own = js_usage::minute::decode_hour(1, &own).unwrap();
        assert_eq!(own, [again, higher], "61 replaced, 62 added, one row");
        assert_eq!(keys(store), [node_key(&h, 1), instance_key(&h, 1, &mine)]);
        let read = NodeSeries::new(store).range(&h, 0, 200, limit(10)).unwrap();
        let at_62 = NodeMinute {
            ws_max_bytes: row(62).ws_max_bytes,
            ..higher
        };
        let at_61 = NodeMinute {
            ws_max_bytes: row(61).ws_max_bytes,
            ..again
        };
        assert_eq!(read, [at_61, at_62, row(64)]);
    });
}

// Asserts that a read whose limit falls on an hour whose pack ends one page of the scan and
// whose instance row begins the next reads that next page, so the minute it answers is the
// fold of both: hour 0 has a pack alone and hours 1 to 40 a pack and an instance row each,
// so the first page of 64 rows ends with the pack of hour 32. Catches a read that stops on
// the count of minutes read, which answers hour 32 without its second part.
#[test]
fn a_limit_on_an_hour_split_over_two_pages_reads_its_instance_row() {
    on_both("split-page", |store| {
        let mine = tag(0);
        let h = host("n1");
        let packs: Vec<NodeMinute> = (0..=40).map(|hour| row(hour * 60)).collect();
        NodeSeries::new(store).append(&h, &packs).unwrap();
        let parts: Vec<NodeMinute> = (1..=40).map(|hour| other(hour * 60)).collect();
        NodeSeries::new(store)
            .with_instance(&mine)
            .append(&h, &parts)
            .unwrap();
        let read = NodeSeries::new(store)
            .range(&h, 0, u64::MAX, limit(33))
            .unwrap();
        assert_eq!(read.len(), 33);
        assert_eq!(read[0], row(0));
        assert_eq!(read[32], folded(32 * 60));
        let all = NodeSeries::new(store)
            .range(&h, 0, u64::MAX, limit(100))
            .unwrap();
        assert_eq!(all.len(), 41);
        assert_eq!(all[40], folded(40 * 60));
    });
}

// Asserts that the read of the last hour of a range includes that hour's instance rows and
// stops before the next hour's pack. Catches an upper bound at the hour's pack key, which
// leaves out the last hour's parts.
#[test]
fn the_last_hour_of_a_range_includes_its_instance_rows() {
    on_both("last-hour", |store| {
        let mine = tag(0);
        let h = host("n1");
        NodeSeries::new(store)
            .append(&h, &[row(61), row(121)])
            .unwrap();
        NodeSeries::new(store)
            .with_instance(&mine)
            .append(&h, &[other(61), other(121)])
            .unwrap();
        let read = NodeSeries::new(store).range(&h, 0, 119, limit(10)).unwrap();
        assert_eq!(read, [folded(61)]);
    });
}

// Asserts that an instance row that does not read is reported with its key by a range and
// by a write that needs the row, and left in place. Catches an instance row read as empty
// (and so overwritten), and a read that skips it.
#[test]
fn a_corrupt_instance_row_is_an_error() {
    on_both("corrupt-own", |store| {
        let mine = tag(0);
        let h = host("n1");
        NodeSeries::new(store).append(&h, &[row(61)]).unwrap();
        let key = instance_key(&h, 1, &mine);
        put(store, &key, b"\x02\x01");
        let series = NodeSeries::new(store).with_instance(&mine);
        let error = series.append(&h, &[other(61)]).unwrap_err();
        assert!(
            matches!(&error, UsageError::Corrupt { key: k, .. } if *k == key),
            "{error}"
        );
        let error = series.range(&h, 0, 200, limit(5)).unwrap_err();
        assert!(
            matches!(&error, UsageError::Corrupt { key: k, .. } if *k == key),
            "{error}"
        );
        assert_eq!(store.get(&key).unwrap().unwrap().value, b"\x02\x01");
    });
}

// Asserts that a write as an instance whose pack was rewritten between its read and the
// write fails with a conflict naming the pack, and that the part it would have put in its
// own row is not written either: the pack and the own row are one transaction. Catches a
// blind put of the pack, and an own row written apart from the pack.
#[test]
fn a_pack_changed_since_its_read_fails_the_instance_write_whole() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let mine = tag(0);
    let h = host("n1");
    NodeSeries::new(&store).append(&h, &[row(61)]).unwrap();
    let key = node_key(&h, 1);
    let theirs = js_usage::minute::encode_hour(&[(62, row(62))].into_iter().collect());
    *store.after_get.lock().unwrap() = Some((key.clone(), theirs.clone()));
    let error = NodeSeries::new(&store)
        .with_instance(&mine)
        .append(&h, &[other(61), row(63)])
        .unwrap_err();
    assert_eq!(error, UsageError::Conflict { key: key.clone() });
    assert_eq!(inner.get(&key).unwrap().unwrap().value, theirs);
    assert!(inner.get(&instance_key(&h, 1, &mine)).unwrap().is_none());
}

// Asserts that the prune deletes an instance row of an hour past retention with its pack,
// keeps those of the hour that holds the cutoff, and leaves alone and counts a key under the
// prefix that is neither a pack nor an instance row: a last component with a `-` and a
// letter, an instance tag after a component that is not an hour, and a 10-character hour
// with a letter. Catches a prune that leaves instance rows behind for ever, one that deletes
// them early, and one that reads a foreign key as a row.
#[test]
fn the_prune_deletes_old_instance_rows_and_leaves_foreign_keys() {
    on_both("prune-own", |store| {
        let mine = tag(0);
        let h = host("n1");
        let (old, kept) = (100, 101);
        NodeSeries::new(store)
            .append(&h, &[row(old * 60), row(kept * 60)])
            .unwrap();
        NodeSeries::new(store)
            .with_instance(&mine)
            .append(&h, &[other(old * 60), other(kept * 60)])
            .unwrap();
        let foreign = [
            format!("{}/x-1", node_key(&h, old)),
            format!("{NODE_PREFIX}n1/x/1-2-3"),
            format!("{NODE_PREFIX}n1/000000010x"),
        ];
        for key in &foreign {
            put(store, key, b"not a row");
        }
        let stats = NodeSeries::new(store)
            .prune(kept * 60 + NODE_RETENTION_MINUTES)
            .unwrap();
        assert_eq!(
            stats,
            PruneStats {
                scanned: 7,
                deleted: 2,
                rewritten: 0,
                unrecognized: 3,
            }
        );
        let mut left = keys(store);
        left.sort();
        let mut want = vec![node_key(&h, kept), instance_key(&h, kept, &mine)];
        want.extend(foreign);
        want.sort();
        assert_eq!(left, want);
    });
}
