//! The hosts' measured load in a walk over the queued jobs: which host a job goes to when
//! several fit, and what a host whose reading went stale is charged. Every host is of 8 cores
//! and 16 GiB (8800 m allowed at the default slack of 1.1) and holds four jobs that book one
//! core each; readings are fed with the farm time they arrive at, and the walk runs at a farm
//! time the test names.

use std::collections::BTreeMap;

use js_jobs::Needs;
use js_sched::RefusalKind;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Resources};

use super::Engine;
use super::jobs::OpenHost;
use super::rig::{Tables, at, engine, host};

const GIB: u64 = 1 << 30;

/// A reading numbered `seq`, taken at `seq * 5` seconds on the agent's clock, of a host whose
/// actions cgroup has used `used` millicores over each 5 s window since the agent started.
fn reading(seq: u64, used: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * used * 5_000,
        memory_working_set_bytes: GIB,
        ..NodeLoad::default()
    }
}

/// The engine over the hosts `names`, each holding the four one-core jobs `held-<n>`.
fn four_jobs_on(names: &[&str]) -> Engine {
    let seqs: Vec<Vec<u64>> = (0..names.len())
        .map(|i| (0..4).map(|j| (i * 4 + j) as u64 + 1).collect())
        .collect();
    let hosts: Vec<(&str, &[u64])> = names
        .iter()
        .zip(&seqs)
        .map(|(name, seqs)| (*name, seqs.as_slice()))
        .collect();
    engine(&hosts)
}

/// Tells the engine two readings of `name` at 5 s and 10 s farm time, so its newest window,
/// fresh at 10 s, measured `used` millicores.
fn measures(e: &mut Engine, name: &str, used: u64) {
    e.heard(&host(name), Some(&reading(1, used)), at(5_000));
    e.heard(&host(name), Some(&reading(2, used)), at(10_000));
}

/// Walks one new job `id` of `cpu` millicores and 1 GiB over the hosts `names` at `now_ms`:
/// the host it was placed on.
fn walk(e: &mut Engine, names: &[&str], id: &str, cpu: u64, now_ms: u64) -> Option<HostId> {
    let tables = Tables::new();
    let needs = Needs::Shape(Resources::new(cpu, GIB, 0, None).unwrap());
    let job = tables.submit_with(id, needs, 600);
    let open: BTreeMap<HostId, OpenHost> = names
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect();
    let (decided, _) = e.walk_jobs(&[job], &open, at(now_ms));
    decided.into_iter().next().map(|(_, host, _, _)| host)
}

/// Behaviour: two hosts book the same four cores and both fit a job of 200 m, but one
/// measures 8500 m (its jobs use more than they book) and the other 4000 m: the job goes to
/// the host with the lower measured load, whichever of the two that is, so the choice is the
/// measurement's and not the names' or the tie-break's. Catches placement that ranks by the
/// bookings alone, which piles work on a host already busier than its cores.
#[test]
fn of_two_hosts_that_fit_the_job_goes_to_the_one_measured_lower() {
    for (busy, idle) in [("a", "b"), ("b", "a")] {
        let mut e = four_jobs_on(&["a", "b"]);
        measures(&mut e, busy, 8_500);
        measures(&mut e, idle, 4_000);
        assert_eq!(e.load_of(busy).unwrap().left_cpu_millis, 8_500);
        assert_eq!(e.load_of(idle).unwrap().left_cpu_millis, 4_000);
        let chosen = walk(&mut e, &["a", "b"], "j1", 200, 10_000);
        assert_eq!(chosen, Some(host(idle)), "busy {busy}");
    }
}

/// Behaviour: a host measured above what it may carry (9000 m against 8800 m) takes no job,
/// however few cores its jobs book (4000 m), while a host measured at its bookings takes it;
/// with the busy host alone the job is refused for CPU, the measurement counted and carried in
/// the refusal. Catches a measurement that never enters the room check.
#[test]
fn a_host_measured_over_its_limit_takes_no_job_whatever_it_books() {
    let mut e = four_jobs_on(&["a", "b"]);
    measures(&mut e, "a", 9_000);
    measures(&mut e, "b", 4_000);
    assert_eq!(
        walk(&mut e, &["a", "b"], "j1", 200, 10_000),
        Some(host("b"))
    );
    assert_eq!(walk(&mut e, &["a"], "j2", 200, 10_000), None);
    let refused = e.take_refused();
    let (_, latest) = refused.latest(&host("a"), RefusalKind::Cpu).unwrap();
    assert_eq!(
        (
            latest.counted,
            latest.new,
            latest.limit,
            latest.measured_cpu_millis
        ),
        (9_000, 200, 8_800, Some(9_000))
    );
}

/// Behaviour: a host's reading is fresh for three windows (15 s) after it arrived. At 25 s
/// (exactly 15 s after the reading of 10 s) the host measured at 6000 m is still charged
/// 6000 m and refuses a job of 4000 m with that measurement; at 25.001 s the reading is stale
/// and the host is charged what its jobs book (4000 m: not zero, and not the stale
/// measurement), the refusal of a job of 5000 m carries no measurement, and a host with the
/// same bookings and a fresh reading is chosen before it. Catches a stale reading taken as no
/// load at all, and a refusal that prints a measurement the predicate did not use.
#[test]
fn a_stale_reading_charges_the_bookings_and_carries_no_measurement() {
    let mut e = four_jobs_on(&["a"]);
    measures(&mut e, "a", 6_000);
    assert_eq!(walk(&mut e, &["a"], "j1", 4_000, 25_000), None);
    let refused = e.take_refused();
    let (_, fresh) = refused.latest(&host("a"), RefusalKind::Cpu).unwrap();
    assert_eq!(
        (fresh.counted, fresh.measured_cpu_millis),
        (6_000, Some(6_000))
    );

    assert_eq!(walk(&mut e, &["a"], "j2", 5_000, 25_001), None);
    let view = e.load_of("a").unwrap();
    assert_eq!((view.fallback, view.left_cpu_millis), (true, 4_000));
    let refused = e.take_refused();
    let (_, stale) = refused.latest(&host("a"), RefusalKind::Cpu).unwrap();
    assert_eq!(
        (stale.counted, stale.new, stale.measured_cpu_millis),
        (4_000, 5_000, None)
    );

    for (stale, fresh) in [("a", "b"), ("b", "a")] {
        let mut e = four_jobs_on(&["a", "b"]);
        measures(&mut e, stale, 4_000);
        e.heard(&host(fresh), Some(&reading(1, 4_000)), at(20_000));
        e.heard(&host(fresh), Some(&reading(2, 4_000)), at(25_000));
        let chosen = walk(&mut e, &["a", "b"], "j3", 200, 30_000);
        assert_eq!(chosen, Some(host(fresh)), "stale {stale}");
    }
}
