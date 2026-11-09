//! The elastic scenarios, shared by the elastic family (`tests/elastic.rs`) and the golden
//! hashes (`tests/replay.rs`): the demand of queued jobs scales a pool of an elastic fake cloud up to its
//! instance limit and, once the work is done, back to no machine at all.
//!
//! Sizes, in agent requests a seed: at most [`MAX_INSTANCES`] machines beat every 5 s (the
//! registry's default for a launched host, which an elastic pool does not set) for at most
//! [`SETTLE`] (the work ends long before), 40 * 2,400 / 5 = 19,200 heartbeats, with the
//! hellos besides, under the 30,000 a seed of the cell's sizing rule. A machine's boot
//! time is virtual: its 120 s cost no wall time.

use std::collections::BTreeMap;
use std::time::Duration;

use js_elastic_fake::Billing;
use js_sim_cell::{
    AcquireAnswer, Cell, CloudCall, CloudEvent, EPOCH_MS, ElasticPool, Expect, HowEnded, OfferSpec,
    Scenario, ServerSpec, Shape, Work,
};

/// The pool's name.
pub const POOL: &str = "burst";
/// The most machines of the pool that may run at once.
pub const MAX_INSTANCES: u64 = 40;
/// How many whole-host jobs the client submits at once, at the start.
pub const RUNS: u32 = 100;
/// How long each run lasts.
pub const RUN: Duration = Duration::from_secs(300);
/// The quiet time at the end: ample for the last runs (in the third of three waves over
/// forty machines, done by about 1,400 s) and for every machine's idle time, drain and
/// release after them.
pub const SETTLE: Duration = Duration::from_secs(2_400);
/// The price of the cloud's one offer, in cents an hour.
pub const PRICE: u64 = 120;

/// What each machine is: the server's host with no GPU, with no capability beyond its resources,
/// since the pool's floor has none and the client asks for the shape's capabilities.
pub fn shape() -> Shape {
    Shape {
        capabilities: BTreeMap::new(),
        ..Shape::cpu_host()
    }
}

/// The cloud's one offer.
pub fn offers() -> Vec<OfferSpec> {
    vec![OfferSpec::new("sim_1x_a100", "zone-a", PRICE)]
}

/// [`RUNS`] whole-host jobs of [`RUN`] against `pool`, which is empty at the start: every
/// job succeeds, placed and released once, no more than the pool's instance limit runs at
/// any step (`instances-within-limits`), every machine is released once idle
/// (`launched-machines-settle`), the cloud runs none of them at the end, every machine the
/// cloud launched was the answer to an acquire id of its own ([`one_machine_per_acquire`]),
/// and the cloud's
/// own bill equals the bill recomputed from the launch and release times of the
/// recording wrapper's calls ([`bill_matches`]).
pub fn scales_up_and_back_to_zero(name: &str, pool: ElasticPool) -> Scenario {
    Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::elastic_pool(pool, shape()))
        .work(RUNS, Work::whole_host(RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(RUNS as usize, HowEnded::Succeeded))
        .expect(Expect::held_and_released(RUNS as usize))
        .expect_that("the cloud runs none of the pool's machines", none_running)
        .expect_that(
            "each acquire id launched one machine",
            one_machine_per_acquire,
        )
        .expect_that("the cloud's bill is the bill of its calls", bill_matches)
        .settle_within(SETTLE)
}

/// The EC2-like pool: billed per second, at least a minute.
pub fn an_ec2_like_pool() -> Scenario {
    let pool = ElasticPool::ec2_like(POOL, offers(), MAX_INSTANCES);
    scales_up_and_back_to_zero(
        "an_ec2_like_pool_scales_up_to_demand_and_back_to_zero",
        pool,
    )
}

/// The Lambda-like pool: each started minute billed whole, at least one.
pub fn a_lambda_like_pool() -> Scenario {
    let pool = ElasticPool::lambda_like(POOL, offers(), MAX_INSTANCES);
    let name = "a_lambda_like_pool_bills_whole_minutes_and_releases_idle_machines";
    scales_up_and_back_to_zero(name, pool)
}

/// Whether the cloud runs a machine at the end: what it runs, when it runs any.
pub fn none_running(cell: &mut Cell) -> Option<String> {
    let Some(cloud) = cell.cloud() else {
        return Some("the cell has no elastic pool".to_owned());
    };
    let running = cloud.fake().running();
    (!running.is_empty()).then(|| format!("it runs {} machines", running.len()))
}

/// Whether every machine the cloud launched was the answer to an acquire of an id of its
/// own, and no acquire id was answered with two machines (design section 7, "Scaler
/// double launch": acquire ids are idempotent): what differs, when anything does.
pub fn one_machine_per_acquire(cell: &mut Cell) -> Option<String> {
    let Some(cloud) = cell.cloud() else {
        return Some("the cell has no elastic pool".to_owned());
    };
    let mut machines: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for call in cloud.calls() {
        if let CloudEvent::Acquire {
            acquire,
            answer: AcquireAnswer::Accepted { machine },
            ..
        } = call.event
        {
            let launched = machines.entry(acquire).or_default();
            if !launched.contains(&machine) {
                launched.push(machine);
            }
        }
    }
    let twice = machines
        .values()
        .filter(|launched| launched.len() > 1)
        .count();
    let launched = cloud.fake().launched();
    let answered = machines.values().map(Vec::len).sum::<usize>();
    let differ = twice > 0 || launched != answered || launched == 0;
    differ.then(|| {
        format!("{twice} acquire ids launched two machines; the cloud launched {launched}, acquires were answered with {answered}")
    })
}

/// Whether the cloud's own bill, from the start to now (`ElasticFake::bill_between`, by its
/// billing), equals the bill recomputed from the launch and release times in the recording
/// wrapper's calls ([`recomputed`]): what each differs by, when they differ.
///
/// It checks the cell's clock plumbing, not the scheduler's cost: the cloud bills on its
/// own clock, and the calls are recorded at the virtual times of their steps, so the two
/// differ when the cloud's clock and the server's drift apart.
pub fn bill_matches(cell: &mut Cell) -> Option<String> {
    let now = cell.now().as_millis();
    let Some(cloud) = cell.cloud() else {
        return Some("the cell has no elastic pool".to_owned());
    };
    let billed = cloud.fake().bill_between(EPOCH_MS, EPOCH_MS + now).0;
    let calls = cloud.calls();
    let launches = calls.iter().filter(|call| accepted(call).is_some()).count();
    let ours = recomputed(&calls, cloud.pool().billing(), PRICE, now);
    let differ = billed != ours || launches == 0;
    differ.then(|| format!("the cloud billed {billed}, the calls of {launches} launches {ours}"))
}

/// The machine an acquire launched, for a call that is one.
fn accepted(call: &CloudCall) -> Option<&str> {
    match &call.event {
        CloudEvent::Acquire {
            answer: AcquireAnswer::Accepted { machine },
            ..
        } => Some(machine),
        _ => None,
    }
}

/// The bill, in cent-milliseconds, of the machines `calls` launched (the first acquire
/// answered with each) at `price` cents an hour, each from its launch to its release (the
/// first release the cloud stopped it at) or to `now`, billed as `billing` bills a run,
/// and counted up to `now`: the cloud's arithmetic, done again from the calls alone.
pub fn recomputed(calls: &[CloudCall], billing: Billing, price: u64, now: u64) -> u128 {
    let mut launched: BTreeMap<&str, u64> = BTreeMap::new();
    let mut released: BTreeMap<&str, u64> = BTreeMap::new();
    for call in calls {
        let at = call.at.as_millis();
        if let Some(machine) = accepted(call) {
            launched.entry(machine).or_insert(at);
        }
        if let CloudEvent::Release { machine, answer } = &call.event
            && answer.ended()
        {
            released.entry(machine.as_str()).or_insert(at);
        }
    }
    let owed = launched.iter().map(|(machine, &from)| {
        let until = released.get(machine).copied().unwrap_or(now);
        let billed = billed_ms(billing, until.saturating_sub(from));
        let stop = from.saturating_add(billed).min(now);
        u128::from(price) * u128::from(stop.saturating_sub(from))
    });
    owed.sum()
}

/// The time a run of `run_ms` is billed for: per second with a minute at the least (EC2),
/// or each started minute whole with one at the least (Lambda).
fn billed_ms(billing: Billing, run_ms: u64) -> u64 {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60_000;
    match billing {
        Billing::Aws => (run_ms.div_ceil(SECOND) * SECOND).max(MINUTE),
        Billing::Lambda => (run_ms.div_ceil(MINUTE) * MINUTE).max(MINUTE),
        other => panic!("the elastic family bills as EC2 or as Lambda, not {other:?}"),
    }
}
