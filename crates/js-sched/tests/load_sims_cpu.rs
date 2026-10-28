//! Simulations of admission by CPU load, over the fluid model of `load_world`. Each
//! bounds the true demand of the model (never the scheduler's own figures) and names the
//! mechanism it exercises.

#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{LoadConfig, Prediction};

use load_world::{Curves, GIB, Spec, W, World};

fn warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
    }
}

fn cold(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: true,
    }
}

/// Sim f. Catches: sums not updated after each placement, so one round places more than
/// the headroom (all sixty on the first worker, or more than the predicate allows on
/// each), and a spread that ignores rank. Three 8-core workers take 8 leases of a core
/// each (8800 of 1000), however the submissions come in, and a burst of twelve is spread
/// four to a worker.
#[test]
fn f_a_burst_in_one_round_is_admitted_up_to_the_headroom_and_spread() {
    for (first, second) in [(60, 0), (30, 30), (12, 0)] {
        let mut w = World::new(LoadConfig::default());
        for name in ["n0", "n1", "n2"] {
            w.linux(name, 8, 64);
        }
        let spec = Spec::flat("burst", 1_000, 400, warm(1_000, 0));
        w.submit_many(first, &spec);
        w.step();
        if second > 0 {
            w.submit_many(second, &spec);
            w.step();
        }
        let per: Vec<usize> = ["n0", "n1", "n2"].map(|n| w.running_on(n)).to_vec();
        if first == 12 {
            assert_eq!(per, [4, 4, 4], "spread by rank");
        } else {
            assert_eq!(per, [8, 8, 8], "up to the headroom and no further: {per:?}");
        }
        for name in ["n0", "n1", "n2"] {
            assert!(w.nodes[name].peak_demand <= 8_800, "{name}");
        }
    }
}

/// Sim g. Catches: no cap on cold leases: 40 unlearned keys at once on an 88-thread
/// node are admitted at most three per window, the cold leases no reading covers yet
/// never exceed `2 * cold_per_window` (a lease is uncovered for at most two windows),
/// and the true demand (4 cores a cold lease) stays within the bound.
#[test]
fn g_a_cold_burst_is_admitted_at_the_cap_and_stays_within_the_bound() {
    let mut w = World::new(LoadConfig::default());
    w.linux("big", 88, 512);
    let spec = Spec {
        mem: vec![(100, GIB)],
        ..Spec::flat("cold", 4_000, 100, cold(2_000, 4 * GIB))
    };
    w.submit_many(40, &spec);
    let mut per_window = vec![0usize; 20];
    let mut before = 0;
    for _ in 0..60 {
        w.step();
        let view =
            w.s.load_view(&js_types::sched::WorkerId::new("big"))
                .unwrap();
        assert!(
            view.uncovered_cpu_millis <= 6 * 2_000,
            "tick {}: {view:?}",
            w.tick
        );
        let started = w.starts.len();
        per_window[((w.tick - 1) / W) as usize] += started - before;
        before = started;
    }
    assert!(per_window.iter().all(|&n| n <= 3), "{per_window:?}");
    // The cap paces the ramp until the measurement (4 cores a lease, 2 predicted) fills
    // the node.
    assert_eq!(per_window[..7], [3; 7], "{per_window:?}");
    assert!(w.stats["cold"].started >= 24, "{per_window:?}");
    // Cold leases demand 4 cores against 2 predicted (`k` = 2, `r` = 1): the bound is
    // `(r k / f) (1 + s) C + n_c A` = 2.67 * 1.1 * 88 + 6 * 4 = 282 cores.
    assert!(
        w.nodes["big"].peak_demand <= 282_000,
        "{}",
        w.nodes["big"].peak_demand
    );
}

/// The ticks until every submitted action of every class has finished, at most `max`.
fn drain(w: &mut World, max: u64) -> u64 {
    for _ in 0..max {
        w.step();
        if w.stats.values().all(|s| s.finished == s.submitted) {
            return w.tick;
        }
    }
    panic!("not drained in {max} ticks: {:?}", w.stats);
}

/// A copy of `config` with the booked rule.
fn off() -> LoadConfig {
    LoadConfig {
        mode: js_sched::PlacementLoad::Off,
        ..LoadConfig::default()
    }
}

/// What the estimator predicts for an action that averages `mean_millis`: the next 250 m
/// step, and never below the estimator's floor.
fn estimated(mean_millis: u64) -> u64 {
    mean_millis
        .div_ceil(250)
        .saturating_mul(250)
        .max(u64::from(js_sizing::CPU_MIN_MILLIS))
}

/// The three populations of sim a on one 88-thread node: wide-pool actions that average
/// 1.5 cores (predicted 2), single-threaded long ones (predicted a core) and light short
/// ones (0.3 core for 8 ticks, under two windows; predicted 500 m, the estimator's step).
fn populations() -> [Spec; 3] {
    [
        Spec::flat("wide", 1_500, 60, warm(2_000, GIB)),
        Spec::flat("single", 1_000, 200, warm(1_000, GIB)),
        Spec::flat("light", 300, 8, warm(estimated(300), GIB / 4)),
    ]
}

fn mixed(config: LoadConfig) -> World {
    let mut w = World::new(config);
    w.linux("n", 88, 512);
    let [wide, single, light] = populations();
    for _ in 0..4 {
        w.submit_many(30, &wide);
        w.submit_many(15, &single);
        w.submit_many(100, &light);
    }
    w
}

/// Sim a. Catches: demand past the bound of the design, `(r k / f) (1 + s_cpu) C`, which for
/// constant-duty actions (`r` 1, `k` at most 1) is 1.47 C = 129 cores of 88; and a model
/// that cannot fail: the same workload under the booked rule puts more demand on the node
/// than the predicate lets in, 132 cores of wide-pool demand.
///
/// The slack alone (`1.1 C`) is not the bound: a measurement is the cores used, which a
/// saturated node caps at its cores, while a covered lease charged the floor `f` of its
/// prediction can demand a third more than that.
#[test]
fn a_demand_stays_within_the_bound_where_the_booked_rule_overloads() {
    let mut on = mixed(LoadConfig::default());
    drain(&mut on, 2_000);
    let peak = on.nodes["n"].peak_demand;
    assert!(peak <= 129_067, "{peak}");

    let mut booked = mixed(off());
    drain(&mut booked, 2_000);
    assert!(
        booked.nodes["n"].peak_demand > peak,
        "{}",
        booked.nodes["n"].peak_demand
    );
    let mut wide = World::new(off());
    wide.linux("n", 88, 512);
    wide.submit_many(120, &populations()[0]);
    wide.run(3);
    assert!(
        wide.nodes["n"].peak_demand >= 130_000,
        "{}",
        wide.nodes["n"].peak_demand
    );
}

/// Sim a (light). Catches: the 250 m step lost (a sub-core prediction rounded up to a
/// core): the light population, 0.3 cores for 8 ticks, runs at 190 at a time on 88 cores
/// (96.8 cores of 500 m), where the booked rule runs 88.
#[test]
fn a_light_short_actions_run_at_more_than_one_a_core() {
    let light = &populations()[2];
    let peak = |config| {
        let mut w = World::new(config);
        w.linux("n", 88, 512);
        w.baseline("n");
        w.submit_many(400, light);
        w.run(3);
        w.nodes["n"].peak_running
    };
    assert_eq!(peak(LoadConfig::default()), 193, "96800 of 500 m");
    assert_eq!(peak(off()), 88);
}

/// Sim e. Catches: a claim that cannot fail, and the throughput given up for the bound:
/// against the booked rule, the mixed workload is not drained more than 5% later, and the
/// light population is drained at least 10% sooner (and waits no longer at the 99th
/// percentile).
#[test]
fn e_throughput_is_not_worse_and_the_light_population_gains() {
    let mut on = mixed(LoadConfig::default());
    let mut booked = mixed(off());
    let (t_on, t_off) = (drain(&mut on, 3_000), drain(&mut booked, 3_000));
    assert!(t_on * 100 <= t_off * 105, "on {t_on} ticks, booked {t_off}");

    let light = |config| {
        let mut w = World::new(config);
        w.linux("n", 88, 512);
        w.submit_many(400, &populations()[2]);
        let ticks = drain(&mut w, 500);
        let mut waits = w.stats["light"].waits.clone();
        waits.sort_unstable();
        (ticks, waits[waits.len() * 99 / 100])
    };
    let ((t_on, p99_on), (t_off, p99_off)) = (light(LoadConfig::default()), light(off()));
    assert!(t_off * 100 >= t_on * 110, "on {t_on}, booked {t_off}");
    assert!(p99_on <= p99_off, "p99 wait on {p99_on}, booked {p99_off}");
}

/// The CPU bound of the design in millicores: `(r k / f) (1 + s_cpu) C + n_c A`, with
/// `r k / f` given in sixteenths-of-thirds as `num / den` (5.33 is 16 / 3).
fn cpu_bound(rk_over_f: (u64, u64), cores: u64, n_c: u64, a_cores: u64) -> u64 {
    let (num, den) = rk_over_f;
    // (num / den) * 1.1 * cores, rounded up, plus n_c * A.
    (num * 1_100 * cores * 1_000).div_ceil(den * 1_000) + n_c * a_cores * 1_000
}

/// A phase change: idle for four windows, then `awake` millicores.
fn phased(awake: u64, hint: Prediction) -> Spec {
    Spec {
        demand: vec![(20, 0), (100, awake)],
        mem: vec![(120, 0)],
        ..Spec::flat("phase", 0, 0, hint)
    }
}

/// A world for sim h's actions, which run under two minutes: with `profiles` every key
/// has the curve of one bucket, at the 4 cores they use when awake, over the 2 they are
/// predicted at; otherwise none has a curve.
fn phase_world(profiles: bool) -> World {
    if !profiles {
        return World::new(LoadConfig::default());
    }
    let curves = Curves::new("sim");
    curves.set_all(&[(2_000, 0)], 5);
    World::with_curves(LoadConfig::default(), &curves)
}

/// Sim h (rise). Catches: a rise in load not believed at once (smoothed with the idle
/// windows before it, or believed only after two): five leases predicted at 2 cores wake
/// to 4 each on a 16-core node, two more are admitted while the readings are idle (the
/// floor bounds that), and once the first reading that shows the rise is in, the
/// measurement (a saturated node) with the two uncovered leases leaves no room, where the
/// floor and the uncovered leases alone would leave room for three.
///
/// Run twice: with keys that have no curve, and with keys that have one bucket at their
/// prediction (2 cores), which must give the same bounds.
#[test]
fn h_a_rise_is_believed_at_once_and_stops_placement() {
    for profiles in [false, true] {
        h_a_rise_is_believed_at_once_and_stops_placement_with(profiles);
    }
}

fn h_a_rise_is_believed_at_once_and_stops_placement_with(profiles: bool) {
    let mut w = phase_world(profiles);
    w.linux("n", 16, 64);
    let phase = phased(4_000, warm(2_000, 0));
    w.submit_many(5, &phase);
    w.run(21);
    let flat = Spec::flat("late", 2_000, 300, warm(2_000, 0));
    w.submit_many(2, &flat);
    w.run(4);
    assert_eq!(
        w.stats["late"].started, 2,
        "admitted against the idle readings"
    );
    // The reading at the end of tick 24 covers a window with all five awake.
    w.submit_many(10, &flat);
    w.run(30);
    assert_eq!(
        w.stats["late"].started, 2,
        "nothing is placed after the rise"
    );
    // Seven leases of up to 4 cores on 16: within `(r k / f) (1 + s) C + n_c A`, with k = 2.
    let bound = cpu_bound((8, 3), 16, 2, 4);
    assert!(
        w.nodes["n"].peak_demand <= bound,
        "{}",
        w.nodes["n"].peak_demand
    );
}

/// Sim h (idle prelude). Catches: the covered-CPU floor missing: batches of leases are
/// admitted window after window while every earlier lease is idle (it measures about 0),
/// the node fills to the predicate, and all leases then wake at four times their
/// prediction (`r` 2, `k` 2). The demand stays within `(r k / f) (1 + s) C + n_c A`
/// (93.9 + 8 cores on 16), and the covered charge never falls below `f` of the covered
/// predictions while they idle.
///
/// Run twice: with keys that have no curve, and with keys that have one bucket at their
/// prediction (2 cores), which must give the same bounds.
#[test]
fn h_the_idle_prelude_cannot_stack_beyond_the_floor() {
    for profiles in [false, true] {
        h_the_idle_prelude_cannot_stack_beyond_the_floor_with(profiles);
    }
}

fn h_the_idle_prelude_cannot_stack_beyond_the_floor_with(profiles: bool) {
    let mut w = phase_world(profiles);
    w.linux("n", 16, 64);
    let node = js_types::sched::WorkerId::new("n");
    let sleeper = Spec {
        demand: vec![(40, 0), (60, 4_000)],
        mem: vec![(100, 0)],
        ..Spec::flat("sleeper", 0, 0, warm(1_000, 0))
    };
    w.submit_many(60, &sleeper);
    let mut floor_held = true;
    for _ in 0..39 {
        w.step();
        let view = w.s.load_view(&node).unwrap();
        if !view.fallback {
            let covered = view.predicted_cpu_millis - view.uncovered_cpu_millis;
            floor_held &= view.left_cpu_millis >= covered * 3 / 4;
        }
    }
    assert!(floor_held, "the covered charge fell below the floor");
    let admitted = w.stats["sleeper"].started;
    w.run(30);
    assert_eq!(
        w.stats["sleeper"].started, admitted,
        "no more after they wake"
    );
    // 0.75 per lease for the covered ones, so 23 fit; all wake at four cores.
    assert!(admitted <= 24, "{admitted}");
    let bound = cpu_bound((16, 3), 16, 2, 4);
    assert_eq!(bound, 101_867);
    assert!(
        w.nodes["n"].peak_demand <= bound,
        "{}",
        w.nodes["n"].peak_demand
    );
}

/// Sim o. Catches: no cold cap, or one that ignores its flag: 1000 unlearned keys queued
/// for an empty 88-thread node are admitted exactly 3 a window (12 with the flag set) while
/// the node has room, never more than `n_c` (6, or 24) uncovered at once, and the demand
/// stays inside the bound. The bound's figures are the design's with its arithmetic
/// fixed: at 12 a window `n_c` = 24 and the bound is 516 + 96 = 612 cores = 6.9 C on 88
/// threads (not 354 = 4.0 C); at the default 3 a window it is 516 + 24 = 540 cores =
/// 6.1 C, and 94 + 8 = 102 cores = 6.4 C on 16.
#[test]
fn o_a_mass_cold_event_ramps_at_the_cap_and_the_bound_is_the_corrected_one() {
    let cold_spec = Spec {
        mem: vec![(2_000, GIB)],
        ..Spec::flat("cold", 2_000, 2_000, cold(2_000, 4 * GIB))
    };
    for per_window in [None, Some(12)] {
        let mut w = World::new(LoadConfig {
            cold_per_window: per_window,
            ..LoadConfig::default()
        });
        w.linux("n", 88, 512);
        w.submit_many(1_000, &cold_spec);
        let cap = u64::from(per_window.unwrap_or(3));
        let n_c = 2 * cap;
        let mut totals = Vec::new();
        for window in 0..24 {
            w.run(W);
            totals.push(w.stats["cold"].started);
            let view = w.s.load_view(&js_types::sched::WorkerId::new("n")).unwrap();
            assert!(
                view.uncovered_cpu_millis <= n_c * 2_000,
                "window {window}: {view:?}"
            );
        }
        // Never more than the cap in a window, and exactly the cap while the node has
        // room for it.
        let steps: Vec<u64> = std::iter::once(0)
            .chain(totals.iter().copied())
            .collect::<Vec<_>>()
            .windows(2)
            .map(|p| p[1] - p[0])
            .collect();
        assert!(steps.iter().all(|&n| n <= cap), "{steps:?}");
        let full = 42 / cap as usize;
        assert!(steps[..full].iter().all(|&n| n == cap), "{steps:?}");
        assert_eq!(totals[full - 1], cap * full as u64);
        // The cold leases demand all they are predicted at (2 cores), so once the node
        // is saturated the measurement stops at its cores and only the floor limits the
        // ramp: some 63 leases, a demand of 1.45 C, inside the bound.
        let running = w.running_of("cold") as u64;
        assert!(
            running > 48,
            "past what the slack alone would hold: {running}"
        );
        let bound = cpu_bound((16, 3), 88, n_c, 4);
        assert!(w.nodes["n"].peak_demand <= bound);
        assert!(w.nodes["n"].peak_demand <= cpu_bound((4, 3), 88, 0, 0));
    }
    assert_eq!(cpu_bound((16, 3), 88, 24, 4), 612_267);
    assert_eq!(612_267 * 10 / 88_000, 69, "6.9 C, not 4.0 C");
    assert_eq!(cpu_bound((16, 3), 88, 6, 4), 540_267);
    assert_eq!(cpu_bound((16, 3), 16, 2, 4), 101_867);
}

fn linux_only(spec: Spec) -> Spec {
    Spec {
        needs: Some(js_caps::Request::from_platform([("OSFamily", "linux")]).unwrap()),
        ..spec
    }
}

fn small() -> Spec {
    linux_only(Spec::flat("small", 1_000, 20, warm(1_000, 0)))
}

fn mac_caps() -> js_caps::NodeCaps {
    js_caps::NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["container"])
}

fn on_mac(spec: Spec) -> Spec {
    Spec {
        needs: Some(js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap()),
        ..spec
    }
}

/// The tick the first lease of `class` on one of `nodes` started, after `from`.
fn first_start(w: &World, class: &str, from: u64) -> Option<u64> {
    w.starts
        .iter()
        .find(|(t, c, _, _, _)| *c == class && *t >= from)
        .map(|(t, ..)| *t)
}

/// Sim b. Catches: a stream of small actions refilling the node a large one waits for (the
/// head would start only when the stream stops, at tick 100): the head of 8 cores (node-wide)
/// behind a running load of one-core leases that end at tick 20, with two more small actions
/// arriving every tick behind it, starts within the longest remaining lease, plus two
/// windows, plus a round (35 ticks of its submission); no small action is placed on the
/// worker it holds meanwhile; a head blocked in the Linux pool does not delay the Mac pool's
/// head; and of two heads in one pool only the first holds a worker, and the second starts
/// no earlier than the first and within the same bound after it.
#[test]
fn b_a_blocked_head_starts_within_the_bound_and_pools_do_not_meet() {
    let mut w = World::new(LoadConfig::default());
    w.linux("n0", 8, 64);
    w.linux("n1", 8, 64);
    w.add_node(
        "m0",
        js_types::sched::Resources::new(8_000, 64 * GIB),
        mac_caps(),
    );
    // Leases that end one after the other, 12 to 26 ticks in: room appears piecemeal, which
    // is what a stream of small work fills up.
    for i in 0..8 {
        let ticks = 12 + 2 * i;
        let lease = Spec::flat("small", 1_000, ticks, warm(1_000, 0));
        w.submit_many(2, &linux_only(lease.clone()));
        w.submit(&on_mac(Spec {
            class: "msmall",
            ..lease
        }));
    }
    w.run(2);
    assert_eq!(w.running_on("n0") + w.running_on("n1"), 16);
    assert_eq!(w.running_on("m0"), 8);
    let head = linux_only(Spec {
        request_cpu: 8_000,
        ..Spec::flat("head", 8_000, 5, warm(8_000, 0))
    });
    let submitted = w.tick;
    let (h1, h2) = (
        w.submit(&head),
        w.submit(&Spec {
            class: "head2",
            ..head.clone()
        }),
    );
    let mac_head = w.submit(&on_mac(Spec {
        class: "mhead",
        ..head.clone()
    }));
    while w.tick < 100 {
        if w.tick == submitted + 5 {
            // One hold per pool: the first head holds a worker, the second none, and the
            // Mac pool's head holds the Mac.
            assert!(w.s.head_hold(h1).is_some());
            assert_eq!(w.s.head_hold(h2), None);
            assert_eq!(w.s.head_hold(mac_head).map(|n| n.as_str()), Some("m0"));
        }
        w.submit_many(2, &small());
        w.submit(&on_mac(Spec {
            class: "msmall",
            ..small()
        }));
        w.step();
    }
    let bound = 24 + 2 * W + 1;
    let head_at = first_start(&w, "head", submitted).expect("the head starts");
    let mac_at = first_start(&w, "mhead", submitted).expect("the Mac head starts");
    let second_at = first_start(&w, "head2", submitted).expect("the second head starts");
    assert!(head_at <= submitted + bound, "head at {head_at}");
    assert!(mac_at <= submitted + bound, "mac head at {mac_at}");
    assert!(second_at >= head_at, "queue order: {second_at} {head_at}");
    assert!(
        second_at <= head_at + 5 + 2 * W + 1,
        "second at {second_at}"
    );
    // No small action on the worker the head held, from its submission to its start.
    let held = w
        .starts
        .iter()
        .find(|(t, c, ..)| *c == "head" && *t == head_at)
        .map(|(_, _, node, ..)| node.clone())
        .unwrap();
    let on_held = w
        .starts
        .iter()
        .filter(|(t, c, node, ..)| *c == "small" && *node == held && *t > submitted && *t < head_at)
        .count();
    assert_eq!(on_held, 0);
    // The rest of the pool went on meanwhile.
    let other = w
        .starts
        .iter()
        .filter(|(t, c, node, ..)| *c == "small" && *node != held && *t > submitted && *t < head_at)
        .count();
    assert!(other > 0, "the other worker kept taking work");
}

/// Sim for the whole-node mark. Catches: a worker being emptied for a whole-machine lease
/// taking small work because its measured load falls as it drains (it would rank first, and
/// refill): two 8-core workers under a stream of one-core actions of 6 ticks; a
/// whole-machine request at tick 10 holds the one with fewer leases, no small action is
/// placed there from its submission to its start, and it starts within the longest lease
/// plus a round.
#[test]
fn a_worker_marked_for_a_whole_machine_lease_is_emptied_not_refilled() {
    let mut w = World::new(LoadConfig::default());
    w.linux("a", 8, 32);
    w.linux("b", 8, 32);
    let short = Spec::flat("short", 1_000, 6, warm(1_000, 0));
    for _ in 0..10 {
        w.submit_many(2, &short);
        w.step();
    }
    let whole = Spec {
        kind: js_types::sched::LeaseKind::WholeMachine,
        request_cpu: 8_000,
        limit: 32 * GIB,
        hint: None,
        ..Spec::flat("whole", 8_000, 10, warm(8_000, 0))
    };
    let submitted = w.tick;
    let op = w.submit(&whole);
    for _ in 0..30 {
        w.submit_many(2, &short);
        w.step();
    }
    let started = first_start(&w, "whole", submitted).expect("the whole-machine lease starts");
    assert!(started <= submitted + 6 + 2, "started at {started}");
    let node = w
        .starts
        .iter()
        .find(|(_, _, _, o, _)| *o == op)
        .map(|(_, _, node, ..)| node.clone())
        .unwrap();
    let refilled = w
        .starts
        .iter()
        .filter(|(t, c, n, ..)| *c == "short" && *n == node && *t >= submitted && *t < started)
        .count();
    assert_eq!(refilled, 0, "the marked worker took small work");
    let after = w
        .starts
        .iter()
        .filter(|(t, c, n, ..)| *c == "short" && *n == node && *t > started)
        .count();
    assert!(after > 0, "small work returns to it afterwards");
}
