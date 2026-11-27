//! Sim ae: head liveness under best fit. Three 88-thread hosts under pure best fit (the
//! pack limit at 1.0); a stream of 4-core, 5-minute leases that keeps every host full; and
//! one host, `c`, that holds a long lease with a late peak: 30 minutes, quiet (4 cores)
//! except for 60 cores in minutes 20 to 24, learned as a curve. As the peak comes into the
//! horizon `c` admits only what fits beside it, so best fit leaves it the lightest host
//! now, while `a` and `b` stay packed. A 44-core head request then holds the host with the
//! smallest binding ratio, `c`, which is the one with the late peak: the head cannot run
//! beside the peak and waits it out. It starts within the bound of the design (the longest
//! remaining lease on the held host at reservation, plus `2W`, plus one round), no stream
//! lease is placed on `c` meanwhile, and `a` and `b` keep taking the stream.
//! Exercises: the reservation under best fit.
//!
//! Logical ticks only (a tick is a second, a profile minute 60 ticks); no assertion reads a
//! clock.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{LoadConfig, Prediction};
use js_types::sched::WorkerId;

use load_curves::{Curves, dilated};
use load_world::{GIB, Spec, W, World};

const CORES: u64 = 88;
/// `C`, in millicores.
const C: u64 = CORES * 1_000;
const MINUTE: u64 = 60;
/// The long lease: its length in minutes, its busy minutes and their cores, and its quiet
/// cores.
const LONG_MINUTES: usize = 30;
const PEAK: std::ops::Range<usize> = 20..25;
const PEAK_MILLIS: u32 = 60_000;
const QUIET_MILLIS: u32 = 4_000;
/// The stream's leases: predicted and booked at 4 cores, really 3 (so no host's demand
/// passes its cores and every lease lasts its five minutes of ticks).
const STREAM_PREDICTED: u64 = 4_000;
const STREAM_REAL: u64 = 3_000;
const STREAM_TICKS: u64 = 5 * MINUTE;
/// The head: 44 cores for five minutes.
const HEAD_MILLIS: u64 = 44_000;
/// The long lease's minute at which the head is submitted: its peak is in the horizon.
const HEAD_AT_MINUTE: u64 = 16;

fn warm(cpu_millis: u64, life_minutes: u8) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes: GIB,
        cold: false,
        life_minutes,
    }
}

fn long_raw() -> Vec<u32> {
    (0..LONG_MINUTES)
        .map(|m| {
            if PEAK.contains(&m) {
                PEAK_MILLIS
            } else {
                QUIET_MILLIS
            }
        })
        .collect()
}

/// The long lease: it asks its raw curve and is predicted at its mean (13 cores).
fn long() -> Spec {
    let raw = long_raw();
    let mean = raw.iter().map(|&c| u64::from(c)).sum::<u64>() / raw.len() as u64;
    Spec {
        key: Some("lg"),
        request_cpu: mean,
        demand: raw.iter().map(|&c| (MINUTE, u64::from(c))).collect(),
        mem: vec![(LONG_MINUTES as u64 * MINUTE, GIB)],
        ..Spec::flat("long", 0, 0, warm(mean, 30))
    }
}

fn stream() -> Spec {
    Spec {
        request_cpu: STREAM_PREDICTED,
        mem: vec![(STREAM_TICKS, GIB)],
        ..Spec::flat(
            "stream",
            STREAM_REAL,
            STREAM_TICKS,
            warm(STREAM_PREDICTED, 5),
        )
    }
}

fn head() -> Spec {
    Spec {
        request_cpu: HEAD_MILLIS,
        mem: vec![(STREAM_TICKS, GIB)],
        ..Spec::flat("head", HEAD_MILLIS, STREAM_TICKS, warm(HEAD_MILLIS, 5))
    }
}

/// The length in ticks of a lease of `class`.
fn ticks_of(class: &str) -> u64 {
    match class {
        "long" => LONG_MINUTES as u64 * MINUTE,
        "stream" | "head" => STREAM_TICKS,
        other => panic!("no lease class {other}"),
    }
}

/// Submits one stream lease when no more than `queued` requests wait, so that the stream
/// fills every slot that opens without a backlog of its own.
fn feed(w: &mut World, queued: usize) {
    if w.s.queued_len() <= queued {
        w.submit(&stream());
    }
}

/// Starts of `class` on `node` in the ticks `(from, to)`.
fn starts_between(w: &World, class: &str, node: &str, from: u64, to: u64) -> usize {
    w.starts
        .iter()
        .filter(|(t, c, n, ..)| *c == class && n == node && *t > from && *t < to)
        .count()
}

/// Sim ae. Catches: a blocked head under best fit whose held host is refilled by the stream
/// (it would wait while the stream runs), and a hold that does not reach the host with the
/// late peak; the bound is the design's, from the leases on the held host at reservation.
#[test]
fn ae_a_44_core_head_on_the_late_peak_host_starts_within_the_bound() {
    let config = LoadConfig {
        pack_limit_permille: 1_000,
        ..LoadConfig::default()
    };
    let curves = Curves::new("sim");
    let points: Vec<(u32, u64)> = long_raw().iter().map(|&c| (c, GIB)).collect();
    curves.set("lg", &dilated(&points, 2), 5);
    curves.set_life("lg", 30);
    curves.set_life_all(5);
    let mut w = World::with_curves(config, curves.clone());
    // `c` measures, then takes the long lease alone; `a` and `b` join after.
    w.linux("c", CORES, 256);
    w.run(10);
    w.submit(&long());
    w.step();
    let long_start = w.tick - 1;
    assert_eq!(w.running_on("c"), 1, "the long lease is on c");
    w.linux("a", CORES, 256);
    w.linux("b", CORES, 256);
    w.run(10);

    // The stream fills the pool and keeps it full until the long lease's minute 16.
    let submit_at = long_start + HEAD_AT_MINUTE * MINUTE;
    while w.tick < submit_at || w.s.queued_len() > 0 {
        feed(&mut w, 0);
        w.step();
        assert!(w.demand_on("c") <= C, "c is never slowed");
    }
    let op = w.submit(&head());
    let submitted = w.tick;

    // The head waits; it holds a host once it is the pool's blocked head.
    let mut reserved = None;
    let mut head_at = None;
    while head_at.is_none() && w.tick < submitted + 3_000 {
        feed(&mut w, 1);
        w.step();
        assert!(w.demand_on("c") <= C, "c is never slowed");
        if reserved.is_none()
            && let Some(name) = w.s.head_hold(op)
        {
            reserved = Some((w.tick - 1, name.as_str().to_owned()));
            // The premise: the stream packed `a` and `b`, and best fit left `c` the
            // lightest now, so it is the host of the smallest binding ratio.
            let view = |n: &str| w.s.load_view(&WorkerId::new(n)).expect("a host");
            let (a, b, c) = (view("a"), view("b"), view("c"));
            assert!(a.predicted_cpu_millis + HEAD_MILLIS > C * 11 / 10);
            assert!(b.predicted_cpu_millis + HEAD_MILLIS > C * 11 / 10);
            assert!(c.left_cpu_millis < a.left_cpu_millis.min(b.left_cpu_millis));
        }
        head_at = w
            .starts
            .iter()
            .find(|(_, _, _, o, _)| *o == op)
            .map(|(t, _, node, ..)| (*t, node.clone()));
    }
    let (held_at, held) = reserved.expect("the head held a host");
    let (head_at, head_on) = head_at.expect("the head starts");
    assert_eq!(held, "c", "the hold reaches the host with the late peak");
    assert_eq!(head_on, held, "the head starts on the host it held");

    // The design's bound: the longest remaining lease on the held host at reservation,
    // plus two windows, plus one round.
    let longest = w
        .starts
        .iter()
        .filter(|(t, _, node, ..)| *node == held && *t <= held_at)
        .map(|(t, class, ..)| t + ticks_of(class))
        .filter(|end| *end > held_at)
        .map(|end| end - held_at)
        .max()
        .expect("a lease on the held host");
    let bound = held_at + longest + 2 * W + 1;
    println!(
        "sim ae: submitted at {submitted}, held c at {held_at}, started at {head_at}, \
         bound {bound} (longest remaining {longest} ticks)"
    );
    assert!(head_at <= bound, "head at {head_at}, bound {bound}");
    // It could not run beside the peak: it started once the long lease was into it.
    assert!(
        head_at >= long_start + PEAK.start as u64 * MINUTE,
        "head at {head_at}"
    );
    // Nothing refilled the held host; the others went on packing the stream.
    assert_eq!(starts_between(&w, "stream", "c", held_at, head_at), 0);
    assert!(starts_between(&w, "stream", "a", held_at, head_at) > 0);
    assert!(starts_between(&w, "stream", "b", held_at, head_at) > 0);
}
