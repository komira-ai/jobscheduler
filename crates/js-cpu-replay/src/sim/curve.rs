//! The curve mode of the replay: classes learn their CPU over a job's age, and the jobs of
//! a class that passes the four checks book it (`docs/design/placement-profiles-live.md`
//! sections 2 to 4).
//!
//! The trace has each job's mean and five-second peak, not its series, so the replay teaches
//! on what it synthesised: at each successful run's end the class's `js_sizing::Profile` is
//! taught the run's own phases ([`crate::shape`]) as a series of 2 s means, in sixteenths of a
//! core, and its [`SeriesWindow`] that the series was usable (every synthesised series is). A
//! job of a class that passes [`Checks`] books [`Curve::of`] the profile, with the flat step
//! (the `Learned` booking) where a slot has too few runs; every other job books flat.
//!
//! [`Rule::Packed`](super::Rule::Packed) places by the curves: a host takes a job when, at
//! every slot ahead, what its jobs are charged ([`Curve::charge`], their flat booking when
//! they have no curve) and the job's booking at that slot stay within the arm's limit, with
//! the host's residual: what its reading shows beyond its jobs' charges now, carried to every
//! slot. The figure that ranks the hosts leaves the residual out, as the design's pack limit
//! does. A job that waited [`MAX_WAIT_MS`] at the head of the queue with no host fitting its
//! curve is tested flat from then on.

use std::collections::BTreeMap;
use std::rc::Rc;

use js_sizing::{
    CPU_UNIT, CURVE_SLOTS, Checks, Clamp, Curve, Profile, RunSeries, SLOT_SECS, SeriesWindow,
    TAIL_SLOT,
};

use super::{Bookings, Host, Running, World};
use crate::shape::Phase;

/// How long a job of a curve class waits at the head of the queue with no host fitting its
/// curve before it is tested with its flat booking, in milliseconds
/// (`curve_max_wait_seconds`, 30 s by default).
pub const MAX_WAIT_MS: u64 = 30_000;

/// Milliseconds of one slot of a curve.
const SLOT_MS: u64 = SLOT_SECS as u64 * 1000;

/// The least a slot books, in millicores: a quarter core.
const LEAST_MILLI: u64 = 250;

/// A slot books at most this part of a host's cores: a quarter, rounded down to a quarter core.
const HOST_SHARE: u64 = 4;

/// What the replay learns of one class: its profile, whether its runs' series were usable, and
/// the curve it last booked.
#[derive(Debug, Clone)]
pub(super) struct Learning {
    profile: Profile,
    series: SeriesWindow,
    /// The curve last booked and the flat booking it was made with; a run taught clears it.
    /// The weights of the kept runs fall by one factor as time passes, so the curve of the
    /// moment a run was taught is the curve of any later moment.
    booked: Option<(u64, Rc<Curve>)>,
}

impl Learning {
    /// A class with no run.
    pub(super) fn new() -> Self {
        Self {
            profile: Profile::new(CPU_UNIT),
            series: SeriesWindow::default(),
            booked: None,
        }
    }

    /// Teaches a usable run that ended at `ended_ms` and lasted `run_ms`, whose 2 s means were
    /// `points`; `clean` when it was not slowed.
    pub(super) fn teach(&mut self, points: &[u32], run_ms: u64, ended_ms: u64, clean: bool) {
        let run = RunSeries {
            granularity_secs: SLOT_SECS,
            points,
            run_usec: run_ms.saturating_mul(1000),
        };
        self.profile.check_and_learn(run, ended_ms, clean);
        self.series.push(true);
        self.booked = None;
    }

    /// Whether the class passes all four checks.
    pub(super) fn passes(&self) -> bool {
        Checks::of(&self.profile, &self.series).pass()
    }

    /// The curve of the class with `flat` booked where a slot has too few runs, held to
    /// `clamp`, as of `now`.
    fn curve(&mut self, flat: u64, clamp: Clamp, now: u64) -> Rc<Curve> {
        match &self.booked {
            Some((was, curve)) if *was == flat => Rc::clone(curve),
            _ => {
                let curve = Rc::new(Curve::of(&self.profile, flat, clamp, now));
                self.booked = Some((flat, Rc::clone(&curve)));
                curve
            }
        }
    }
}

/// What the replay keeps for the curve mode.
#[derive(Debug, Default)]
pub(super) struct Curves {
    /// What each class (by trace index) has learned.
    classes: BTreeMap<u32, Learning>,
    /// When the job at the head of the queue first found no host fitting its curve: the head
    /// changes only when a job is placed, which clears it.
    stalled: Option<u64>,
}

impl Curves {
    /// A job was placed: nothing waits at the head.
    pub(super) fn placed(&mut self) {
        self.stalled = None;
    }
}

/// The 2 s means of the demand of `phases` over the `run_ms` that follow their first `from_ms`
/// (the start of a job holds its booking and asks for nothing, and is no part of its series),
/// in sixteenths of a core, rounded half up; the last point is the mean of what remains.
pub(super) fn series(phases: &[Phase], from_ms: u64, run_ms: u64) -> Vec<u32> {
    let points = run_ms.div_ceil(SLOT_MS);
    let mut sums = vec![0u128; usize::try_from(points).unwrap_or(0)];
    let end = from_ms + run_ms;
    let mut at = 0;
    for phase in phases {
        let (from, to) = (at.max(from_ms), (at + phase.ms).min(end));
        at += phase.ms;
        for point in (from.saturating_sub(from_ms) / SLOT_MS)..points {
            let lo = (from_ms + point * SLOT_MS).max(from);
            let hi = (from_ms + (point + 1) * SLOT_MS).min(to);
            if hi <= lo {
                break;
            }
            sums[point as usize] += u128::from(hi - lo) * u128::from(phase.milli);
        }
    }
    sums.iter()
        .enumerate()
        .map(|(point, sum)| {
            let len = u128::from(SLOT_MS.min(run_ms - point as u64 * SLOT_MS));
            u32::try_from((sum * 16 + len * 500) / (len * 1000)).unwrap_or(u32::MAX)
        })
        .collect()
}

impl Running {
    /// The milliseconds the job has been running at `now`: from its start, which follows its
    /// placement by the time it took to start.
    fn age_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.start)
            .saturating_sub(self.own.saturating_sub(self.run))
    }

    /// What it is charged at slot `k` ahead of `now`: its flat booking, or its curve's charge.
    fn charge(&self, now: u64, k: usize) -> u64 {
        self.curve.as_ref().map_or(self.booking.milli, |curve| {
            curve.charge(self.age_ms(now), k, self.booking.milli)
        })
    }

    /// What it books at slot `k` of its life: its flat booking, or its curve's.
    fn books(&self, k: usize) -> u64 {
        self.curve
            .as_ref()
            .map_or(self.booking.milli, |curve| curve.booking(k))
    }

    /// What it booked over `wall_ms` from its placement, in millicore-ms: its flat booking
    /// times the time, or its curve's bookings, each slot's times the time spent in it (the
    /// time before its start at the first slot).
    pub(super) fn booked(&self, wall_ms: u64) -> u128 {
        let Some(curve) = &self.curve else {
            return u128::from(self.booking.milli) * u128::from(wall_ms);
        };
        let before = self.own.saturating_sub(self.run).min(wall_ms);
        let age = wall_ms - before;
        let mut total = u128::from(curve.booking(0)) * u128::from(before);
        for slot in 0..CURVE_SLOTS {
            let from = slot as u64 * SLOT_MS;
            let to = if slot == TAIL_SLOT {
                u64::MAX
            } else {
                from + SLOT_MS
            };
            let ms = age.min(to).saturating_sub(from);
            total += u128::from(curve.booking(slot)) * u128::from(ms);
        }
        total
    }
}

impl Host {
    /// The figure [`Rule::Packed`](super::Rule::Packed) gives it with `new` on it, or `None`
    /// past `limit`: the fullest slot of what its jobs are charged and `new` books, which fits
    /// when the host's residual on top of it is within `limit`.
    pub(super) fn packed(&self, new: &Running, limit: u64) -> Option<u64> {
        let now = new.start;
        let mut booked = [0u64; CURVE_SLOTS];
        for run in &self.runs {
            for (k, slot) in booked.iter_mut().enumerate() {
                *slot += run.charge(now, k);
            }
        }
        let covered: u64 = self
            .runs
            .iter()
            .filter(|run| run.start < self.read_at)
            .map(|run| run.charge(now, 0))
            .sum();
        let residual = self.measured().saturating_sub(covered);
        let figure = (0..CURVE_SLOTS)
            .map(|k| booked[k] + new.books(k))
            .max()
            .unwrap_or(0);
        (figure + residual <= limit).then_some(figure)
    }
}

impl World<'_> {
    /// Gives `new`, a job to be placed at `now`, its curve when the arm books curves, its
    /// class passes the four checks and it has not waited [`MAX_WAIT_MS`] at the head with no
    /// host fitting it; otherwise it keeps its flat booking.
    pub(super) fn curved(&mut self, new: &mut Running, now: u64) {
        if self.arm.bookings != Bookings::Curve {
            return;
        }
        let waited = self
            .curves
            .stalled
            .is_some_and(|since| now.saturating_sub(since) >= MAX_WAIT_MS);
        let most = self.setup.farm.milli / HOST_SHARE / LEAST_MILLI * LEAST_MILLI;
        let clamp = Clamp {
            least_millis: LEAST_MILLI,
            most_millis: most,
        };
        let class = self.curves.classes.get_mut(&self.jobs[new.job].class);
        if let Some(class) = class.filter(|class| !waited && class.passes()) {
            new.curve = Some(class.curve(new.booking.milli, clamp, now));
        }
    }

    /// Notes that no host took `new`, the job at the head of the queue at `now`: when it has a
    /// curve and is the first job to find none, the wait bound starts.
    pub(super) fn stall(&mut self, new: &Running, now: u64) {
        if new.curve.is_some() && self.curves.stalled.is_none() {
            self.curves.stalled = Some(now);
        }
    }

    /// Teaches the class of `run`, a successful run that ended at `end`, its series.
    pub(super) fn teach_curve(&mut self, run: &Running, end: u64, slowed_ms: u64) {
        if self.arm.bookings != Bookings::Curve {
            return;
        }
        let from = run.own.saturating_sub(run.run);
        let points = series(&run.phases, from, run.run);
        let class = self.jobs[run.job].class;
        let learning = self
            .curves
            .classes
            .entry(class)
            .or_insert_with(Learning::new);
        learning.teach(&points, run.run, end, slowed_ms == 0);
    }
}

#[cfg(test)]
#[path = "curve_tests.rs"]
mod tests;
