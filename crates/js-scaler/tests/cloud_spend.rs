//! What the scaler lets a cloud bill, measured from the cloud's side: the elastic fake keeps
//! its own bill from its own launch and release times, so these tests can fail on an
//! accounting bug in the scaler, which a test of the scaler against its own arithmetic
//! cannot.

mod cloud_rig;
mod common;

use cloud_rig::{DAY_MS, PRICE, Rng, Sim};
use common::{minutes, settings};
use js_elastic::{Acquired, HostRef, RefusalCode};
use js_elastic_fake::{Billing, Call, Step};
use js_scaler::{Blocked, LaunchMode, Settings};

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
/// Left out on purpose: a list that lags the truth and restarts that lose records. The
/// bound's latency does not cover them (a list that lags longer than `unknown_hold` makes
/// a second machine, see `cloud_stuck.rs`, and an orphan is counted only from the moment it
/// is found, see `cloud_crash.rs`), so those cases are pinned one by one in those files and
/// not folded into this bound. Odd seeds bill as AWS does (per second), even ones as Lambda.
///
/// It also requires that, in every seed, the cap refused a launch and launches were sent
/// with at least half the day's cap already billed, that every outcome the generator
/// scripts really happened somewhere across the seeds, and that the cloud billed more than
/// half the cap in some day (a bound that nothing approaches proves nothing). It catches an accounting bug that
/// lets the scaler launch past its cap: a machine left out of the day's sum, a billed
/// minimum not counted, a running machine that stops accruing, an unresolved launch not
/// counted.
#[test]
fn the_clouds_bill_over_any_day_stays_within_the_day_cap_plus_the_release_latency() {
    let settings = capped();
    let bound =
        settings.max_spend_per_day + settings.max_instances as u64 * PRICE * LATENCY_MINUTES / 60;
    let mut billed_most = 0;
    // How often each outcome of the generator really happened, over all seeds: a generator
    // that silently stopped producing one would make the bound hold for the wrong reason.
    let mut seen = Seen::default();
    for seed in 0..16 {
        let mut rng = Rng::new(0x5EED_0000 + seed);
        // Lambda bills by the minute and AWS by the second; the bound holds for both.
        let billing = if seed % 2 == 0 {
            Billing::Lambda
        } else {
            Billing::Aws
        };
        let mut sim = Sim::new(settings.clone(), billing);
        sim.fake.script(Call::Acquire, &acquire_script(&mut rng));
        let (mut burst, mut quiet, mut requests) = (0, 0, 0);
        let (mut release_run, mut list_run) = (0, 0);
        let mut capped = false;
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
            let release_step = fault(&mut rng, &mut release_run, 3);
            sim.fake.script_forever(Call::Release, release_step);
            let step = fault(&mut rng, &mut list_run, 3);
            sim.fake.script_forever(Call::List, step);
            if rng.one_in(500) {
                let running = sim.fake.running();
                if !running.is_empty() {
                    let pick = rng.below(running.len() as u64) as usize;
                    sim.fake.preempt(&running[pick]);
                    seen.preemptions += 1;
                }
            }
            let released_before = sim.fake.count(Call::Release);
            sim.tick(None);
            let released = sim.fake.count(Call::Release) > released_before;
            seen.release_faults += usize::from(released && release_step != Step::Pass);
            seen.list_failed += usize::from(sim.blocked() == Some(Blocked::ListFailed));
            capped |= sim.blocked() == Some(Blocked::MaxSpendDay);
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
        // The cap must have refused a launch in this very seed, and launches must have been
        // sent with the day half spent, or this seed never approached the bound.
        let peak = sim.window_at_acquire.iter().max().copied().unwrap_or(0);
        assert!(capped, "seed {seed}: the day cap never refused a launch");
        assert!(
            peak >= settings.max_spend_per_day / 2,
            "seed {seed}: no launch was sent with more than {peak} cents billed in the day"
        );
        seen.add(&sim);
    }
    assert!(
        billed_most > settings.max_spend_per_day / 2,
        "the cloud billed at most {billed_most} cents in a day: nothing approached the bound"
    );
    seen.assert_all_happened();
}

/// How often each outcome the generator scripts really happened.
#[derive(Default)]
struct Seen {
    capacity: usize,
    quota: usize,
    denied: usize,
    shape: usize,
    lost_landed: usize,
    lost_unlanded: usize,
    throttled: usize,
    list_failed: usize,
    release_faults: usize,
    preemptions: usize,
}

impl Seen {
    /// Adds what the answers of `sim` show.
    fn add(&mut self, sim: &Sim) {
        let refused = |wanted: RefusalCode| {
            let each = sim.answers.iter();
            each.filter(|answer| matches!(answer, Acquired::Refused(r) if r.code == wanted))
                .count()
        };
        self.capacity += refused(RefusalCode::Capacity);
        self.quota += refused(RefusalCode::Quota);
        self.denied += refused(RefusalCode::Denied);
        self.shape += refused(RefusalCode::Shape);
        self.lost_landed += sim.lost_landed;
        self.lost_unlanded += sim.lost_unlanded;
        let deferred = sim.answers.iter();
        self.throttled += deferred
            .filter(|answer| matches!(answer, Acquired::Deferred { .. }))
            .count();
    }

    fn assert_all_happened(&self) {
        let each = [
            ("capacity refusals", self.capacity),
            ("quota refusals", self.quota),
            ("denials", self.denied),
            ("shape refusals", self.shape),
            ("lost answers that landed", self.lost_landed),
            ("lost answers that did not", self.lost_unlanded),
            ("429s", self.throttled),
            ("ticks with the list failing", self.list_failed),
            ("failing releases", self.release_faults),
            ("preemptions", self.preemptions),
        ];
        for (what, count) in each {
            assert!(count > 0, "the generator produced no {what}");
        }
    }
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

/// Asserts that a pool in plan mode asks the cloud for nothing, across random demand: for 16
/// seeds and a simulated day each, with the cloud scripted to say yes to everything and its
/// list failing now and then, the simulation runs every action the step returns against the
/// cloud (`Sim::tick`), so an acquire, a release or a token the step wanted would reach it.
/// The cloud records no `acquire`, no `release` and no machine, while the pool shows what
/// it would have launched (more than a hundred plan lines in all, so the demand was real).
/// It catches a plan mode that launches when demand is large, after a list failure, or with
/// a stale cooldown.
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
        let mut list_run = 0;
        for _ in 0..24 * 60 {
            sim.waiting = rng.below(6) as usize;
            let step = fault(&mut rng, &mut list_run, 3);
            sim.fake.script_forever(Call::List, step);
            sim.tick(None);
            would_have_launched += sim.rig.state.plan.len();
        }
        assert_eq!(sim.sent, 0, "seed {seed}");
        assert_eq!(sim.fake.count(Call::Acquire), 0, "seed {seed}");
        assert_eq!(sim.fake.count(Call::Release), 0, "seed {seed}");
        assert_eq!(sim.fake.launched(), 0, "seed {seed}");
    }
    assert!(would_have_launched > 100, "{would_have_launched}");
}

/// Pins a known overrun, which `max_age` (host lifetime) is to close: with demand that never
/// pauses, the machines the cap allowed stay claimed and are never released, so the cloud's
/// bill over a day is far over the day cap plus the release latency that bounds bursty
/// demand. Here two requests wait for two days against a cap of 600 cents, and the last day
/// costs the cloud 2,880 cents (two machines at a cent a minute for 24 hours) against a
/// bound of 760. The scaler is not wrong by its own rules (the cap limits launches, not the
/// life of a machine a request holds); this test says what the cap does not bound. When
/// host lifetime lands this test moves: the bill must then be within the new bound, and the
/// assertion below is turned around.
#[test]
fn demand_that_never_pauses_bills_past_the_day_cap_until_a_host_lifetime_exists() {
    let settings = capped();
    let bound =
        settings.max_spend_per_day + settings.max_instances as u64 * PRICE * LATENCY_MINUTES / 60;
    let mut sim = Sim::new(settings, Billing::Lambda);
    sim.waiting = 2;
    sim.run(2 * 24 * 60);
    let now = sim.clock.now_ms();
    let day = sim.fake.bill_between(now - DAY_MS, now).cents_up();
    assert_eq!(sim.fake.running().len(), 2);
    assert_eq!(day, 2 * 24 * 60, "two machines for a day");
    assert!(day > bound, "{day} against {bound}");
}
