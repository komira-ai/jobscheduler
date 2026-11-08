//! The tail counters over the in-memory fake and over a real SQLite file: every scenario of
//! `tail_cases` once on each, and the checks that need no store.
//!
//! Every test names the defect it would catch. Nothing sleeps and no test reads a clock.

mod tail_cases;

use std::sync::Arc;

use js_state::ManualClock;
use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_testkit::dir::TestDir;
use js_usage::{NoTail, TAIL_BUCKETS, TailDelta, TailSink, TailSinkCounters, TailTotals};
use tail_cases::{Hooked, every, totals};

/// A fresh, empty in-memory store.
fn fake() -> Arc<Hooked> {
    let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    Hooked::over(Arc::new(fake), None)
}

/// A fresh, empty SQLite store in a scratch directory of its own.
fn sqlite() -> Arc<Hooked> {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-usage-tail");
    let clock = Arc::new(ManualClock::default());
    let store = SqliteStore::open(&dir.path().join("state.db"), clock, Options::default())
        .expect("open the scratch store");
    Hooked::over(Arc::new(store), Some(Box::new(dir)))
}

/// Every scenario on the in-memory fake.
mod on_fake {
    use super::fake;
    crate::tail_store_tests!(fake);
}

/// Every scenario on SQLite.
mod on_sqlite {
    use super::sqlite;
    crate::tail_store_tests!(sqlite);
}

/// Asserts that deltas merge and totals add, counter by counter, saturating. Catches: a
/// merge that keeps only the last delta, and a field left out of the sum.
#[test]
fn deltas_merge_and_totals_add_by_counter() {
    let mut d = every();
    d.merge(&every());
    let mut t = totals(&every());
    t.add(&every());
    assert_eq!(TailTotals::default().error_buckets.len(), TAIL_BUCKETS);
    assert_eq!(d.error_buckets, [2, 4, 6, 8, 10, 12, 14, 16]);
    assert_eq!(t.error_buckets, d.error_buckets);
    assert_eq!((d.started_on_marked, d.error_sum_ms), (6, -80_000));
    assert_eq!(t, totals(&d));
    assert!(!d.is_empty());
    assert!(TailDelta::default().is_empty());
}

/// Asserts that the sink that keeps nothing loads zero, takes every delta and counts
/// nothing. Catches: a default sink that fails (a registry with no store would then report
/// a load failure) or that counts a fault on every delta.
#[test]
fn the_sink_that_keeps_nothing_loads_zero_and_takes_every_delta() {
    assert_eq!(NoTail.load(), Ok(TailTotals::default()));
    NoTail.add(&every());
    assert_eq!(NoTail.counters(), TailSinkCounters::default());
}
