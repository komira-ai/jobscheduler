//! A farm-shaped replay of the class `docs/design/placement-profiles-live.md` starts from: a
//! compile that runs 120 s at a mean of 4.25 cores and bursts to 55 cores for about 9 s,
//! arriving every 4 s beside a steady class, on two hosts of 88 cores, each job with the CPU
//! series its host recorded. It replays the flat step (every class booked at its median) and
//! the curve mode (a class that passes its checks booked by its curve, packed slot by slot),
//! every job living its recorded series, and prints booked against used, the CPU asked past the
//! hosts' cores, the slowdown and the wait of each.

use js_cpu_replay::report::{CURVE_MODE, FLAT_STEP};
use js_cpu_replay::shape::Shape;
use js_cpu_replay::sim::{Arm, Farm, Outcome, Setup, replay};
use js_cpu_replay::trace::{End, Job};
use js_types::CpuSeries;

/// Two hosts of 88 cores.
const FARM: Farm = Farm {
    hosts: 2,
    ..Farm::OURS
};

/// A compile's series: 60 points of 2 s, nothing but its burst, 8 s at 55 cores and 2 s at 35
/// (510 s of CPU), from the point `at`.
fn compile_series(at: usize) -> CpuSeries {
    let mut points = vec![0u16; 60];
    points[at..at + 4].fill(880);
    points[at + 4] = 560;
    CpuSeries::new(2, points, 120_000_000, false).unwrap()
}

/// The trace: 800 compiles of class 0, one every 4 s, and 120 steady jobs of class 1 (300 s at
/// 3 cores, no burst), one every 30 s. The compiles burst at point `at(k)` of their series, the
/// `k`th compile.
fn trace(at: fn(usize) -> usize) -> Vec<Job> {
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
        cpu_series: None,
    };
    let compiles = (0..800).map(|k| Job {
        cpu_series: Some(compile_series(at(k))),
        ..job(k as u64 * 4_000, (120_000, 510_000, Some(55_000)), 0)
    });
    let steady = (0..120).map(|n| Job {
        cpu_series: CpuSeries::new(4, vec![48; 75], 300_000_000, false),
        ..job(n * 30_000 + 7_000, (300_000, 900_000, None), 1)
    });
    let mut jobs: Vec<Job> = compiles.chain(steady).collect();
    jobs.sort_by_key(|job| job.arrive_ms);
    jobs
}

/// Every compile bursts 40 s into its run: the class is shaped.
fn same(_: usize) -> usize {
    20
}

/// Each compile bursts at another point of its run: the class has no shape.
fn moving(k: usize) -> usize {
    k * 7 % 55
}

/// What the jobs booked over what they used, in hundredths.
fn booked_per_used(outcome: &Outcome) -> u128 {
    outcome.booked * 100 / outcome.used as u128
}

/// The replay of the trace of `at` by `arm`, every job living its recorded series.
fn replayed(at: fn(usize) -> usize, arm: Arm) -> Outcome {
    let setup = Setup {
        farm: FARM,
        shape: Shape::Recorded,
        speed: 1,
        held_out_ms: None,
    };
    let outcome = replay(&trace(at), arm, setup);
    let mut waits = outcome.waits.clone();
    waits.sort_unstable();
    println!(
        "{}: booked/used x100 {}, core-s past cores {}, slowdown s {}, wait p90 s {}, placed {}, \
         booked by curve {}",
        arm.name,
        booked_per_used(&outcome),
        outcome.over_excess / 1_000_000,
        outcome.slowed_ms / 1000,
        waits[waits.len() * 9 / 10] / 1000,
        outcome.waits.len(),
        outcome.curved.iter().filter(|curved| **curved).count(),
    );
    outcome
}

/// Asserts the curve mode against the flat step on the compile class, learned from the series
/// the compiles' hosts recorded. With the burst at the same point of every compile (the class
/// is shaped), the curve mode books compiles by their curve and keeps their bursts apart, so it
/// asks the hosts for less CPU past their cores than the flat step, whose flat booking lets the
/// bursts meet, and slows the jobs less. With the burst at another point of each compile, the
/// class fails its checks and the curve mode books no job by curve. Every job is placed in each.
/// Catches a curve arm that books the flat number on a shaped class, one that books by curve a
/// class whose bursts move, and a packing that lets the bursts of a shaped class meet.
#[test]
fn the_curve_mode_keeps_a_shaped_compiles_bursts_apart() {
    let flat = replayed(same, FLAT_STEP);
    let curve = replayed(same, CURVE_MODE);
    assert!(curve.curved.iter().any(|curved| *curved));
    assert!(flat.curved.iter().all(|curved| !*curved));
    assert!(
        curve.over_excess < flat.over_excess,
        "{} against {}",
        curve.over_excess,
        flat.over_excess
    );
    assert!(curve.slowed_ms < flat.slowed_ms);
    assert_eq!((flat.waits.len(), curve.waits.len()), (920, 920));
    let moved = replayed(moving, CURVE_MODE);
    assert!(moved.curved.iter().all(|curved| !*curved));
    assert_eq!(moved.waits.len(), 920);
}
