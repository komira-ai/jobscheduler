//! The CPU a run asks for over its life, as the replay models it from what was recorded:
//! its CPU time, its wall time and its peak over about five seconds.
//!
//! Nothing records the curve of a run's CPU today, so the replay brackets it. Of every life
//! that uses the recorded CPU time in the recorded wall time and never goes above the
//! recorded peak, [`Shape::Burst`] is the burstiest (its peak for as long as its CPU time
//! lasts at that rate, nothing the rest of the time) and [`Shape::Blip`] the calmest that
//! still reaches the peak for five seconds. A run's real life lies between the two, so
//! every figure the replay reports is given under both. Without a recorded peak both are
//! [`Shape::Flat`]. [`Shape::ClassBurst`] is the burst at the same point of every run of a
//! class: the shaped class the curve mode books by its curve, which a test replays.
//!
//! An agent run with `--cpu-series` records the curve, and a trace carries it
//! ([`crate::trace::Job::cpu_series`]): [`Shape::Recorded`] runs each job as its series says
//! ([`recorded`]), and a job whose series cannot stand for its run as the burst.

use js_sizing::{RunSeries, series_agrees};
use js_types::CpuSeries;

/// The window the agent measures a run's peak over, in milliseconds (`PeakWindow` in the
/// agent's `crun/peak.rs`).
pub const PEAK_WINDOW_MS: u64 = 5_000;

/// A stretch of a run with one demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phase {
    /// How long it lasts when nothing slows it, in milliseconds.
    pub ms: u64,
    /// The CPU it asks for meanwhile, in millicores.
    pub milli: u64,
}

/// How the replay spreads a run's CPU time over its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shape {
    /// Its mean, all its life.
    Flat,
    /// Five seconds at its peak, the rest at the level that keeps its mean.
    Blip,
    /// Its peak while its CPU time lasts at that rate, nothing the rest of the time.
    Burst,
    /// [`Shape::Burst`], with the burst at the same point of every run of its class (where
    /// the other shapes place it by the run).
    ClassBurst,
    /// The run's recorded CPU series ([`recorded`]); [`Shape::Burst`] for a run whose series is
    /// missing or cannot stand for it.
    Recorded,
}

impl Shape {
    /// The shape's name in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Shape::Flat => "flat",
            Shape::Blip => "blip",
            Shape::Burst => "burst",
            Shape::ClassBurst => "class burst",
            Shape::Recorded => "recorded",
        }
    }
}

/// The most CPU the replay takes a run to average, in millicores: 24 cores. The store stamps
/// a job's start when the server hears of it, up to a heartbeat late, so the record of a run
/// of a few seconds can imply far more than it used; no run of ten seconds or more on the
/// farm averaged more (the report prints the most one did).
pub const MOST_MEAN_MILLI: u64 = 24_000;

/// A run's own wall time, in milliseconds: the recorded `wall_ms`, raised to what `cpu_ms`
/// of CPU takes at [`MOST_MEAN_MILLI`] (or on a whole host of `host_milli`, if smaller), and
/// at least 1.
#[must_use]
pub fn own_wall_ms(wall_ms: u64, cpu_ms: u64, host_milli: u64) -> u64 {
    let most = host_milli.clamp(1, MOST_MEAN_MILLI);
    let least = cpu_ms.saturating_mul(1000).div_ceil(most);
    wall_ms.max(least).max(1)
}

/// The phases of a run of `wall_ms` of its own time (see [`own_wall_ms`]) that used
/// `cpu_ms` of CPU and peaked at `peak_milli`, shaped as `shape`, on a host of `host_milli`.
/// `at` (0 to 999) places the burst: the thousandths of the run's other time before it.
#[must_use]
pub fn phases(
    shape: Shape,
    wall_ms: u64,
    cpu_ms: u64,
    peak_milli: Option<u32>,
    host_milli: u64,
    at: u64,
) -> Vec<Phase> {
    let wall = wall_ms.max(1);
    let total = cpu_ms.saturating_mul(1000);
    let mean = (total / wall).min(host_milli);
    let peak = peak_milli
        .map_or(mean, u64::from)
        .clamp(mean, host_milli.max(mean));
    let flat = vec![Phase {
        ms: wall,
        milli: mean,
    }];
    if peak == mean {
        return flat;
    }
    let burst = || {
        let busy = total.div_ceil(peak).clamp(1, wall);
        let idle = wall - busy;
        let before = idle * at / 1000;
        kept([(before, 0), (busy, peak), (idle - before, 0)])
    };
    match shape {
        Shape::Flat => flat,
        Shape::Burst | Shape::ClassBurst | Shape::Recorded => burst(),
        Shape::Blip => {
            let in_window = peak.saturating_mul(PEAK_WINDOW_MS);
            if wall <= PEAK_WINDOW_MS || in_window > total {
                // A run no longer than the window has its mean as its peak; one whose
                // window holds more than its CPU time was measured over a longer window
                // than five seconds. Either is shaped as the burst it can only be.
                return burst();
            }
            let rest = wall - PEAK_WINDOW_MS;
            let level = (total - in_window) / rest;
            let before = rest * at / 1000;
            kept([
                (before, level),
                (PEAK_WINDOW_MS, peak),
                (rest - before, level),
            ])
        }
    }
}

/// Whether `series` can stand for a run that used `cpu_ms` of CPU, as the job service judges a
/// run's series before its class learns it: it does not miss the run's start, and its total
/// agrees with the CPU time ([`series_agrees`]).
#[must_use]
pub fn usable(series: &CpuSeries, cpu_ms: u64) -> bool {
    let points: Vec<u32> = series.points.iter().map(|point| u32::from(*point)).collect();
    let run = RunSeries {
        granularity_secs: series.granularity_secs,
        points: &points,
        run_usec: series.run_usec,
    };
    !series.partial && series_agrees(run, cpu_ms.saturating_mul(1000))
}

/// The phases of a run whose host recorded `series` and that used `cpu_ms` of CPU, on a host of
/// `host_milli`: each point at its mean, held to the host, for the seconds it covers as the job
/// service reads a series (`granularity_secs` each; the last up to the series' run rounded up to
/// a second, at least one second and at most a point). `None` when the series has no point or
/// is not [`usable`].
#[must_use]
pub fn recorded(series: &CpuSeries, cpu_ms: u64, host_milli: u64) -> Option<Vec<Phase>> {
    if series.points.is_empty() || !usable(series, cpu_ms) {
        return None;
    }
    let span = u64::from(series.granularity_secs);
    let count = series.points.len() as u64;
    let last = (count - 1) * span;
    let end = series
        .run_usec
        .div_ceil(1_000_000)
        .clamp(last + 1, count * span);
    let phases = series.points.iter().zip(1..).map(|(point, at)| Phase {
        ms: 1000 * if at == count { end - last } else { span },
        milli: (u64::from(*point) * 125 / 2).min(host_milli),
    });
    Some(phases.collect())
}

/// The phases of `parts` (length, demand) that last at least a millisecond.
fn kept<const N: usize>(parts: [(u64, u64); N]) -> Vec<Phase> {
    parts
        .into_iter()
        .filter(|(ms, _)| *ms > 0)
        .map(|(ms, milli)| Phase { ms, milli })
        .collect()
}

/// The CPU time `phases` use, in millicore-milliseconds.
#[must_use]
pub fn cpu_of(phases: &[Phase]) -> u64 {
    phases.iter().map(|phase| phase.ms * phase.milli).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: u64 = 88_000;

    fn phase(ms: u64, milli: u64) -> Phase {
        Phase { ms, milli }
    }

    #[test]
    fn a_short_recorded_wall_is_raised_to_what_its_cpu_takes_on_a_whole_host() {
        // 19.7 s of CPU recorded in 2 ms: at 24 cores it takes at least 821 ms, and on a
        // host of 16 cores at least 1232 ms.
        assert_eq!(own_wall_ms(2, 19_700, HOST), 821);
        assert_eq!(own_wall_ms(2, 19_700, 16_000), 1232);
        assert_eq!(own_wall_ms(60_000, 120_000, HOST), 60_000);
        assert_eq!(own_wall_ms(0, 0, HOST), 1);
        assert_eq!(own_wall_ms(0, 5, 0), 5000);
    }

    #[test]
    fn a_run_with_no_peak_is_flat_at_its_mean_whatever_the_shape() {
        for shape in [Shape::Flat, Shape::Blip, Shape::Burst, Shape::ClassBurst] {
            assert_eq!(
                phases(shape, 60_000, 120_000, None, HOST, 500),
                vec![phase(60_000, 2000)]
            );
        }
        assert_eq!(phases(Shape::Burst, 0, 0, None, HOST, 0), vec![phase(1, 0)]);
    }

    #[test]
    fn a_compile_that_peaks_at_twenty_cores_is_bracketed_by_burst_and_blip() {
        // A minute at a mean of 2 cores (120 s of CPU) that peaked at 20.
        let peak = Some(20_000);
        assert_eq!(
            phases(Shape::Flat, 60_000, 120_000, peak, HOST, 500),
            vec![phase(60_000, 2000)]
        );
        // Burst: 6 s at 20 cores, idle 54 s, half of it before.
        let burst = phases(Shape::Burst, 60_000, 120_000, peak, HOST, 500);
        assert_eq!(
            phases(Shape::ClassBurst, 60_000, 120_000, peak, HOST, 500),
            burst,
            "a class's burst is the burst, placed by the caller"
        );
        assert_eq!(
            burst,
            vec![phase(27_000, 0), phase(6000, 20_000), phase(27_000, 0)]
        );
        // Blip: 5 s at 20 cores, the other 55 s at (120 - 100) / 55 = 0.363 cores.
        let blip = phases(Shape::Blip, 60_000, 120_000, peak, HOST, 0);
        assert_eq!(blip, vec![phase(5000, 20_000), phase(55_000, 363)]);
        assert_eq!(cpu_of(&burst), 120_000_000);
        assert_eq!(cpu_of(&blip), 119_965_000);
        // At the end of its idle time the burst comes last.
        assert_eq!(
            phases(Shape::Burst, 60_000, 120_000, peak, HOST, 999),
            vec![phase(53_946, 0), phase(6000, 20_000), phase(54, 0)]
        );
    }

    #[test]
    fn a_blip_that_cannot_be_is_a_burst() {
        // No longer than the peak's window.
        assert_eq!(
            phases(Shape::Blip, 4000, 4000, Some(2000), HOST, 0),
            vec![phase(2000, 2000), phase(2000, 0)]
        );
        // Five seconds at the peak hold more CPU than the run used.
        assert_eq!(
            phases(Shape::Blip, 10_000, 10_000, Some(3000), HOST, 0),
            vec![phase(3334, 3000), phase(6666, 0)]
        );
    }

    #[test]
    fn a_peak_is_held_between_the_mean_and_the_host() {
        // Under the mean: flat.
        assert_eq!(
            phases(Shape::Burst, 10_000, 20_000, Some(1000), HOST, 0),
            vec![phase(10_000, 2000)]
        );
        // Over the host: the host.
        assert_eq!(
            phases(Shape::Burst, 10_000, 88_000, Some(200_000), HOST, 0),
            vec![phase(1000, 88_000), phase(9000, 0)]
        );
        // A burst of CPU time under a millisecond at its peak still lasts one.
        assert_eq!(
            phases(Shape::Burst, 10_000, 0, Some(1000), HOST, 0),
            vec![phase(1, 1000), phase(9999, 0)]
        );
        assert_eq!(
            [
                Shape::Flat,
                Shape::Blip,
                Shape::Burst,
                Shape::ClassBurst,
                Shape::Recorded
            ]
            .map(Shape::name),
            ["flat", "blip", "burst", "class burst", "recorded"]
        );
    }

    /// Asserts that a run is shaped by its recorded series: each 2 s point at its mean (16
    /// sixteenths a core, 3 sixteenths 187 millicores), the last point for the second left of the
    /// run, a point past the host held to it, a last point whose run ends before it given one
    /// second; and that a series that misses the run's start, has no point, or whose total is
    /// more than 5% off the CPU time stands for no run. Catches a series read at the wrong scale,
    /// a last point given a whole span, and a partial or disagreeing series run as if whole.
    #[test]
    fn a_recorded_series_shapes_its_run_and_one_that_cannot_stand_for_it_shapes_none() {
        // 2 s at 2 cores, 2 s at 3/16, 1 s at 1 core: 4000 + 375 + 1000 = 5375 ms of CPU.
        let series = CpuSeries::new(2, vec![32, 3, 16], 5_000_000, false).unwrap();
        assert!(usable(&series, 5375));
        assert_eq!(
            recorded(&series, 5375, HOST),
            Some(vec![phase(2000, 2000), phase(2000, 187), phase(1000, 1000)])
        );
        // On a host of 1.5 cores the first point is held to the host.
        assert_eq!(
            recorded(&series, 5375, 1500),
            Some(vec![phase(2000, 1500), phase(2000, 187), phase(1000, 1000)])
        );
        // A single point covers the run; a last point the run ends before still covers a second.
        let one = CpuSeries::new(4, vec![16], 3_000_000, false).unwrap();
        assert_eq!(recorded(&one, 3000, HOST), Some(vec![phase(3000, 1000)]));
        let cut = CpuSeries::new(4, vec![16, 0], 0, false).unwrap();
        assert_eq!(
            recorded(&cut, 4000, HOST),
            Some(vec![phase(4000, 1000), phase(1000, 0)])
        );
        // 5% off agrees, more does not.
        assert!(usable(&series, 5657));
        assert!(!usable(&series, 5658));
        assert_eq!(recorded(&series, 5658, HOST), None);
        let partial = CpuSeries {
            partial: true,
            ..series.clone()
        };
        assert!(!usable(&partial, 5375));
        assert_eq!(recorded(&partial, 5375, HOST), None);
        let empty = CpuSeries::new(2, Vec::new(), 0, false).unwrap();
        assert!(usable(&empty, 0));
        assert_eq!(recorded(&empty, 0, HOST), None);
    }
}
