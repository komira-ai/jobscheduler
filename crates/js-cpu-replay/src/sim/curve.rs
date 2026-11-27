//! The curve mode of the replay: classes learn their CPU over a job's age, and the jobs of
//! a class that passes the four checks book it (`docs/design/placement-profiles-live.md`
//! sections 2 to 4).
//!
//! Each class learns as the job service's history does (`sizes_profile.rs` in
//! `js-sched-placer`): at each successful run's end, the series its host recorded
//! ([`crate::trace::Job::cpu_series`]) teaches the class's `js_sizing::Profile` when it is
//! usable ([`crate::shape::usable`]: whole, and agreeing with the run's CPU time), and the
//! class's [`SeriesWindow`] records whether it was; a run with no series, or one that is no
//! success, of a class that exists records an unusable one. A run is clean when the replay
//! slowed it by less than a tenth of its run ([`clean`]), the bound the job service puts on its
//! node's CPU pressure. Nothing is taught from a job of the held-out part ([`super::Setup`]). A job of
//! a class that passes [`Checks`] books [`Curve::of`] the profile, with the flat step (the
//! `Learned` booking) where a slot has too few runs; every other job books flat.
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
    CPU_PRESSURE_MAX_PERMILLE, CPU_UNIT, CURVE_SLOTS, Checks, Clamp, Curve, Profile, RunSeries,
    SLOT_SECS, SeriesWindow, TAIL_SLOT,
};
use js_types::CpuSeries;

use super::{Bookings, Host, Running, World};
use crate::shape;
use crate::trace::End;

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

/// Whether a run that ran `ran_ms` from its start and was slowed `slowed_ms` of it is clean: its
/// slowdown is under [`CPU_PRESSURE_MAX_PERMILLE`] of its run, as the job service takes a run
/// on a node under that pressure, and as the replay's flat sizing takes one.
pub(super) fn clean(slowed_ms: u64, ran_ms: u64) -> bool {
    slowed_ms.saturating_mul(1000) / ran_ms.max(1) < CPU_PRESSURE_MAX_PERMILLE
}

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

    /// Teaches a successful run that ended at `ended_ms` and used `cpu_ms` of CPU, whose host
    /// recorded `series`; `clean` when it was not slowed. The profile learns the series when it
    /// is [`shape::usable`], and the window records whether it was.
    pub(super) fn teach(
        &mut self,
        series: Option<&CpuSeries>,
        cpu_ms: u64,
        ended_ms: u64,
        clean: bool,
    ) {
        let usable = series.filter(|series| shape::usable(series, cpu_ms));
        if let Some(series) = usable {
            let points: Vec<u32> = series
                .points
                .iter()
                .map(|point| u32::from(*point))
                .collect();
            let run = RunSeries {
                granularity_secs: series.granularity_secs,
                points: &points,
                run_usec: series.run_usec,
            };
            self.profile.check_and_learn(run, ended_ms, clean);
            self.booked = None;
        }
        self.series.push(usable.is_some());
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

    /// Teaches the class of `run`, which ended at `end` slowed by `slowed_ms`, what its host
    /// recorded, when the arm books curves and its job arrived before the held-out part: a
    /// successful run its series, and one that is no success an unusable series, for a class
    /// that exists (it starts none).
    pub(super) fn teach_curve(&mut self, run: &Running, end: u64, slowed_ms: u64) {
        let jobs = self.jobs;
        let job = &jobs[run.job];
        let arrived = job.arrive_ms / self.setup.speed.max(1);
        let held = self.setup.held_out_ms.is_some_and(|from| arrived >= from);
        if self.arm.bookings != Bookings::Curve || held {
            return;
        }
        let classes = &mut self.curves.classes;
        if job.end != End::Ok {
            if let Some(learning) = classes.get_mut(&job.class) {
                learning.series.push(false);
            }
            return;
        }
        let learning = classes.entry(job.class).or_insert_with(Learning::new);
        let ran = end.saturating_sub(run.start).saturating_sub(job.start_ms);
        learning.teach(
            job.cpu_series.as_ref(),
            job.cpu_ms,
            end,
            clean(slowed_ms, ran),
        );
    }
}

#[cfg(test)]
#[path = "curve_tests.rs"]
mod tests;
