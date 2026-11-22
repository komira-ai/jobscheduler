//! The report of a trace: what its jobs look like, how far the replay can be trusted, and what
//! each way of packing does with the same arrivals (`docs/design/cpu-over-time.md`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::shape::{MOST_MEAN_MILLI, PEAK_WINDOW_MS, Shape, own_wall_ms};
use crate::sim::{Arm, Bookings, Farm, Outcome, Rule, Setup, as_placed, replay};
use crate::trace::{End, Job};

/// The ways of packing the report compares, in its order.
pub const ARMS: [Arm; 9] = [
    Arm {
        name: "recorded bookings",
        bookings: Bookings::Recorded,
        rule: Rule::Booked,
        limit_permille: 1100,
    },
    Arm {
        name: "A median, 1.1",
        bookings: Bookings::Learned,
        rule: Rule::Booked,
        limit_permille: 1100,
    },
    Arm {
        name: "A median, 1.0",
        bookings: Bookings::Learned,
        rule: Rule::Booked,
        limit_permille: 1000,
    },
    Arm {
        name: "A median, 0.9",
        bookings: Bookings::Learned,
        rule: Rule::Booked,
        limit_permille: 900,
    },
    Arm {
        name: "B reserve z=2, 1.1",
        bookings: Bookings::Learned,
        rule: Rule::Reserve { z_tenths: 20 },
        limit_permille: 1100,
    },
    Arm {
        name: "C measured, 1.0",
        bookings: Bookings::Learned,
        rule: Rule::Measured,
        limit_permille: 1000,
    },
    Arm {
        name: "C measured, 1.1",
        bookings: Bookings::Learned,
        rule: Rule::Measured,
        limit_permille: 1100,
    },
    Arm {
        name: "D curve, 1.0",
        bookings: Bookings::Learned,
        rule: Rule::Curve,
        limit_permille: 1000,
    },
    Arm {
        name: "D curve, 1.1",
        bookings: Bookings::Learned,
        rule: Rule::Curve,
        limit_permille: 1100,
    },
];

/// The arrival speeds the options are replayed at: as recorded, and four times as fast (the
/// same jobs in a quarter of the time, to see where each way of packing saturates).
pub const SPEEDS: [u64; 2] = [1, 4];

/// The two shapes that bracket a run's life (see [`crate::shape`]).
pub const SHAPES: [Shape; 2] = [Shape::Blip, Shape::Burst];

/// The classes the report lists by name, the most CPU first.
const TOP_CLASSES: usize = 15;

/// A class whose typical run lasts at least this long, in milliseconds, is long enough for a
/// curve over its life to be worth predicting.
const LONG_MS: u64 = 60_000;

/// A run at least this long, in milliseconds, has a recorded wall time the store's late start
/// stamp barely shortens.
const LONG_RUN_MS: u64 = 10_000;

/// The runs the summary describes apart, in milliseconds: those that carry the CPU.
const THIRTY_S: u64 = 30_000;

/// The waits the checks compare twice: of every job, and of the jobs that arrived after the
/// trace's first hour, when the replayed farm holds as many jobs as the recorded one did.
const WARM_MS: u64 = 3_600_000;

/// A long class whose typical peak is at least this many times its typical mean is bursty.
const BURSTY_RATIO: u64 = 2;

/// The report of `jobs` (a trace) on `farm`.
#[must_use]
pub fn report(jobs: &[Job], farm: Farm) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# CPU packing replay\n\n{}", summary(jobs, farm));
    classes(&mut text, jobs, farm);
    checks(&mut text, jobs, farm);
    options(&mut text, jobs, farm);
    text
}

/// The nearest-rank `q` percentile (0 to 100) of `sorted`, 0 when empty.
fn percentile(sorted: &[u64], q: u64) -> u64 {
    let rank = (sorted.len() as u64 * q).div_ceil(100).max(1);
    sorted
        .get(usize::try_from(rank - 1).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(0)
}

/// `values`, sorted.
fn sorted(mut values: Vec<u64>) -> Vec<u64> {
    values.sort_unstable();
    values
}

/// `part` of `whole` as a percentage with one decimal, `0.0` of nothing.
fn percent(part: f64, whole: f64) -> String {
    if whole > 0.0 {
        format!("{:.1}", part * 100.0 / whole)
    } else {
        "0.0".to_owned()
    }
}

/// Milliseconds as whole seconds.
fn secs(ms: u64) -> String {
    ms.div_ceil(1000).to_string()
}

/// Millicores as cores with one decimal.
fn cores(milli: u64) -> String {
    format!("{:.1}", milli as f64 / 1000.0)
}

/// The trace in one paragraph: its jobs, its span, the CPU they used, their recorded waits
/// and the most a run of ten seconds or more averaged.
#[must_use]
pub fn summary(jobs: &[Job], farm: Farm) -> String {
    let span = jobs
        .iter()
        .map(|job| job.arrive_ms + job.wait_ms + job.start_ms + job.wall_ms)
        .max()
        .unwrap_or(0);
    let cpu: u64 = jobs.iter().map(|job| job.cpu_ms).sum();
    let waits = sorted(jobs.iter().map(|job| job.wait_ms).collect());
    let farm_cores = farm.hosts as f64 * farm.milli as f64 / 1000.0;
    let mean = cpu as f64 / span.max(1) as f64;
    let most = jobs
        .iter()
        .filter(|job| job.wall_ms >= LONG_RUN_MS)
        .map(|job| job.cpu_ms * 1000 / job.wall_ms)
        .max()
        .unwrap_or(0);
    let long: Vec<&Job> = jobs.iter().filter(|job| job.wall_ms >= THIRTY_S).collect();
    let means = sorted(
        long.iter()
            .map(|job| job.cpu_ms * 1000 / job.wall_ms)
            .collect(),
    );
    let long_cpu: u64 = long.iter().map(|job| job.cpu_ms).sum();
    format!(
        "{} jobs over {:.1} h on {} hosts of {} cores: {:.0} core-hours of CPU, a mean of {:.1} \
         cores ({}% of the farm). Recorded wait p50 {} s, p90 {} s, p99 {} s. The most CPU a run \
         of {} s or more averaged: {} cores (a shorter run is taken to average at most {}). Runs \
         of {} s or more: {}, with {}% of the CPU, their mean CPU p25 {}, p50 {}, p75 {} cores.\n",
        jobs.len(),
        span as f64 / 3_600_000.0,
        farm.hosts,
        farm.milli / 1000,
        cpu as f64 / 3_600_000.0,
        mean,
        percent(mean, farm_cores),
        secs(percentile(&waits, 50)),
        secs(percentile(&waits, 90)),
        secs(percentile(&waits, 99)),
        LONG_RUN_MS / 1000,
        cores(most),
        cores(MOST_MEAN_MILLI),
        THIRTY_S / 1000,
        long.len(),
        percent(long_cpu as f64, cpu as f64),
        cores(percentile(&means, 25)),
        cores(percentile(&means, 50)),
        cores(percentile(&means, 75)),
    )
}

/// What one class's good runs looked like.
#[derive(Default)]
struct Class {
    runs: u64,
    cpu_ms: u64,
    walls: Vec<u64>,
    means: Vec<u64>,
    peaks: Vec<u64>,
    /// Each run's peak over its mean, in tenths.
    ratios: Vec<u64>,
    /// The share of each run's life near its peak if it was a blip, and if a burst, in
    /// thousandths.
    blip: Vec<u64>,
    burst: Vec<u64>,
}

/// The classes of the trace's good runs, and the table of the largest.
fn classes(text: &mut String, jobs: &[Job], farm: Farm) {
    let mut by_class: BTreeMap<u32, Class> = BTreeMap::new();
    for job in jobs.iter().filter(|job| job.end == End::Ok) {
        let wall = own_wall_ms(job.wall_ms, job.cpu_ms, farm.milli);
        let mean = (job.cpu_ms * 1000 / wall).max(1);
        let class = by_class.entry(job.class).or_default();
        class.runs += 1;
        class.cpu_ms += job.cpu_ms;
        class.walls.push(wall);
        class.means.push(mean);
        if let Some(peak) = job.peak_milli {
            let peak = u64::from(peak).clamp(mean, farm.milli.max(mean));
            class.peaks.push(peak);
            class.ratios.push(peak * 10 / mean);
            class.burst.push(mean * 1000 / peak);
            class.blip.push((PEAK_WINDOW_MS * 1000 / wall).min(1000));
        }
    }
    let total: u64 = by_class.values().map(|class| class.cpu_ms).sum();
    let runs: u64 = by_class.values().map(|class| class.runs).sum();
    let mut largest: Vec<(&u32, &Class)> = by_class.iter().collect();
    largest.sort_by_key(|(index, class)| (std::cmp::Reverse(class.cpu_ms), **index));
    let _ = writeln!(
        text,
        "## Jobs by class\n\n{} classes, {} runs that ended with 0. The {} with the most CPU:\n\n\
         | class | runs | CPU % | wall p50 s | mean p50 | peak p50 | peak/mean p50 | near peak p50, blip to burst |\n\
         |---|---|---|---|---|---|---|---|",
        by_class.len(),
        runs,
        TOP_CLASSES.min(by_class.len()),
    );
    for (index, class) in largest.iter().take(TOP_CLASSES) {
        let median = |values: &[u64]| percentile(&sorted(values.to_vec()), 50);
        let _ = writeln!(
            text,
            "| {index} | {} | {} | {} | {} | {} | {} | {} |",
            class.runs,
            percent(class.cpu_ms as f64, total as f64),
            secs(median(&class.walls)),
            cores(median(&class.means)),
            peak_cell(class, cores(median(&class.peaks))),
            peak_cell(class, format!("{:.1}", median(&class.ratios) as f64 / 10.0)),
            peak_cell(
                class,
                format!(
                    "{}% to {}%",
                    percent(median(&class.blip) as f64, 1000.0),
                    percent(median(&class.burst) as f64, 1000.0)
                )
            ),
        );
    }
    let kinds = [
        ("short (wall p50 under 60 s)", Kind::Short),
        ("long and bursty (peak/mean p50 2 or more)", Kind::Bursty),
        ("long and steady", Kind::Steady),
        ("long, no peak recorded", Kind::Unknown),
    ];
    let _ = writeln!(
        text,
        "\n| kind | classes | runs % | CPU % |\n|---|---|---|---|"
    );
    for (name, kind) in kinds {
        let of: Vec<&Class> = by_class
            .values()
            .filter(|class| kind_of(class) == kind)
            .collect();
        let _ = writeln!(
            text,
            "| {name} | {} | {} | {} |",
            of.len(),
            percent(
                of.iter().map(|class| class.runs).sum::<u64>() as f64,
                runs as f64
            ),
            percent(
                of.iter().map(|class| class.cpu_ms).sum::<u64>() as f64,
                total as f64
            ),
        );
    }
    let _ = writeln!(text);
}

/// What a class's typical run is like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Short,
    Bursty,
    Steady,
    Unknown,
}

/// The kind of `class`.
fn kind_of(class: &Class) -> Kind {
    let median = |values: &[u64]| percentile(&sorted(values.to_vec()), 50);
    if median(&class.walls) < LONG_MS {
        Kind::Short
    } else if class.ratios.is_empty() {
        Kind::Unknown
    } else if median(&class.ratios) >= BURSTY_RATIO * 10 {
        Kind::Bursty
    } else {
        Kind::Steady
    }
}

/// `cell`, or `-` for a class with no recorded peak.
fn peak_cell(class: &Class, cell: String) -> String {
    if class.peaks.is_empty() {
        "-".to_owned()
    } else {
        cell
    }
}

/// How far the replay can be trusted: the waits it gives the recorded bookings beside the
/// recorded waits, and the slowdown it models for the jobs as they were placed beside the
/// contention they recorded.
fn checks(text: &mut String, jobs: &[Job], farm: Farm) {
    let _ = writeln!(
        text,
        "## Checks of the replay\n\n\
         Waits, recorded and replayed with the recorded bookings (seconds), of every job and of \
         the jobs that arrived after the first hour. A replayed job is placed at the first \
         one-second tick after it arrives, so a replayed wait is up to a second longer, and every \
         wait here is rounded up to the second:\n\n\
         | | p50 | p90 | p99 | after 1 h: p50 | p90 | p99 |\n|---|---|---|---|---|---|---|"
    );
    let recorded: Vec<u64> = jobs.iter().map(|job| job.wait_ms).collect();
    waits_row(text, "recorded", jobs, &recorded);
    for shape in SHAPES {
        let outcome = replay(
            jobs,
            ARMS[0],
            Setup {
                farm,
                shape,
                speed: 1,
            },
        );
        waits_row(
            text,
            &format!("replayed, {}", shape.name()),
            jobs,
            &outcome.waited,
        );
    }
    let _ = writeln!(
        text,
        "\nThe jobs as placed, by the contention they recorded (the time some task of the job \
         waited for a CPU, over its wall time), against the slowdown the replay models for them \
         (their lost time over their own wall time):\n\n\
         | recorded contention | runs | recorded, mean % | modelled blip, mean % | modelled burst, mean % |\n\
         |---|---|---|---|---|"
    );
    let modelled: Vec<Outcome> = SHAPES
        .iter()
        .map(|&shape| {
            as_placed(
                jobs,
                Setup {
                    farm,
                    shape,
                    speed: 1,
                },
            )
        })
        .collect();
    let bands = [
        ("none recorded", None),
        ("under 1%", Some((0, 10))),
        ("1% to 10%", Some((10, 100))),
        ("10% or more", Some((100, u64::MAX))),
    ];
    for (name, band) in bands {
        let mut runs = 0u64;
        let mut recorded = 0.0;
        let mut shares = [0.0; 2];
        for (index, job) in jobs.iter().enumerate() {
            let wall = job.wall_ms.max(1);
            let permille = job.pressure_ms.map(|ms| ms * 1000 / wall);
            let inside = match (band, permille) {
                (None, None) => true,
                (Some((low, high)), Some(permille)) => permille >= low && permille < high,
                _ => false,
            };
            if !inside {
                continue;
            }
            runs += 1;
            recorded += permille.unwrap_or(0) as f64 / 10.0;
            for (share, outcome) in shares.iter_mut().zip(&modelled) {
                let own = own_wall_ms(job.wall_ms, job.cpu_ms, farm.milli);
                *share += outcome.slowed[index] as f64 * 100.0 / own as f64;
            }
        }
        let mean = |sum: f64| format!("{:.1}", sum / runs.max(1) as f64);
        let _ = writeln!(
            text,
            "| {name} | {runs} | {} | {} | {} |",
            if band.is_some() {
                mean(recorded)
            } else {
                "-".to_owned()
            },
            mean(shares[0]),
            mean(shares[1]),
        );
    }
    let _ = writeln!(text);
}

/// The row of `waits` (by trace index) of every job of `jobs` and of those that arrived after
/// the first hour.
fn waits_row(text: &mut String, name: &str, jobs: &[Job], waits: &[u64]) {
    let all = sorted(waits.to_vec());
    let warm = sorted(
        jobs.iter()
            .zip(waits)
            .filter(|(job, _)| job.arrive_ms >= WARM_MS)
            .map(|(_, wait)| *wait)
            .collect(),
    );
    let _ = writeln!(
        text,
        "| {name} | {} | {} | {} | {} | {} | {} |",
        secs(percentile(&all, 50)),
        secs(percentile(&all, 90)),
        secs(percentile(&all, 99)),
        secs(percentile(&warm, 50)),
        secs(percentile(&warm, 90)),
        secs(percentile(&warm, 99)),
    );
}

/// Every arm at every speed and shape.
fn options(text: &mut String, jobs: &[Job], farm: Farm) {
    let cpu: u128 = jobs.iter().map(|job| u128::from(job.cpu_ms) * 1000).sum();
    for speed in SPEEDS {
        let _ = writeln!(
            text,
            "## Options, arrivals at {speed}x\n\n\
             | arm | shape | jobs/h | wait p50 s | p90 s | p99 s | jobs per host | host-time over \
             cores % | asked past cores, mean | slowdown % | CPU used % | booked/used | used above \
             booking % | plan error % | host-time above plan % |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
        );
        let mut alone = 0;
        for arm in ARMS {
            for shape in SHAPES {
                let outcome = replay(jobs, arm, Setup { farm, shape, speed });
                alone += outcome.alone;
                row(text, arm, shape, &outcome, (farm, cpu));
            }
        }
        let _ = writeln!(
            text,
            "\nJobs no host admitted even with nothing on the farm, run alone on an empty host, \
             over every row: {alone}.\n"
        );
    }
}

/// One arm's row.
fn row(text: &mut String, arm: Arm, shape: Shape, outcome: &Outcome, (farm, cpu): (Farm, u128)) {
    let span = outcome.last_end_ms.max(1) as f64;
    let host_time = farm.hosts as f64 * span;
    let capacity = host_time * farm.milli as f64;
    let waits = sorted(outcome.waits.clone());
    let over = if outcome.over_ms > 0 {
        cores((outcome.over_excess / u128::from(outcome.over_ms)) as u64)
    } else {
        "-".to_owned()
    };
    let _ = writeln!(
        text,
        "| {} | {} | {:.0} | {} | {} | {} | {:.1} | {} | {} | {} | {} | {:.2} | {} | {} | {} |",
        arm.name,
        shape.name(),
        outcome.waits.len() as f64 * 3_600_000.0 / span,
        secs(percentile(&waits, 50)),
        secs(percentile(&waits, 90)),
        secs(percentile(&waits, 99)),
        outcome.held_ms as f64 / host_time,
        percent(outcome.over_ms as f64, host_time),
        over,
        percent(outcome.slowed_ms as f64, outcome.own_ms as f64),
        percent(outcome.used, capacity),
        outcome.booked as f64 / cpu.max(1) as f64,
        percent(outcome.above_booking as f64, cpu as f64),
        percent(outcome.plan_error as f64, capacity),
        percent(outcome.under_ms as f64, host_time),
    );
}

#[cfg(test)]
mod tests;
