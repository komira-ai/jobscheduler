//! Retention of the node series.

mod common;

use std::sync::atomic::Ordering;

use common::{Wrapped, fake, host, on_both, put, row};
use js_usage::store::{NODE_PREFIX, node_key};
use js_usage::{NODE_RETENTION_MINUTES, NodeSeries, PruneStats, UsageError};

const H: u64 = 5_000;

fn stored_hours(store: &dyn js_state::Store, h: &str) -> Vec<String> {
    let prefix = format!("{NODE_PREFIX}{h}/");
    store
        .scan_prefix(&prefix)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k[prefix.len()..].to_owned())
        .collect()
}

// Asserts an hour is deleted only when its last minute is before the cutoff (now less 14
// days): with the cutoff on an hour boundary the earlier hour goes and that hour stays, and
// one minute earlier the earlier hour stays too. Catches `<=` for `<` (the boundary hour
// lost a minute early) and a cutoff off by one.
#[test]
fn the_hour_holding_the_cutoff_is_kept_until_it_is_entirely_older() {
    assert_eq!(NODE_RETENTION_MINUTES, 14 * 24 * 60);
    on_both("prune-edge", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        series
            .append(
                &h,
                &[row((H - 1) * 60 + 59), row(H * 60), row((H - 2) * 60)],
            )
            .unwrap();
        // Cutoff = the first minute of hour H - 1: hour H - 2 is entirely older, H - 1 is not.
        let stats = series.prune((H - 1) * 60 + NODE_RETENTION_MINUTES).unwrap();
        assert_eq!(
            stats,
            PruneStats {
                scanned: 3,
                deleted: 1,
                ..PruneStats::default()
            }
        );
        assert_eq!(stored_hours(store, "n1").len(), 2);
        // Cutoff = the first minute of hour H: hour H - 1 ends one minute before it.
        let stats = series.prune(H * 60 + NODE_RETENTION_MINUTES).unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(stored_hours(store, "n1"), vec![format!("{H:010}")]);
    });
    on_both("prune-edge-2", |store| {
        let series = NodeSeries::new(store);
        let h = host("n1");
        series
            .append(&h, &[row((H - 1) * 60 + 59), row(H * 60)])
            .unwrap();
        let stats = series.prune(H * 60 + NODE_RETENTION_MINUTES - 1).unwrap();
        assert_eq!(stats.deleted, 0);
        assert_eq!(stored_hours(store, "n1").len(), 2);
    });
}

// Asserts a clock earlier than the retention (the cutoff would be negative) deletes
// nothing; catches an unchecked subtraction.
#[test]
fn an_early_clock_deletes_nothing() {
    on_both("prune-early", |store| {
        let series = NodeSeries::new(store);
        series.append(&host("n1"), &[row(0)]).unwrap();
        assert_eq!(series.prune(100).unwrap().deleted, 0);
        assert_eq!(stored_hours(store, "n1").len(), 1);
    });
}

// Asserts pruning covers every node, more rows than one scan page, is repeatable, and
// touches nothing outside the node prefix; the stray key under the prefix is counted and
// left. Catches a prune of one node only, one that stops after the first page, and a prefix
// that is too wide.
#[test]
fn prune_covers_every_node_and_page_and_only_its_prefix() {
    on_both("prune-all", |store| {
        let series = NodeSeries::new(store);
        let old: Vec<_> = (0..300).map(|hour| row(hour * 60)).collect();
        series.append(&host("a"), &old).unwrap();
        series.append(&host("b"), &old[..5]).unwrap();
        series.append(&host("b"), &[row(H * 60)]).unwrap();
        put(store, "usage/node/stray", b"?");
        // Ten characters, one of them not a digit: the length alone does not make an hour.
        put(store, "usage/node/b/000000000x", b"?");
        put(store, "usage/key/old", b"profile");
        let stats = series.prune(H * 60 + NODE_RETENTION_MINUTES).unwrap();
        assert_eq!(
            stats,
            PruneStats {
                scanned: 307,
                deleted: 305,
                rewritten: 0,
                unrecognized: 1
            }
        );
        assert_eq!(stored_hours(store, "a").len(), 0);
        assert_eq!(stored_hours(store, "b").len(), 1);
        assert!(store.get("usage/key/old").unwrap().is_some());
        assert!(store.get("usage/node/stray").unwrap().is_some());
        let again = series.prune(H * 60 + NODE_RETENTION_MINUTES).unwrap();
        assert_eq!(
            again,
            PruneStats {
                scanned: 2,
                deleted: 0,
                rewritten: 0,
                unrecognized: 1
            }
        );
    });
}

// Asserts pruning an empty store is a no-op that reads nothing it must not.
#[test]
fn prune_of_nothing_is_nothing() {
    on_both("prune-empty", |store| {
        let stats = NodeSeries::new(store).prune(u64::MAX).unwrap();
        assert_eq!(stats, PruneStats::default());
    });
}

// Asserts a row rewritten between the scan and the delete is not deleted (its version
// moved) and is counted; catches an unconditional delete that would drop fresh minutes.
#[test]
fn a_row_rewritten_after_the_scan_is_kept() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let series = NodeSeries::new(&store);
    let h = host("n1");
    series.append(&h, &[row(0)]).unwrap();
    let key = node_key(&h, 0);
    let rewritten = js_usage::minute::encode_hour(&[(1, row(1))].into_iter().collect());
    *store.after_scan.lock().unwrap() = Some((key, rewritten));
    let stats = series.prune(H * 60 + NODE_RETENTION_MINUTES).unwrap();
    assert_eq!((stats.deleted, stats.rewritten), (0, 1));
    assert_eq!(stored_hours(&inner, "n1").len(), 1);
}

// Asserts a failing scan or delete is a store error.
#[test]
fn prune_surfaces_store_failures() {
    let inner = fake();
    let store = Wrapped::new(&inner);
    let series = NodeSeries::new(&store);
    series.append(&host("n1"), &[row(0)]).unwrap();
    store.fail_scan.store(true, Ordering::SeqCst);
    assert!(matches!(
        series.prune(H * 60 + NODE_RETENTION_MINUTES),
        Err(UsageError::Store(_))
    ));
    store.fail_scan.store(false, Ordering::SeqCst);
    store.fail_batch.store(true, Ordering::SeqCst);
    assert!(matches!(
        series.prune(H * 60 + NODE_RETENTION_MINUTES),
        Err(UsageError::Store(_))
    ));
    assert_eq!(stored_hours(&inner, "n1").len(), 1);
}
