//! The read of a host's silence from its `node/<host>` row, by its pool's grace.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Phase};
use js_jobs::{NodeHello, Retention, keys};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_state::{Cond, ManualClock, Store, StoreTime, Txn};
use js_types::{HostId, ProviderId, ResourceClass, Resources, Shape};

use super::{Silence, holding};
use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::clock::SharedClock;
use crate::job_api::read::view::millis;

/// The grace of the pool `farm`: shorter than the default grace of `burst`.
const FARM_GRACE: Duration = Duration::from_secs(10);

/// A pool named `name`, with a grace of `grace_ms` when one is given.
fn pool(name: &str, grace_ms: Option<u64>) -> PoolConfig {
    let resources = Resources::new(8000, 16 << 30, 0, None).unwrap();
    let shape = Shape::new(resources, BTreeMap::new());
    let provider = ProviderId::new(name).unwrap();
    let pool = PoolConfig::new(provider, ResourceClass::gpu_host(), shape, 4);
    match grace_ms {
        Some(grace_ms) => pool.with_timing(2_000, grace_ms).unwrap(),
        None => pool,
    }
}

/// A registry serving `burst` (the default grace) and `farm` ([`FARM_GRACE`]) on `clock`.
fn registry(clock: &Arc<ManualClock>) -> Registry {
    let shared = SharedClock::new(OneClock(Arc::clone(clock)));
    let placer = Arc::new(crate::sched::place::SchedPlacer::new(
        crate::sched::engine::Settings::default(),
        shared.clone(),
    ));
    let farm = pool("farm", Some(u64::try_from(FARM_GRACE.as_millis()).unwrap()));
    let pools = (pool("burst", None), vec![farm]);
    Registry::with_pools(pools, shared, SequentialSessions::new(), placer).unwrap()
}

/// The hello of a host of the pool `pool`.
fn hello_in(pool: &str) -> NodeHello {
    NodeHello {
        session: None,
        pool: pool.to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        agent_process: None,
        agent_version: None,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Asserts the silence each row gives, judged from its own `heard_at`: none while a row is heard
/// within its pool's grace (at exactly the grace too), the instant the grace ran out once it is
/// not; each pool's own grace (`farm` is silent while `burst` is still heard); the longest grace
/// for a pool this server does not serve; none for a host with no row and for a row this build
/// cannot read; and a store whose clock or reads fail fails the read. Catches a silence judged by
/// one grace for every pool, an off-by-one at the grace, a silence counted from the wrong
/// instant, an unreadable row that fails a whole listing, and a failure answered as no silence.
#[test]
fn each_row_is_silent_past_its_pools_grace_from_the_instant_it_ran_out() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let silence = Silence::of(registry(&clock));
    for (name, pool) in [
        ("h-burst", "burst"),
        ("h-farm", "farm"),
        ("h-else", "elsewhere"),
    ] {
        jobs.hello_node(&host(name), &hello_in(pool)).unwrap();
    }
    let txn = Txn::new().put(keys::node(&host("h-junk")), vec![0xff], Cond::Absent);
    store.txn(txn).expect("reaches").expect("writes");
    // Each write moves the store's clock on, so each row is judged from its own `heard_at`.
    let heard = |name: &str| {
        let row = jobs.read_node(&host(name)).unwrap().unwrap();
        row.record.heard_at
    };
    let (burst, farm, other) = (heard("h-burst"), heard("h-farm"), heard("h-else"));
    let to = |at: StoreTime| clock.advance(at.since(store.now().unwrap()));
    let hosts: BTreeSet<HostId> = ["h-burst", "h-farm", "h-else", "h-junk", "h-rowless"]
        .into_iter()
        .map(host)
        .collect();
    let read = || silence.read(&jobs, &hosts).unwrap();
    let default = Duration::from_millis(DEFAULT_GRACE_MS);
    let ms = Duration::from_millis(1);

    assert_eq!(read(), BTreeMap::new(), "just heard");
    to(farm.saturating_add(FARM_GRACE));
    assert_eq!(read(), BTreeMap::new(), "farm heard its grace ago");
    clock.advance(ms);
    let farm_missed = (host("h-farm"), millis(farm.saturating_add(FARM_GRACE)));
    assert_eq!(
        read(),
        BTreeMap::from([farm_missed.clone()]),
        "farm past its grace, burst and elsewhere heard within theirs"
    );
    to(burst.saturating_add(default));
    assert_eq!(
        read(),
        BTreeMap::from([farm_missed.clone()]),
        "burst heard the default grace ago"
    );
    to(other.saturating_add(default).saturating_add(ms));
    let expected = BTreeMap::from([
        (host("h-burst"), millis(burst.saturating_add(default))),
        (host("h-else"), millis(other.saturating_add(default))),
        farm_missed,
    ]);
    assert_eq!(read(), expected, "every readable row past its grace");

    store.fail_clock(true);
    assert!(silence.read(&jobs, &hosts).is_err(), "a clock that fails");
    store.fail_clock(false);
    store.fail_read_after(0);
    assert!(silence.read(&jobs, &hosts).is_err(), "a read that fails");
}

/// Asserts that the hosts read are those of the jobs that have not ended, each once. Catches a
/// silence read for an ended job's host and a job without a host.
#[test]
fn the_hosts_read_are_those_of_the_jobs_not_ended() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_store, jobs) = tables_on(clock, Retention::DEFAULT);
    let booked = crate::agent_api::job_rig::BOOKED;
    let spec = crate::agent_api::job_rig::spec;
    let (one, _) = crate::agent_api::job_rig::placed(&jobs, "j-one", "h-1", spec(), booked);
    let (two, _) = crate::agent_api::job_rig::placed(&jobs, "j-two", "h-1", spec(), booked);
    let mut ended = crate::agent_api::job_rig::placed(&jobs, "j-ended", "h-2", spec(), booked).0;
    ended.state = js_jobs::JobState::Cancelled;
    let mut hostless = one.clone();
    hostless.host = None;
    let hosts = holding([&one, &two, &ended, &hostless]);
    assert_eq!(hosts, BTreeSet::from([host("h-1")]));
}
