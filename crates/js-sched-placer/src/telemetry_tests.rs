//! What `/metrics` reads of the placement ([`Telemetry`]): the buckets of a ratio histogram, and
//! what the engine and the placer give of their hosts and counters. The fit and the checks the
//! telemetry carries are read in `sizes_fit_tests`.

use js_registry::Placer as _;
use js_sched::LoadCounters;
use js_types::sched::FarmTime;
use js_types::sched::WorkerId;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Resources, Shape};

use super::{CurveChecks, FIT_BOUNDS_PERMILLE, RatioHistogram, RunFit, Telemetry};
use crate::engine::{Engine, Settings};
use crate::place::SchedPlacer;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// A shape of 8 cores and 16 GiB, with an `arch` when `arch` says so (without one it has no
/// scheduler form, and its host is no worker).
fn shape(arch: bool) -> Shape {
    let caps = arch
        .then(|| ("arch".to_owned(), "x86_64".to_owned()))
        .into_iter()
        .collect();
    Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
}

/// A load reading of a host, the `seq`-th of its agent, of a host that used 500 millicores.
fn reading(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 2_500_000,
        memory_working_set_bytes: 1 << 30,
        ..NodeLoad::default()
    }
}

/// Behaviour: an observation exactly at a bound is in that bound's bucket, one thousandth above
/// it in the next, 0 in the first and one above the last bound in the last bucket (the one
/// past the nine bounds); each is counted once, and the sum is the sum of the observations.
/// Catches a bound that is exclusive, and a value placed a bucket off.
#[test]
fn an_observation_is_in_the_bucket_of_the_first_bound_it_does_not_pass() {
    for (index, bound) in FIT_BOUNDS_PERMILLE.iter().enumerate() {
        let mut exact = RatioHistogram::default();
        exact.observe(*bound);
        assert_eq!(exact.counts[index], 1, "at {bound}");
        assert_eq!(exact.count(), 1, "at {bound}");
        let mut above = RatioHistogram::default();
        above.observe(bound + 1);
        assert_eq!(above.counts[index + 1], 1, "above {bound}");
        assert_eq!(above.count(), 1, "above {bound}");
    }
    let mut ratios = RatioHistogram::default();
    ratios.observe(0);
    ratios.observe(5000);
    ratios.observe(5001);
    assert_eq!(ratios.counts, [1, 0, 0, 0, 0, 0, 0, 0, 1, 1]);
    assert_eq!(ratios.count(), 3);
    assert_eq!(ratios.sum_permille, 10_001);
}

/// Behaviour: the sum stops at `u64::MAX` and the count goes on. Catches a sum that wraps to a
/// small one.
#[test]
fn the_sum_of_a_histogram_stops_at_the_largest_number() {
    let mut ratios = RatioHistogram::default();
    ratios.observe(u64::MAX);
    ratios.observe(7);
    assert_eq!(ratios.sum_permille, u64::MAX);
    assert_eq!(ratios.counts[9], 1);
    assert_eq!(ratios.count(), 2);
}

/// Behaviour: an engine over no host reads as the telemetry of nothing: no counter, no host, no
/// fit, no check. Catches a telemetry that starts with figures.
#[test]
fn an_engine_with_no_host_reads_as_nothing() {
    let engine = Engine::build(Settings::default(), at(0));
    let telemetry = engine.telemetry();
    assert_eq!(telemetry, Telemetry::default());
    assert_eq!(telemetry.counters, LoadCounters::default());
    assert_eq!(telemetry.fit, RunFit::default());
    assert_eq!(telemetry.checks, CurveChecks::default());
}

/// Behaviour: a host that said hello and gave two readings shows the CPU and the bookable
/// memory the scheduler holds for it and the load view it charges it by, and the counters are
/// the scheduler's, with the two readings accepted; a host whose shape has no scheduler form
/// is no worker and is absent. Catches a capacity read in another unit, a view of another
/// host, and a host with no worker listed.
#[test]
fn a_host_up_with_readings_shows_its_capacity_and_its_load_and_a_host_with_no_worker_is_absent() {
    let mut engine = Engine::build(Settings::default(), at(0));
    engine.host_up(&host("a"), &shape(true), at(0));
    engine.host_up(&host("b"), &shape(true), at(0));
    engine.host_up(&host("none"), &shape(false), at(0));
    engine.heard(&host("a"), Some(&reading(1)), at(5_000));
    engine.heard(&host("a"), Some(&reading(2)), at(10_000));
    let telemetry = engine.telemetry();
    let hosts: Vec<&HostId> = telemetry.hosts.keys().collect();
    assert_eq!(hosts, [&host("a"), &host("b")]);
    let capacity = engine.sched().capacity(&WorkerId::new("a")).unwrap();
    let a = telemetry.hosts[&host("a")];
    assert_eq!(a.cpu_capacity_millis, 8000);
    assert_eq!(a.memory_capacity_bytes, capacity.memory_bytes);
    assert!(a.memory_capacity_bytes > 0);
    assert_eq!(Some(a.load), engine.load_of("a"));
    assert_eq!(Some(telemetry.hosts[&host("b")].load), engine.load_of("b"));
    assert_ne!(telemetry.hosts[&host("b")].load, a.load);
    assert_eq!(telemetry.counters, engine.sched().load_counters());
    assert!(telemetry.counters.readings_accepted > 0);
}

/// Behaviour: the placer's telemetry is its engine's: a host up and a reading it was told show
/// in it, with the capacity of the host's shape. Catches a placer that reads another engine or
/// none.
#[test]
fn the_placer_gives_the_telemetry_of_its_engine() {
    let placer = SchedPlacer::default();
    assert!(placer.telemetry().hosts.is_empty());
    placer.host_up(&host("h1"), &shape(true));
    placer.heard(&host("h1"), Some(&reading(1)));
    let telemetry = placer.telemetry();
    assert_eq!(telemetry, placer.engine().telemetry());
    assert_eq!(telemetry.hosts[&host("h1")].cpu_capacity_millis, 8000);
    assert_eq!(telemetry.hosts.len(), 1);
}
