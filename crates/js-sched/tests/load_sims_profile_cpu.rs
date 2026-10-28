//! Simulations of admission by CPU profiles, over the fluid model of `load_world`: what
//! the per-minute rule admits that the flat one refuses, what the dilation and the horizon
//! check keep out, what a wrong profile costs and how the shape guard ends it, and what the
//! 30-minute charge does to short actions. Each bounds the true demand of the model (never
//! the scheduler's own figures) and names the mechanism it exercises.
//!
//! A tick is a second; a profile minute is 60 ticks. Every curve is the dilated fit a
//! `CurveSource` would give (two minutes either side).

#[path = "load_world/mod.rs"]
mod load_world;

use std::sync::Arc;

use js_sched::{LoadConfig, Prediction};
use js_types::sched::WorkKey;
use js_types::sched::usage::key_hash;

use load_world::curves::dilated;
use load_world::{Curves, GIB, Spec, World};

fn warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
    }
}

/// An 88-thread node that has measured (two readings in), and the curves it is given.
fn node(config: LoadConfig) -> (World, Arc<Curves>) {
    let curves = Curves::new("sim");
    let mut w = World::with_curves(config, &curves);
    w.linux("n", 88, 512);
    w.run(10);
    (w, curves)
}

/// Millicores per minute of a run of `len` minutes that asks `busy_cores` during the
/// minutes of `busy` and `quiet` otherwise.
fn shape(len: usize, busy: std::ops::Range<usize>, busy_cores: u32, quiet: u32) -> Vec<u32> {
    (0..len)
        .map(|m| if busy.contains(&m) { busy_cores } else { quiet })
        .collect()
}

/// An action of the key `key` that really asks `raw` millicores in each minute, predicted
/// at `p_bar` (its flat cost).
fn acting(class: &'static str, key: &'static str, raw: &[u32], p_bar: u64) -> Spec {
    Spec {
        key: Some(key),
        demand: raw.iter().map(|&c| (60, u64::from(c))).collect(),
        mem: vec![(raw.len() as u64 * 60, GIB)],
        ..Spec::flat(class, 0, 0, warm(p_bar, GIB))
    }
}

/// Gives `key` the dilated curve of `raw`.
fn learn(curves: &Curves, key: &str, raw: &[u32]) {
    let points: Vec<(u32, u64)> = raw.iter().map(|&c| (c, GIB)).collect();
    curves.set(key, &dilated(&points, 2), 5);
}

fn first_start(w: &World, class: &str) -> Option<u64> {
    w.starts
        .iter()
        .find(|(_, c, ..)| *c == class)
        .map(|(t, ..)| *t)
}

/// Runs until every submitted action has finished, then two more windows, so that the
/// readings no longer show the load that has just ended.
fn drain(w: &mut World, max: u64) {
    for _ in 0..max {
        w.step();
        if w.stats.values().all(|s| s.finished == s.submitted) {
            w.run(3 * load_world::W);
            return;
        }
    }
    panic!("not drained in {max} ticks: {:?}", w.stats);
}

const BUSY: u32 = 58_000;
const QUIET: u32 = 4_000;
/// The flat cost of both actions of sim p: two of them are 98 cores on 96.8.
const P_BAR: u64 = 49_000;

/// Sim p (the profile rule). Catches: the per-minute rule missing, so that two actions
/// whose busy minutes do not overlap are not co-admitted: A is busy (58 cores) in minutes
/// 0 to 2, B in minutes 7 to 9, a gap of 4 minutes (twice the dilation), each learned and
/// predicted at 49 cores. B is admitted beside A at once and the node's true demand stays
/// within the 110% line at every tick. Under the flat rule (the operator's off switch),
/// 49 + 49 is over the line and B waits until A's busy phase is behind it.
#[test]
fn p_non_overlapping_busy_phases_are_admitted_together_under_profiles() {
    let a = shape(12, 0..3, BUSY, QUIET);
    let b = shape(12, 7..10, BUSY, QUIET);
    let (mut w, curves) = node(LoadConfig::default());
    learn(&curves, "pa", &a);
    learn(&curves, "pb", &b);
    w.submit(&acting("pa", "pa", &a, P_BAR));
    w.submit(&acting("pb", "pb", &b, P_BAR));
    w.step();
    assert_eq!(w.running_of("pa") + w.running_of("pb"), 2, "both at once");
    drain(&mut w, 900);
    assert!(
        w.nodes["n"].peak_demand <= 96_800,
        "true demand {}",
        w.nodes["n"].peak_demand
    );
    let c = w.s.load_counters();
    assert_eq!((c.profile_cpu, c.demotions_cpu), (2, 0));

    let flat = LoadConfig {
        profile: false,
        ..LoadConfig::default()
    };
    let (mut w, _) = node(flat);
    w.submit(&acting("pa", "pa", &a, P_BAR));
    w.submit(&acting("pb", "pb", &b, P_BAR));
    w.run(100);
    assert_eq!(w.running_of("pb"), 0, "49 + 49 > 96.8 while A is busy");
    drain(&mut w, 1_200);
    let started = first_start(&w, "pb").expect("B runs");
    assert!(started >= 190, "after A's busy phase: tick {started}");
}

/// Sim p (the dilation). Catches: a busy phase that starts a little early not covered by
/// the dilation: B busy in minutes 6 to 8 (a gap of 3, under twice the dilation) is not
/// admitted beside A at once, but waits until A has aged a minute and its busy phase no
/// longer reaches B's.
#[test]
fn p_a_gap_under_twice_the_dilation_is_refused_at_first() {
    let a = shape(12, 0..3, BUSY, QUIET);
    let b = shape(12, 6..9, BUSY, QUIET);
    let (mut w, curves) = node(LoadConfig::default());
    learn(&curves, "pa", &a);
    learn(&curves, "pb", &b);
    w.submit(&acting("pa", "pa", &a, P_BAR));
    w.submit(&acting("pb", "pb", &b, P_BAR));
    w.run(30);
    assert_eq!(w.running_of("pa"), 1);
    assert_eq!(
        w.running_of("pb"),
        0,
        "dilated busy phases of 0..4 and 4..10 meet"
    );
    w.run(60);
    let started = first_start(&w, "pb").expect("B is admitted once A has aged");
    assert!((60..90).contains(&(started - 10)), "tick {started}");
    assert!(w.nodes["n"].peak_demand <= 96_800);
}

/// Sim p (the horizon check). Catches: the check beyond the horizon missing: two actions
/// busy at minute 40, which no minute of the next 30 shows, are not admitted together; one
/// busy at minute 20 is admitted beside the first.
#[test]
fn p_two_busy_phases_at_minute_forty_are_refused_by_the_h_plus_check() {
    let long = shape(45, 40..43, BUSY, QUIET);
    let early = shape(45, 20..23, BUSY, QUIET);
    for (second, raw, together) in [("pb", &long, false), ("pc", &early, true)] {
        let (mut w, curves) = node(LoadConfig::default());
        learn(&curves, "pa", &long);
        learn(&curves, second, raw);
        w.submit(&acting("pa", "pa", &long, P_BAR));
        w.submit(&acting("second", second, raw, P_BAR));
        w.run(40);
        assert_eq!(w.running_of("pa"), 1);
        assert_eq!(w.running_of("second"), usize::from(together), "{second}");
    }
}

/// Sim q's actions: both keep 79 cores busy for 3 minutes, are predicted at 21 cores and
/// have the same curve shape as sim p's, six minutes apart.
const WRONG_BUSY: u32 = 79_000;
const WRONG_P_BAR: u64 = 21_000;

fn pair_of(w: &mut World, a: &[u32], b: &[u32]) {
    w.submit(&acting("qa", "qa", a, WRONG_P_BAR));
    w.submit(&acting("qb", "qb", b, WRONG_P_BAR));
}

/// Sim q (a wrong profile). Catches: a profile that is wrong going unanswered, and the
/// floor and the shape guard missing. B's curve says busy in minutes 7 to 9 but it is
/// busy in minutes 0 to 2 (six minutes off, beyond the dilation): both are admitted, both
/// run busy together, the node's true demand (158 cores on 88) stays within the
/// wrong-profile limit `(r_b k / g) (1 + s) C` with `r_b` 1 and `k` the busy cores over
/// the prediction. Each run's minutes at 44 cores, over twice the 15.75-core charge,
/// misfit; after two such runs the key is charged flat for 8 runs and an identical pair is
/// no longer admitted together.
#[test]
fn q_a_wrong_profile_is_bounded_and_the_key_is_demoted_after_two_runs() {
    let a = shape(12, 0..3, WRONG_BUSY, QUIET);
    let said = shape(12, 7..10, WRONG_BUSY, QUIET);
    let (mut w, curves) = node(LoadConfig::default());
    learn(&curves, "qa", &a);
    learn(&curves, "qb", &said);
    let qb = key_hash(&WorkKey::new("sim", "qb"));
    for run in 0..2 {
        pair_of(&mut w, &a, &a);
        w.step();

        assert_eq!(
            w.running_of("qa") + w.running_of("qb"),
            2,
            "run {run}: admitted together"
        );
        drain(&mut w, 1_500);
        assert_eq!(
            w.s.shape_demoted_runs(qb),
            if run == 0 { (0, 0) } else { (8, 0) }
        );
    }
    // k = 79 / 21 = 3.76: (3.76 / 0.75) * 96.8 = 485 cores.
    let limit = 96_800 * u64::from(WRONG_BUSY) / (WRONG_P_BAR * 750 / 1000);
    assert!(
        w.nodes["n"].peak_demand > 96_800 && w.nodes["n"].peak_demand <= limit,
        "{} against {limit}",
        w.nodes["n"].peak_demand
    );
    let c = w.s.load_counters();
    assert_eq!(c.demotions_cpu, 1);
    assert_eq!(
        w.s.shape_demoted_runs(key_hash(&WorkKey::new("sim", "qa"))),
        (0, 0)
    );
    // The same pair again: B is charged its flat 21 cores beside A's 79 and is refused.
    pair_of(&mut w, &a, &a);
    w.run(10);
    assert_eq!((w.running_of("qa"), w.running_of("qb")), (1, 0));
}

/// Sim q (a shift the dilation absorbs). Catches: a dilation that does not cover a busy
/// phase one minute early: B busy in minutes 6 to 8 where its curve says 7 to 9 never
/// exceeds the line, is not judged a misfit however often it runs, and is co-admitted
/// with A every time.
#[test]
fn q_a_shift_of_one_minute_is_absorbed_by_the_dilation() {
    let a = shape(12, 0..3, WRONG_BUSY, QUIET);
    let said = shape(12, 7..10, WRONG_BUSY, QUIET);
    let early = shape(12, 6..9, WRONG_BUSY, QUIET);
    let (mut w, curves) = node(LoadConfig::default());
    learn(&curves, "qa", &a);
    learn(&curves, "qb", &said);
    for _ in 0..3 {
        pair_of(&mut w, &a, &early);
        w.step();
        assert_eq!(w.running_of("qa") + w.running_of("qb"), 2);
        drain(&mut w, 1_000);
    }
    assert!(
        w.nodes["n"].peak_demand <= 96_800,
        "{}",
        w.nodes["n"].peak_demand
    );
    let c = w.s.load_counters();
    assert_eq!((c.demotions_cpu, c.misfit_cpu_minutes), (0, 0));
}

/// Sim v. Catches: a lease longer than the horizon charged only for the next 30 minutes:
/// an action of 90 minutes busy at minutes 40 to 42 is charged its remaining maximum from
/// the horizon on (58 cores at the `H+` slot while the busy phase, dilated to minute 44, is
/// ahead of minute 30 after now, the floor once it is behind), and a second one of the
/// same shape is refused until the first is 15 minutes old.
#[test]
fn v_a_lease_longer_than_the_horizon_is_charged_its_remaining_maximum() {
    let run = shape(90, 40..43, BUSY, QUIET);
    // A curve has 64 points; the last stands for every later minute.
    let long = &run[..64];
    let (mut w, curves) = node(LoadConfig::default());
    learn(&curves, "pa", long);
    learn(&curves, "pb", long);
    w.submit(&acting("pa", "pa", &run, P_BAR));
    w.submit(&acting("pb", "pb", &run, P_BAR));
    w.step();
    let n = js_types::sched::WorkerId::new("n");
    let ahead = |w: &World, m: u32| w.s.charged_ahead(&n, m).expect("registered").0;
    assert_eq!(w.running_of("pb"), 0, "the pair is refused");
    assert_eq!(ahead(&w, 10), 36_750, "nothing busy inside the horizon");
    assert_eq!(ahead(&w, 31), 58_000, "beyond it the busy phase");
    // The busy phase reaches minute 44: until A is 15 minutes old, the H+ slot holds it.
    while ahead(&w, 31) == 58_000 {
        assert_eq!(w.running_of("pb"), 0, "tick {}", w.tick);
        w.step();
    }
    assert_eq!(
        w.tick,
        10 + 15 * 60,
        "the reading that gives A its fifteenth minute"
    );
    assert_eq!(ahead(&w, 25), 58_000, "minute 40 is inside the horizon now");
    assert_eq!(ahead(&w, 31), 36_750, "and from minute 45 on only the tail");
    w.step();
    assert_eq!(w.running_of("pb"), 1, "B is admitted at once");
}

/// What a stream of short actions makes of a node that holds one long action with a late
/// busy phase: the actions that started, the 95th percentile of their waits in ticks, and
/// the requests the load kept out.
struct Stream {
    started: u64,
    p95: u64,
    blocked: u64,
}

/// Sim af's world: a long action `lg` (25 minutes, 94 cores busy in minutes 12 to 16,
/// predicted at 28 cores) placed first, then two 30-second one-core actions a second for
/// 300 seconds, none with a curve of its own and every one with a P90 life of a minute;
/// 150 seconds more to finish.
fn stream(config: LoadConfig) -> Stream {
    let raw = shape(25, 12..17, 94_000, QUIET);
    let (mut w, curves) = node(config);
    learn(&curves, "lg", &raw);
    curves.set_life("lg", 25);
    curves.set_life_all(1);
    w.submit(&acting("lg", "lg", &raw, 28_000));
    w.step();
    let short = Spec::flat("short", 1_000, 30, warm(1_000, GIB / 4));
    for _ in 0..300 {
        w.submit_many(2, &short);
        w.step();
    }
    w.run(150);
    let mut waits = w.stats["short"].waits.clone();
    waits.sort_unstable();
    Stream {
        started: w.stats["short"].started,
        p95: waits[waits.len() * 95 / 100],
        blocked: w.s.load_counters().blocked,
    }
}

/// Sim af. Catches: the cost of charging a new lease for the whole horizon (the known gap),
/// and the setting that ends the charge at the expected life not removing it. The long
/// action asks 94 cores at minutes 10 to 18 (dilated); a short action charged a core for
/// 30 minutes is refused by that busy phase however soon it ends, so the shorts queue (45 of
/// 600 started, the rest refused by the load); charged for their life (1 minute at the P90
/// plus 2) all 600 run as under the flat rule, with no refusal and no more queue wait than
/// it, which is none.
#[test]
fn af_short_actions_are_refused_by_the_tail_charge_unless_it_ends_at_their_life() {
    let flat = stream(LoadConfig {
        profile: false,
        ..LoadConfig::default()
    });
    assert_eq!((flat.started, flat.p95, flat.blocked), (600, 0, 0));
    let tail = stream(LoadConfig::default());
    assert!(tail.blocked > 0, "the refusals the tail charge causes");
    assert!(tail.started * 4 < flat.started, "{} started", tail.started);
    assert!(tail.p95 * 10 > flat.p95 * 11, "p95 wait {}", tail.p95);
    let life = stream(LoadConfig {
        expected_life_delta_minutes: Some(2),
        ..LoadConfig::default()
    });
    assert_eq!(life.started, flat.started);
    assert_eq!(life.blocked, 0);
    assert!(life.p95 * 10 <= flat.p95 * 11, "p95 wait {}", life.p95);
}
