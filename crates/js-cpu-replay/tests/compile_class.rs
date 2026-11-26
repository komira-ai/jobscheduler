//! A farm-shaped replay of the class `docs/design/placement-profiles-live.md` starts from: a
//! compile that runs 120 s at a mean of 4.25 cores and bursts to 55 cores for about 9 s,
//! arriving every 4 s beside a steady class, on two hosts of 88 cores. It replays the flat step
//! (every class booked at its median) and the curve mode (a class that passes its checks booked
//! by its curve, packed slot by slot), and prints booked against used, the CPU asked past the
//! hosts' cores, the slowdown and the wait of each.

use js_cpu_replay::report::ARMS;
use js_cpu_replay::shape::Shape;
use js_cpu_replay::sim::{Farm, Outcome, Setup, replay};
use js_cpu_replay::trace::{End, Job};

/// Two hosts of 88 cores.
const FARM: Farm = Farm {
    hosts: 2,
    ..Farm::OURS
};

/// The trace: 800 compiles of class 0, one every 4 s, and 120 steady jobs of class 1 (300 s at
/// 3 cores, no burst), one every 30 s.
fn trace() -> Vec<Job> {
    let job = |arrive_ms: u64, (wall_ms, cpu_ms, peak): (u64, u64, Option<u32>), class: u32| Job {
        arrive_ms,
        wait_ms: 0,
        start_ms: 0,
        wall_ms,
        cpu_ms,
        peak_milli: peak,
        pressure_ms: None,
        memory_mib: 1024,
        booked_milli: 4_250,
        booked_mib: 2048,
        host: 0,
        end: End::Ok,
        class,
        fallback: None,
        client: 9,
    };
    let compiles = (0..800).map(|k| job(k * 4_000, (120_000, 510_000, Some(55_000)), 0));
    let steady = (0..120).map(|n| job(n * 30_000 + 7_000, (300_000, 900_000, None), 1));
    let mut jobs: Vec<Job> = compiles.chain(steady).collect();
    jobs.sort_by_key(|job| job.arrive_ms);
    jobs
}

/// The arm named `name` of the report.
fn arm(name: &str) -> js_cpu_replay::sim::Arm {
    *ARMS.iter().find(|arm| arm.name == name).unwrap()
}

/// What the jobs booked over what they used, in hundredths.
fn booked_per_used(outcome: &Outcome) -> u128 {
    outcome.booked * 100 / outcome.used as u128
}

/// What a replay of the trace under `shape` came to for the arm `name`: booked over used (in
/// hundredths), the CPU asked past the hosts' cores (core-seconds), the jobs' slowdown (s), the
/// 90th percentile of the wait (s) and the jobs placed.
fn figures(shape: Shape, name: &str) -> (u128, u128, u64, u64, usize) {
    let setup = Setup {
        farm: FARM,
        shape,
        speed: 1,
    };
    let outcome = replay(&trace(), arm(name), setup);
    let mut waits = outcome.waits.clone();
    waits.sort_unstable();
    let p90 = waits[waits.len() * 9 / 10];
    let figures = (
        booked_per_used(&outcome),
        outcome.over_excess / 1_000_000,
        outcome.slowed_ms / 1000,
        p90 / 1000,
        outcome.waits.len(),
    );
    println!(
        "{} {name}: booked/used x100, core-s past cores, slowdown s, wait p90 s, placed = {figures:?}",
        shape.name()
    );
    figures
}

/// Asserts the curve mode against the flat step on the compile class, the replay's figures
/// exact. With the burst at the same point of every compile (the class is shaped), the flat step
/// books 4.25 cores at every second and lets the bursts meet; the curve mode books the burst
/// where it comes (22 cores, a quarter of the host) and keeps bursts apart, so it asks the hosts
/// past their cores for about half as much (539 663 core-s against 966 138) and halves the
/// slowdown, while the jobs wait longer for a host (a p90 of 596 s against 152 s). It books
/// less than the compile uses (0.90 of it), since the burst above a quarter of the host is
/// unbooked. With the burst at another point of each run (the class is not shaped and books
/// flat), the two are close. Every job is placed in each. Catches a curve arm that books the
/// flat number on a shaped class (equal figures), one that books by curve a class whose bursts
/// move, and a packing that lets the bursts of a shaped class meet.
#[test]
fn the_curve_mode_keeps_a_shaped_compiles_bursts_apart_at_the_cost_of_wait() {
    let (flat, curve) = ("A median, 1.1", "A median, curves packed, 1.1");
    assert_eq!(
        figures(Shape::ClassBurst, flat),
        (123, 966_138, 43_384, 152, 920)
    );
    assert_eq!(
        figures(Shape::ClassBurst, curve),
        (90, 539_663, 20_214, 596, 920)
    );
    assert_eq!(figures(Shape::Burst, flat), (119, 589_666, 27_279, 44, 920));
    assert_eq!(
        figures(Shape::Burst, curve),
        (118, 609_396, 29_243, 47, 920)
    );
}
