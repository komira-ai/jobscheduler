//! The replay on held-out days: the gate before `--placement-curves on`
//! (`docs/design/placement-profiles-live.md` section 4, "Farm-wide, before `on`", step 3).
//!
//! The trace's first `learn_days` days (of 24 h each from its first arrival) are the learning
//! days, and every job that arrives after them is held out. The flat step ([`FLAT_STEP`]) and the
//! curve mode ([`CURVE_MODE`]) each replay the whole trace once at the speed it arrived, every
//! job living its recorded CPU series ([`Shape::Recorded`]; a job whose series cannot stand for
//! its run lives a burst). The curve mode's classes learn only from the jobs of the learning days
//! ([`Setup::held_out_ms`]), so what they book in the held-out part was learned on other days;
//! the flat step's sizing learns from every job in both arms, as the job service's does. On the
//! held-out jobs, the curve mode passes when:
//! - its mean total time (each job's wait plus run, from its arrival to its end) is at most the
//!   flat step's plus the spread between two recorded days: how far the flat step's mean total
//!   time on the last learning day is from its mean on the day before (0 when either had no
//!   job);
//! - its throughput (the held-out jobs over the time from the part's start to the last one's
//!   end) is at least 98% of the flat step's;
//! - it asks the hosts for fewer core-seconds past their cores than the flat step, from the
//!   part's start on;
//! - it booked at least one held-out job by its curve, so that what it is judged on is a curve
//!   and not only the other order in which it ranks the hosts.

use std::fmt::Write as _;
use std::ops::Range;

use crate::report::{CURVE_MODE, FLAT_STEP};
use crate::shape::{self, Shape};
use crate::sim::{Farm, Outcome, Setup, replay};
use crate::trace::Job;

/// A day of the trace, in milliseconds.
pub const DAY_MS: u64 = 86_400_000;

/// The fewest learning days: the spread is taken between the last two.
pub const MIN_LEARN_DAYS: u32 = 2;

/// The least throughput the curve mode may keep, in hundredths of the flat step's.
const THROUGHPUT_PERCENT: u128 = 98;

/// What one arm did with the held-out jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    /// Their mean total time, from arrival to end, in milliseconds.
    mean_ms: u64,
    /// The time from the part's start to the end of the last of them, in milliseconds.
    span_ms: u64,
    /// The CPU asked past the hosts' cores from the part's start on, in millicore-ms.
    over: u128,
    /// How many of them were booked by their class's curve.
    curved: usize,
}

/// The mean total time (from arrival to end, in milliseconds) of the jobs of `jobs` that
/// arrived in `arrived`, as `outcome` ended them; `None` when none did.
fn mean_total(jobs: &[Job], outcome: &Outcome, arrived: &Range<u64>) -> Option<u64> {
    let totals: Vec<u64> = jobs
        .iter()
        .zip(&outcome.ended)
        .filter(|(job, _)| arrived.contains(&job.arrive_ms))
        .map(|(job, end)| end.saturating_sub(job.arrive_ms))
        .collect();
    let count = u64::try_from(totals.len())
        .ok()
        .filter(|count| *count > 0)?;
    Some(totals.iter().sum::<u64>() / count)
}

/// The spread between two days' mean total times: how far apart they are, and 0 when either day
/// had no job (no spread is measured, so the curve mode is held to the flat step's own time).
fn spread(day: Option<u64>, before: Option<u64>) -> u64 {
    day.zip(before)
        .map_or(0, |(day, before)| day.abs_diff(before))
}

/// What `outcome` did with the jobs of `jobs` that arrived at or after `from`.
fn held(jobs: &[Job], outcome: &Outcome, from: u64) -> Held {
    let part = jobs
        .iter()
        .zip(outcome.ended.iter().zip(&outcome.curved))
        .filter(|(job, _)| job.arrive_ms >= from);
    let last = part.clone().map(|(_, (end, _))| *end).max().unwrap_or(from);
    Held {
        mean_ms: mean_total(jobs, outcome, &(from..u64::MAX)).unwrap_or(0),
        span_ms: last.saturating_sub(from).max(1),
        over: outcome.held_over_excess,
        curved: part.filter(|(_, (_, curved))| **curved).count(),
    }
}

/// Whether the curve mode holds each of the four against the flat step, `spread_ms` the spread
/// between two recorded days: its total time, its throughput, its core-seconds past cores, and a
/// job booked by curve. Both arms end the same jobs, so the curve mode keeps 98% of the flat
/// step's throughput when its span is at most the flat step's over 0.98.
fn holds(flat: &Held, curve: &Held, spread_ms: u64) -> [bool; 4] {
    [
        curve.mean_ms <= flat.mean_ms.saturating_add(spread_ms),
        u128::from(curve.span_ms) * THROUGHPUT_PERCENT <= u128::from(flat.span_ms) * 100,
        curve.over < flat.over,
        curve.curved > 0,
    ]
}

/// `yes` or `no`.
fn yes(holds: bool) -> &'static str {
    if holds { "yes" } else { "no" }
}

/// The gate's section of the report for `jobs` (a trace) on `farm`: learning on the trace's
/// first `learn_days` days (the command line refuses fewer than [`MIN_LEARN_DAYS`]; fewer is
/// read as that many) and holding out the jobs that arrive after them.
#[must_use]
pub fn gate(jobs: &[Job], farm: Farm, learn_days: u32) -> String {
    let days = u64::from(learn_days.max(MIN_LEARN_DAYS));
    let from = days * DAY_MS;
    let mut text = "## The gate on held-out days\n\n".to_owned();
    let part: Vec<&Job> = jobs.iter().filter(|job| job.arrive_ms >= from).collect();
    if part.is_empty() {
        let _ = writeln!(
            text,
            "No job arrived after the trace's first {days} days: nothing is held out."
        );
        return text;
    }
    let setup = Setup {
        farm,
        shape: Shape::Recorded,
        speed: 1,
        held_out_ms: Some(from),
    };
    let (flat, curve) = (
        replay(jobs, FLAT_STEP, setup),
        replay(jobs, CURVE_MODE, setup),
    );
    let day = |day: u64| mean_total(jobs, &flat, &(day * DAY_MS..(day + 1) * DAY_MS));
    let spread = spread(day(days - 1), day(days - 2));
    let (flat, curve) = (held(jobs, &flat, from), held(jobs, &curve, from));
    let recorded = part
        .iter()
        .filter_map(|job| job.cpu_series.as_ref().map(|series| (series, job.cpu_ms)))
        .filter(|(series, cpu_ms)| shape::recorded(series, *cpu_ms, farm.milli).is_some())
        .count();
    let kept = holds(&flat, &curve, spread);
    let n = part.len();
    let secs = |ms: u64| ms as f64 / 1000.0;
    let per_hour = |arm: &Held| n as f64 * 3_600_000.0 / arm.span_ms as f64;
    let _ = writeln!(
        text,
        "Learned on the trace's first {days} days (24 h each from its first arrival); held out: \
         the {n} jobs that arrived after them, {recorded} with a CPU series that stands for their \
         run. Each arm replays the whole trace with every job living its recorded series (a job \
         without one lives a burst); the curve mode's classes learn only from the jobs of the \
         learning days, the flat step's sizing from every job.\n\n\
         | on the held-out jobs | flat step | curve mode | the curve mode must | holds |\n\
         |---|---|---|---|---|\n\
         | total time (wait plus run), mean s | {:.1} | {:.1} | be at most the flat step's plus \
         {:.1}, the spread of its mean between days {} and {days} | {} |\n\
         | throughput, jobs per hour | {:.1} | {:.1} | be at least {THROUGHPUT_PERCENT}% of the \
         flat step's | {} |\n\
         | core-s asked past the hosts' cores | {} | {} | be fewer | {} |\n\
         | jobs booked by curve | - | {} | be at least one | {} |\n",
        secs(flat.mean_ms),
        secs(curve.mean_ms),
        secs(spread),
        days - 1,
        yes(kept[0]),
        per_hour(&flat),
        per_hour(&curve),
        yes(kept[1]),
        flat.over / 1_000_000,
        curve.over / 1_000_000,
        yes(kept[2]),
        curve.curved,
        yes(kept[3]),
    );
    let verdict = if kept.iter().all(|each| *each) {
        "The curve mode passes the gate: no loss against the flat step on the held-out days."
    } else {
        "The curve mode does not pass the gate: the profiles stay in shadow \
         (`placement-profiles-live.md` section 5, stage 3)."
    };
    let _ = writeln!(text, "{verdict}");
    text
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
