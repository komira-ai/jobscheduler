//! The curve source a configuration hands the scheduler: a core with none places as it always
//! did, a core with one lets the scheduler read it, and every core built from the same
//! configuration (a second frontend, a restart over the same store, a clone of the
//! configuration) holds the same source.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_server_core::{Config, Core, CurveSource};
use js_types::sched::WorkKey;
use js_types::sched::usage::{Curve, MinutePoint};

use crate::common::{Rig, at, block_on, core_by, submit, up};
use crate::stored::Setup;

/// A source that has a one-minute curve for every key and counts the keys it was asked about.
#[derive(Default)]
struct Spy {
    asked: AtomicUsize,
}

impl Spy {
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl CurveSource for Spy {
    fn curve(&self, _: &WorkKey) -> Option<Curve> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let point = MinutePoint {
            cores_milli: 500,
            working_set_bytes: 1 << 30,
        };
        Some(Curve::new(vec![point], 5).expect("a short curve"))
    }
}

/// How many actions the scheduler charged flat though their key has a curve, after `core`
/// admits action `n` onto a new node.
fn flat_with_curve(core: &Core<Rig>, base: u64, n: u64) -> u64 {
    up(core, base, "node-a", "d1");
    submit(core, base + 1, n);
    block_on(core.tick(at(base + 1_000)));
    core.with(|view, _| view.sched().load_counters().curve_flat)
}

/// Catches: a configuration that gives the scheduler curves by default: a core built without
/// a source reads none, so no key counts as having a curve.
#[test]
fn a_core_without_a_curve_source_places_as_before() {
    let core = core_by(&Config::new(1));
    assert_eq!(flat_with_curve(&core, 0, 1), 0);
    core.with(|view, _| assert_eq!(view.sched().load_counters().admitted, 1));
}

/// Catches: `Config::with_curves` that never reaches the scheduler: the source is asked for
/// the key of the action placed, and the scheduler counts that key as having a curve.
#[test]
fn a_core_with_a_curve_source_lets_the_scheduler_read_it() {
    let spy = Arc::new(Spy::default());
    let core = core_by(&Config::new(1).with_curves(spy.clone()));
    assert_eq!(spy.asked(), 0, "nothing is read before a placement");
    assert_eq!(flat_with_curve(&core, 0, 1), 1);
    assert!(spy.asked() > 0);
}

/// Catches: a core opened over a state store that builds its scheduler without the source,
/// and a restart or a second frontend over the same store that does the same: each core
/// built from one configuration reads the one source.
#[test]
fn every_core_built_from_a_configuration_reads_its_source() {
    let spy = Arc::new(Spy::default());
    let config = Config::new(1).with_curves(spy.clone());
    let setup = Setup::new();

    let first = setup.core_with(&config, Rig::default(), 0);
    assert_eq!(flat_with_curve(&first, 0, 1), 1, "opened over a store");
    let seen = spy.asked();
    assert!(seen > 0);
    drop(first);

    let restarted = setup.core_with(&config, Rig::default(), 5_000);
    assert_eq!(flat_with_curve(&restarted, 5_000, 2), 1, "after a restart");
    assert!(spy.asked() > seen);
    let seen = spy.asked();

    let second = setup.core_with(&config.clone(), Rig::default(), 6_000);
    assert_eq!(
        flat_with_curve(&second, 6_000, 3),
        1,
        "a clone of the configuration"
    );
    assert!(spy.asked() > seen);
}

/// Catches: a configuration whose comparison ignores the source (two cores that read
/// different curves would be built alike), or one that tells equal sources apart, and a
/// `Debug` that fails for a source that has none.
#[test]
fn configurations_are_equal_when_they_share_a_source() {
    let spy: Arc<dyn CurveSource + Send + Sync> = Arc::new(Spy::default());
    let with = Config::new(1).with_curves(Arc::clone(&spy));
    assert_eq!(with, with.clone());
    assert_eq!(with, Config::new(1).with_curves(spy));
    assert_ne!(with, Config::new(1).with_curves(Arc::new(Spy::default())));
    assert_ne!(with, Config::new(1));
    assert!(format!("{with:?}").contains("Curves"));
}
