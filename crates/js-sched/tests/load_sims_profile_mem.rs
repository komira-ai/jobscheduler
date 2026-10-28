//! Simulations of admission by memory curves, over the fluid model of `load_world`: a
//! lease released at its peak, a lease past its curve, the kill at the lease's own limit
//! and the demotion it leads to, a misfit shared by many keys and the node guard on flat
//! memory. Each bounds the true memory of the model (never the scheduler's own figures)
//! and names the mechanism it exercises.
//!
//! A tick is a second; a profile minute is 60 ticks.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_world/mod.rs"]
mod load_world;

use std::sync::Arc;

use js_sched::{LoadConfig, Prediction};
use js_types::sched::WorkKey;
use js_types::sched::usage::key_hash;

use load_curves::Curves;
use load_world::{GIB, Spec, World};

fn warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
        life_minutes: 1,
    }
}

/// A node of `cores` cores and `gib` GiB that has measured (two readings in).
fn node(config: LoadConfig, cores: u64, gib: u64) -> (World, Arc<Curves>) {
    let curves = Curves::new("sim");
    let mut w = World::with_curves(config, curves.clone());
    w.linux("n", cores, gib);
    w.run(10);
    (w, curves)
}

/// An action of the key `key` that holds `mem` (ticks, bytes) over its run of `total`
/// ticks and one core, with the cgroup limit `limit`, predicted at `flat` bytes.
fn holding(key: &'static str, total: u64, mem: Vec<(u64, u64)>, limit: u64, flat: u64) -> Spec {
    Spec {
        key: Some(key),
        limit,
        mem,
        ..Spec::flat(key, 1_000, total, warm(1_000, flat))
    }
}

fn first_start(w: &World, class: &str) -> Option<u64> {
    w.starts
        .iter()
        .find(|(_, c, ..)| *c == class)
        .map(|(t, ..)| *t)
}

/// Runs until every submitted action has finished, then two more windows.
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

/// The default settings without the node guard on flat memory, which would refuse by the
/// leases' flat costs (two of 8 GiB on 15 GiB; twenty of more than 2 GiB on 40) before the
/// curves are asked: these sims show the curve rule and the limits alone.
fn curves_alone() -> LoadConfig {
    LoadConfig {
        flat_memory_slack_permille: None,
        ..LoadConfig::default()
    }
}

/// Sim r (the peak is behind it). Catches: memory charged as the point of the curve (a
/// second lease admitted while the first one's peak is still ahead of the minute the
/// second one asks for it) and as the flat peak held to the end (a second lease waiting
/// for the first to end): two actions that hold 2 GiB, then 8 GiB for two minutes, then
/// 1 GiB, on a 15 GiB node. The second starts when the first has passed its peak (three
/// minutes after its start, at the reading that says so), not before and not at its end.
#[test]
fn r_a_second_lease_is_admitted_once_the_first_is_past_its_peak() {
    let (mut w, curves) = node(curves_alone(), 8, 15);
    let per_minute: Vec<(u32, u64)> = [2, 8, 8, 1].iter().map(|&g| (1_000, g * GIB)).collect();
    for key in ["m1", "m2"] {
        curves.set(key, &per_minute, 5);
    }
    let shape = vec![(60, 2 * GIB), (120, 8 * GIB), (120, GIB)];
    w.submit(&holding("m1", 300, shape.clone(), 12 * GIB, 8 * GIB));
    w.submit(&holding("m2", 300, shape, 12 * GIB, 8 * GIB));
    w.run(300);
    assert_eq!(first_start(&w, "m1"), Some(10));
    assert_eq!(
        first_start(&w, "m2"),
        Some(190),
        "three minutes after the first"
    );
    assert!(
        w.nodes["n"].peak_memory <= 15 * GIB,
        "{}",
        w.nodes["n"].peak_memory / GIB
    );
}

/// Sim r (past the curve). Catches: a lease whose working set is above its curve charged
/// the curve: `m3` holds 9 GiB where its curve says 4, beside `p` (curve 8 GiB, holds 1);
/// a third action that asks 2 GiB would fit the curves (4 + 8 + 2 on 15) and the node's
/// reading (10 + 2), but not the working set that `m3` is using (9 + 8 + 2): it waits for
/// `m3` to end.
#[test]
fn r_a_lease_above_its_curve_is_charged_its_working_set() {
    let (mut w, curves) = node(LoadConfig::default(), 8, 15);
    curves.set("p", &[(1_000, 8 * GIB)], 5);
    curves.set("m3", &[(1_000, 4 * GIB)], 5);
    curves.set("n", &[(1_000, 2 * GIB)], 5);
    w.submit(&holding("p", 300, vec![(300, GIB)], 12 * GIB, 8 * GIB));
    w.submit(&holding("m3", 300, vec![(300, 9 * GIB)], 12 * GIB, 4 * GIB));
    w.run(60);
    assert_eq!(
        w.s.load_counters().misfit_memory_minutes,
        1,
        "the minute that used 9 against 4"
    );
    w.submit(&holding("n", 100, vec![(100, GIB)], 12 * GIB, 2 * GIB));
    w.run(100);
    assert_eq!(w.running_of("n"), 0, "9 + 8 + 2 > 15");
    drain(&mut w, 600);
    let started = first_start(&w, "n").expect("it runs after m3");
    assert!(started >= 300, "tick {started}");
}

/// The pattern of a key's runs in sim r and u: `None` is a run that holds its curve's
/// 2 GiB, `Some(peak)` one that holds it for a minute and then `peak` bytes.
type Runs = [Option<u64>];

/// Runs `pattern` of the key `k` one run after another on a 64 GiB node, each run's limit
/// being the booking the earlier kills taught (the ladder: 4, 16, 32 GiB, the node).
/// Returns the world, the kills so far after each run, and the runs the shape guard still
/// charges the key's memory flat after each.
fn runs_of(pattern: &Runs) -> (World, Vec<u64>, Vec<u32>) {
    let (mut w, curves) = node(LoadConfig::default(), 8, 64);
    curves.set("k", &[(1_000, 2 * GIB)], 5);
    let k = key_hash(&WorkKey::new("sim", "k"));
    let (mut kills, mut left) = (Vec::new(), Vec::new());
    for run in pattern {
        let mem = match run {
            None => vec![(180, 2 * GIB)],
            Some(peak) => vec![(60, 2 * GIB), (120, *peak)],
        };
        w.submit(&holding("k", 180, mem, 4 * GIB, 2 * GIB));
        drain(&mut w, 2_000);
        kills.push(w.stats["k"].killed);
        left.push(w.s.shape_demoted_runs(k).1);
    }
    (w, kills, left)
}

/// Sim r (the kill and the demotion). Catches: the kill at the lease's own limit not being
/// a misfit, the retry judged as a run of the curve, and the demotion lasting the wrong
/// number of runs. A key peaks at 8 GiB where its curve says 2 and its limit is 4: killed at
/// the limit, run again one rung up (16 GiB), charged flat at the rung; the next run peaks
/// at 20 GiB and is killed at the 16 it was taught; two kills in a row demote the memory
/// curve for 8 runs. The counters say two kills, one demotion, and the retries were flat.
#[test]
fn r_two_kills_at_the_limit_in_a_row_charge_the_key_flat_for_eight_runs() {
    let (w, kills, left) = runs_of(&[
        Some(8 * GIB),
        Some(20 * GIB),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ]);
    assert_eq!(kills[..2], [1, 2]);
    assert!(
        kills.iter().skip(2).all(|&k| k == 2),
        "no kill after the demotion: {kills:?}"
    );
    assert_eq!(left[0], 0);
    assert_eq!(left[1], 8);
    assert_eq!(left[2..], [7, 6, 5, 4, 3, 2, 1, 0, 0, 0]);
    let c = w.s.load_counters();
    assert_eq!((c.demotions_memory, c.demotions_cpu), (1, 0));
    // The profile placements: runs 1 and 2, then the run after the eight flat ones, and
    // all three after it that hold the curve; the retries and the flat runs are not.
    assert_eq!(c.profile_memory, 2 + 2);
}

/// Sim u's per-key patterns. Catches: the second trigger (3 of 10) missing, the first
/// (2 of 3) off, and the documented gap closed by accident. A misfit every third run is
/// killed three times and charged flat from the third; a misfit every fifth run is
/// killed each time and never demoted. (Each kill is a new peak above the rung the last
/// one taught, so a repeat needs new growth.)
#[test]
fn u_a_misfit_every_third_run_is_demoted_and_every_fifth_run_is_not() {
    let growth = [Some(8 * GIB), Some(20 * GIB), Some(40 * GIB)];
    let third = [growth[0], None, None, growth[1], None, None, growth[2]];
    let (_, kills, left) = runs_of(&third);
    assert_eq!(kills.last(), Some(&3));
    assert_eq!(left[..6], [0; 6]);
    assert_eq!(left[6], 8, "the third kill trips it");
    let fifth = [
        growth[0], None, None, None, None, growth[1], None, None, None, None, growth[2],
    ];
    let (_, kills, left) = runs_of(&fifth);
    assert_eq!(kills.last(), Some(&3), "killed every time");
    assert!(left.iter().all(|&l| l == 0), "never demoted: {left:?}");
}

const KEYS: [&str; 20] = [
    "u00", "u01", "u02", "u03", "u04", "u05", "u06", "u07", "u08", "u09", "u10", "u11", "u12",
    "u13", "u14", "u15", "u16", "u17", "u18", "u19",
];

/// The cgroup limit of a lease booked at 4 GiB: 1.5 times that and 512 MiB.
const LIMIT: u64 = 6 * GIB + 512 * (1 << 20);

/// Twenty keys with learned curves of 2 GiB, each booked at 4 GiB, that all hold 2 GiB for a
/// minute and then ask for 8 GiB: past the 6.5 GiB limit of their cgroup.
fn misfitting(w: &mut World, curves: &Curves) {
    for key in KEYS {
        curves.set(key, &[(1_000, 2 * GIB)], 5);
        w.submit(&holding(
            key,
            180,
            vec![(60, 2 * GIB), (120, 8 * GIB)],
            LIMIT,
            4 * GIB,
        ));
    }
}

/// Sim u. Catches: the ladder and the per-lease limit not being what bounds a misfit shared
/// by many keys: twenty keys with learned curves of 2 GiB on a 40 GiB node are all
/// admitted (the curves sum to 40), and all allocate past them in the same minute. The
/// node's true memory exceeds the sum of the curves, by at most the sum of the leases'
/// limits less their charges (20 * (6.5 - 2) = 90 GiB, reached), every lease is killed at
/// its own limit (twenty kills, no more), and each runs again one rung up charged flat.
#[test]
fn u_a_misfit_shared_by_twenty_keys_is_bounded_by_the_limits() {
    let (mut w, curves) = node(curves_alone(), 88, 40);
    misfitting(&mut w, &curves);
    w.step();
    assert_eq!(w.running_of("u00"), 1);
    assert_eq!(KEYS.iter().map(|k| w.running_of(k)).sum::<usize>(), 20);
    let n = js_types::sched::WorkerId::new("n");
    let mut flat_after = 0;
    for _ in 0..3_000 {
        w.step();
        if w.tick > 100 {
            let view = w.s.load_view(&n).expect("registered");
            flat_after = flat_after.max(view.predicted_memory_bytes);
        }
        if w.stats.values().all(|s| s.finished == s.submitted) {
            break;
        }
    }
    let killed: u64 = KEYS.iter().map(|k| w.stats[k].killed).sum();
    let finished: u64 = KEYS.iter().map(|k| w.stats[k].finished).sum();
    assert_eq!((killed, finished), (20, 20));
    let peak = w.nodes["n"].peak_memory;
    assert!(
        peak > 40 * GIB,
        "past the sum of the curves: {} GiB",
        peak / GIB
    );
    assert!(
        peak <= 40 * GIB + 20 * (LIMIT - 2 * GIB),
        "{} GiB",
        peak / GIB
    );
    let c = w.s.load_counters();
    assert_eq!(
        (c.profile_memory, c.demotions_memory),
        (20, 0),
        "the retries were flat"
    );
    assert!(
        flat_after <= 40 * GIB,
        "the retries at their rung: {} GiB",
        flat_after / GIB
    );
}

/// Sim w. Catches: the node guard on flat memory (option A) missing: the same twenty keys
/// with `d` = 0.25 are admitted only while the flat costs (4 GiB each) stay within
/// 1.25 * 40 = 50 GiB, twelve at once, and the node's true memory stays within
/// 1.63 * 1.25 * 40 GiB where without the guard it reached 130. An action at the whole
/// host's memory is admitted all the same on the idle node.
#[test]
fn w_the_flat_memory_guard_refuses_at_the_flat_sum_and_bounds_the_node() {
    let guarded = LoadConfig {
        flat_memory_slack_permille: Some(250),
        ..LoadConfig::default()
    };
    let (mut w, curves) = node(guarded, 88, 40);
    misfitting(&mut w, &curves);
    w.step();
    assert_eq!(
        KEYS.iter().map(|k| w.running_of(k)).sum::<usize>(),
        12,
        "12 * 4 <= 50 < 13 * 4"
    );
    for _ in 0..4_000 {
        w.step();
        if w.stats.values().all(|s| s.finished == s.submitted) {
            break;
        }
    }
    let finished: u64 = KEYS.iter().map(|k| w.stats[k].finished).sum();
    assert_eq!(finished, 20);
    let peak = w.nodes["n"].peak_memory;
    assert!(peak <= 40 * GIB * 163 * 125 / 10_000, "{} GiB", peak / GIB);
    // A whole-host action: the guard does not apply to it.
    curves.set("whole", &[(1_000, GIB)], 5);
    w.submit(&holding("whole", 30, vec![(30, GIB)], 40 * GIB, 40 * GIB));
    w.run(2);
    assert_eq!(w.running_of("whole"), 1);
}

/// A key that peaks at 10 GiB where its curve says 1 and its limit is 4, beside `o`, which
/// books 20 GiB and holds 1 for 600 ticks, on a 32 GiB node; `o` has a curve (1 GiB) when
/// `o_curve` is set. `k` starts, is killed at its limit and its retry (the 16 GiB rung,
/// charged flat) is placed or refused. Returns the world after `k` is killed.
fn retry_beside_a_big_booking(config: LoadConfig, o_curve: bool) -> World {
    let (mut w, curves) = node(config, 8, 32);
    if o_curve {
        curves.set("o", &[(1_000, GIB)], 5);
    }
    curves.set("k", &[(1_000, GIB)], 5);
    w.submit(&holding("o", 600, vec![(600, GIB)], 20 * GIB, 20 * GIB));
    w.run(5);
    let mem = vec![(30, GIB), (120, 10 * GIB)];
    w.submit(&holding("k", 150, mem, 4 * GIB, 2 * GIB));
    // The first run fits (2 + 20 <= 32) and the kill at 4 GiB comes within a minute.
    w.run(120);
    assert_eq!(w.stats["k"].killed, 1, "{:?}", w.stats);
    w
}

/// Sim x (the retry and the flat-memory guard at d = 0). Catches: a retry, charged flat
/// at the 16 GiB rung, that is refused with no end, or that is not refused while the flat
/// charges say it does not fit. `o` books 20 GiB on a 32 GiB node. Without a curve for
/// `o` the flat sum is 36: the retry waits for `o` to end, then runs and finishes. With a
/// curve for `o` (charged 1 GiB) the flat-memory guard must hold the same: memory is never
/// planned over the worker's RAM (see the ignored sim below).
#[test]
fn x_a_retry_waits_for_the_flat_booking_that_blocks_it_and_then_runs() {
    let mut w = retry_beside_a_big_booking(LoadConfig::default(), false);
    assert_eq!(w.running_of("k"), 0, "refused beside o by the flat sum");
    assert_eq!(w.running_of("o"), 1);
    assert_eq!(w.stats["k"].started, 1);
    drain(&mut w, 2_000);
    assert_eq!(w.stats["o"].finished, 1);
    assert_eq!(w.stats["k"].finished, 1);
    assert_eq!(w.stats["k"].started, 2, "one kill, one retry");
    let o_started = w.starts.iter().find(|s| s.1 == "o").map(|s| s.0).unwrap();
    let retry = w.starts.iter().filter(|s| s.1 == "k").nth(1).map(|s| s.0);
    assert!(
        retry.unwrap() >= o_started + 600,
        "the retry started at {retry:?} while o (from {o_started}) still ran"
    );
}

/// Sim x (the retry beside a lease charged by a curve). Catches: a flat-charged retry
/// (16 GiB) admitted beside a 20 GiB lease that a curve charges 1 GiB, on a 32 GiB worker:
/// the flat costs sum to 36, so the retry waits until that lease ends, then runs.
#[test]
fn x_a_retry_waits_for_a_curve_charged_booking_that_blocks_it_by_flat_cost() {
    let mut w = retry_beside_a_big_booking(LoadConfig::default(), true);
    assert_eq!(w.running_of("k"), 0, "refused beside o by the flat sum");
    assert_eq!(w.running_of("o"), 1);
    drain(&mut w, 2_000);
    assert_eq!(w.stats["k"].finished, 1);
    assert_eq!(w.stats["k"].started, 2, "one kill, one retry");
    let o_started = w.starts.iter().find(|s| s.1 == "o").map(|s| s.0).unwrap();
    let retry = w.starts.iter().filter(|s| s.1 == "k").nth(1).map(|s| s.0);
    assert!(
        retry.unwrap() >= o_started + 600,
        "the retry started at {retry:?} while o (from {o_started}) still ran"
    );
}
