//! What the scaler lets a cloud bill, measured from the cloud's side: the elastic fake keeps
//! its own bill from its own launch and release times, so these tests can fail on an
//! accounting bug in the scaler, which a test of the scaler against its own arithmetic
//! cannot.

mod cloud_rig;
mod common;

use cloud_rig::{DAY_MS, PRICE, Rng, Sim};
use common::{minutes, settings};
use js_elastic::HostRef;
use js_elastic_fake::{Billing, Call, Step};
use js_scaler::{Action, Blocked, LaunchMode, Settings};

/// The pool of these tests: four machines at most, the day capped at 600 cents (ten
/// machine-hours at a cent a minute), a host released after 5 quiet minutes.
fn capped() -> Settings {
    Settings {
        idle: minutes(5),
        max_spend_per_day: 600,
        ..settings("gpu")
    }
}

/// How long a machine may outlive the moment the day cap stops the scaler launching, in
/// minutes. Stated, not measured: it is the demand left in the burst that was being served
/// (at most `BURST` minutes), plus what a machine does after its demand has gone (it boots
/// for 2 minutes, is quiet for the 5 of `idle`, is drained, marked and released over 3
/// ticks, and a release may fail for 3 ticks in a row), which is under 15, rounded up so
/// that the bound is not tight. The quiet gap between bursts (`GAP`) is longer than that,
/// so no machine serves two bursts.
const LATENCY_MINUTES: u64 = 40;

/// The longest demand burst, in ticks (minutes).
const BURST: u64 = 20;

/// The quiet time forced after a burst, in ticks: longer than any machine's life after its
/// demand is gone, so every machine is released before the next burst.
const GAP: u64 = 20;

/// Days simulated by each seed.
const DAYS: u64 = 3;

/// Draws the fault of a call for one tick: working most of the time, and a stream of
/// failures of the kinds a cloud has, never more than `streak` in a row.
fn fault(rng: &mut Rng, run: &mut u64, streak: u64) -> Step {
    let failing = *run < streak && rng.one_in(5);
    *run = if failing { *run + 1 } else { 0 };
    match (failing, rng.below(3)) {
        (false, _) => Step::Pass,
        (true, 0) => Step::Lost {
            landed: rng.one_in(2),
        },
        (true, 1) => Step::Refuse(js_elastic::RefusalCode::Denied),
        (true, _) => Step::Throttled,
    }
}

/// The acquire outcomes of one run: a long script of refusals for capacity, quota, denial
/// and shape, answers that are lost (landed or not), 429s, and mostly successes.
fn acquire_script(rng: &mut Rng) -> Vec<Step> {
    use js_elastic::RefusalCode::{Capacity, Denied, Quota, Shape};
    (0..400)
        .map(|_| match rng.below(100) {
            0..=9 => Step::Refuse(Capacity),
            10..=13 => Step::Refuse(Quota),
            14..=16 => Step::Refuse(Denied),
            17..=19 => Step::Refuse(Shape),
            20..=24 => Step::Lost { landed: true },
            25..=28 => Step::Lost { landed: false },
            29..=38 => Step::Throttled,
            _ => Step::Pass,
        })
        .collect()
}

/// Asserts the spend bound, on the cloud's own bill, over random scripted outcomes. For 16
/// seeds, three simulated days each at one tick a minute, demand arrives in bursts of up to
/// four requests lasting up to 20 minutes with 20 quiet minutes after each; the cloud
/// refuses for capacity, quota, denial and shape, loses answers (landed or not), answers
/// 429, fails its list and its release for up to three ticks at a time, and preempts a
/// machine now and then. Two things are checked against the cloud's bill:
///
/// 1. at every acquire call, what the cloud had billed in the 24 hours before it is within
///    the day cap (`max_spend_per_day`), because the scaler must never launch with the cap
///    already spent;
/// 2. at every tick, what the cloud has billed in the 24 hours ending then is at most
///    `max_spend_per_day + max_instances x price x LATENCY_MINUTES / 60`, the day cap plus
///    what the machines running when the cap stopped the launches can still cost before
///    they are released. The bound is stated in the constants above: no machine outlives
///    its burst by more than the release latency, so after the cap bites at most
///    `max_instances` machines run on for at most that long.
///
/// It also requires that the cap did bite in the runs (a bound that nothing approaches
/// proves nothing) and that the cloud billed something. It catches an accounting bug that
/// lets the scaler launch past its cap: a machine left out of the day's sum, a billed
/// minimum not counted, a running machine that stops accruing, an unresolved launch not
/// counted.
#[test]
fn the_clouds_bill_over_any_day_stays_within_the_day_cap_plus_the_release_latency() {
    let settings = capped();
    let bound =
        settings.max_spend_per_day + settings.max_instances as u64 * PRICE * LATENCY_MINUTES / 60;
    let mut capped_at_least_once = false;
    let mut billed_most = 0;
    for seed in 0..16 {
        let mut rng = Rng::new(0x5EED_0000 + seed);
        let mut sim = Sim::new(settings.clone(), Billing::Lambda);
        sim.fake.script(Call::Acquire, &acquire_script(&mut rng));
        let (mut burst, mut quiet, mut requests) = (0, 0, 0);
        let (mut release_run, mut list_run) = (0, 0);
        for _ in 0..DAYS * 24 * 60 {
            if burst == 0 && quiet == 0 && rng.one_in(10) {
                burst = 1 + rng.below(BURST);
                requests = 1 + rng.below(4) as usize;
            }
            sim.waiting = if burst > 0 { requests } else { 0 };
            if burst > 0 {
                burst -= 1;
                quiet = if burst == 0 { GAP } else { 0 };
            } else {
                quiet = quiet.saturating_sub(1);
            }
            let step = fault(&mut rng, &mut release_run, 3);
            sim.fake.script_forever(Call::Release, step);
            let step = fault(&mut rng, &mut list_run, 3);
            sim.fake.script_forever(Call::List, step);
            if rng.one_in(500) {
                let running = sim.fake.running();
                if !running.is_empty() {
                    let pick = rng.below(running.len() as u64) as usize;
                    sim.fake.preempt(&running[pick]);
                }
            }
            sim.tick(None);
            capped_at_least_once |= sim.blocked() == Some(Blocked::MaxSpendDay);
            let now = sim.clock.now_ms();
            let day = sim.fake.bill_between(now.saturating_sub(DAY_MS), now);
            billed_most = billed_most.max(day.cents_up());
            assert!(
                day.cents_up() <= bound,
                "seed {seed}: the cloud billed {} cents in the day ending now, over the bound {bound}",
                day.cents_up()
            );
            let running: Vec<HostRef> = sim.fake.running();
            assert!(running.len() <= settings.max_instances, "seed {seed}");
        }
        for (n, window) in sim.window_at_acquire.iter().enumerate() {
            assert!(
                *window <= settings.max_spend_per_day,
                "seed {seed}: acquire {n} was sent with {window} cents already billed in the day"
            );
        }
        assert!(sim.sent > 20, "seed {seed}: only {} acquires", sim.sent);
    }
    assert!(capped_at_least_once, "the day cap never bit");
    assert!(
        billed_most > settings.max_spend_per_day / 2,
        "the cloud billed at most {billed_most} cents in a day: nothing approached the bound"
    );
}

/// Asserts the mirror of the bound: with a day cap too small for a single launch (the
/// least a launch can cost is `boot_timeout` at the price, 15 cents; the cap is 14) the
/// cloud is never asked for a machine, however long demand waits, and the pool says why
/// (`MaxSpendDay`). It catches a cap that lets a launch through when its least cost
/// does not fit.
#[test]
fn a_day_cap_below_the_least_cost_of_a_launch_launches_nothing() {
    let mut sim = Sim::new(
        Settings {
            max_spend_per_day: 14,
            ..capped()
        },
        Billing::Lambda,
    );
    sim.waiting = 2;
    sim.run(3 * 60);
    assert_eq!(sim.acquires(), 0);
    assert_eq!(sim.fake.launched(), 0);
    assert_eq!(sim.blocked(), Some(Blocked::MaxSpendDay));
    assert_eq!(sim.fake.bill(), js_elastic_fake::Bill::default());
}

/// Asserts the converse at the exact edge, against the cloud's bill: with a day cap of
/// 15 cents one launch fits (its least cost is 15), the machine runs and bills, and a second
/// launch is refused while the first is billed within the day. It catches a cap off by one
/// at the edge, and a first launch refused by a cap it fits.
#[test]
fn a_day_cap_equal_to_the_least_cost_of_a_launch_allows_exactly_one() {
    let mut sim = Sim::new(
        Settings {
            max_spend_per_day: 15,
            ..capped()
        },
        Billing::Lambda,
    );
    sim.waiting = 2;
    sim.run(3 * 60);
    assert_eq!(sim.fake.launched(), 1);
    assert_eq!(sim.blocked(), Some(Blocked::MaxSpendDay));
}

/// Asserts that a pool in plan mode asks the cloud for nothing, across random demand: for
/// 16 seeds and a simulated day each, with the cloud scripted to say yes to everything,
/// the step returns no acquire and no token to mint, and the cloud records no `acquire`
/// and no `release`, while the pool shows what it would have launched. The step is handed
/// the cloud's read handle's worth of calls only (the list and the unresolved ids), as the
/// server's plan mode is. It catches a plan mode that launches when demand is large, or
/// after a list failure, or with a stale cooldown.
#[test]
fn a_pool_in_plan_mode_makes_zero_create_calls_through_the_cloud() {
    let mut would_have_launched = 0;
    for seed in 0..16 {
        let mut rng = Rng::new(0xA11CE + seed);
        let plan = Settings {
            launch: LaunchMode::Plan,
            ..capped()
        };
        let mut sim = Sim::new(plan, Billing::Lambda);
        let handle = sim.fake.read_handle();
        for _ in 0..24 * 60 {
            sim.waiting = rng.below(6) as usize;
            let step = fault(&mut rng, &mut 0, 3);
            sim.fake.script_forever(Call::List, step);
            sim.rig.pool().held = handle.list().ok();
            sim.rig.pool().unresolved = handle.unresolved();
            sim.rig.input.waiting = (0..sim.waiting)
                .map(|n| common::request(&format!("r{n}")))
                .collect();
            let now = sim.now();
            let actions = sim.rig.tick(now);
            let spends = actions
                .iter()
                .any(|action| matches!(action, Action::Acquire(_) | Action::Mint { .. }));
            assert!(!spends, "seed {seed}: {actions:?}");
            would_have_launched += sim.rig.state.plan.len();
            sim.clock.advance(cloud_rig::TICK);
        }
        assert_eq!(sim.fake.count(Call::Acquire), 0, "seed {seed}");
        assert_eq!(sim.fake.count(Call::Release), 0, "seed {seed}");
        assert_eq!(sim.fake.launched(), 0, "seed {seed}");
    }
    assert!(would_have_launched > 100, "{would_have_launched}");
}
