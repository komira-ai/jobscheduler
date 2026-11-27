//! Backfill on a held worker: which later work may be placed on a worker held for a large
//! request (a whole-machine mark, an action that owns its worker, a blocked head) while the
//! worker empties for it.
//!
//! A held worker has a start time: when the request it is held for is expected to start
//! there (for a whole-machine mark or an owner action, the end of the worker's tail; for a
//! blocked head, the first instant its charge fits, [`crate::admission`]'s `start_time`).
//! Work after the request in queue order is placed there only if its own end, plus the
//! margin kept spare ([`Backfill::with_margin`], [`Backfill::DEFAULT_MARGIN`] unless set),
//! is no later than that start ([`js_whole::cost::fits_in_tail`]). Work whose end is not
//! known never is, nor is any work on a worker whose start time is not known.
//!
//! The end of work is its start plus its duration: the P90 of its key's wall minutes
//! (the raw P90 the tail reads, not the charge's expected life, which adds
//! `expected_life_delta_minutes`) cut to its max wall time, else the max wall time. It
//! counts only when the max wall time is enforced: something ends the work at that bound
//! without the scheduler's help ([`Backfill::enforced`]). A bound above the ceiling is
//! taken as no bound, and work with no bound has no end.
//!
//! The same setting gives the scheduler two readings of a lease's end outside backfill.
//! A lease at or past its end is read as ending the margin from now, the time for whatever
//! enforces its bound to end it. And with bounds enforced, a lease with neither an estimate
//! nor a bound counts, for the order in which workers are picked to drain, as ending at its
//! start plus the ceiling ([`Backfill::ceiling_ms`]); its worker's start time stays unknown,
//! so nothing backfills there.

use std::time::Duration;

use js_whole::cost::{DEFAULT_BACKFILL_MARGIN_MS, fits_in_tail};

/// Whether work may be placed on a held worker before the request it is held for starts
/// there ([`crate::Scheduler::with_backfill`]), and the margin kept. The default places
/// none (no bound is known to be enforced) and keeps [`Backfill::DEFAULT_MARGIN`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backfill {
    /// Whether the max wall time of every operation is enforced by the executor.
    bounds_enforced: bool,
    /// The longest max wall time trusted; a longer one is taken as no bound.
    ceiling: Duration,
    /// What backfill keeps spare before a start, and how long a lease at or past its end
    /// is read as still running.
    margin: Duration,
}

impl Default for Backfill {
    fn default() -> Self {
        Self {
            bounds_enforced: false,
            ceiling: Duration::ZERO,
            margin: Self::DEFAULT_MARGIN,
        }
    }
}

impl Backfill {
    /// The margin a setting keeps unless [`Backfill::with_margin`] gives another: one
    /// minute ([`js_whole::cost::DEFAULT_BACKFILL_MARGIN_MS`]).
    pub const DEFAULT_MARGIN: Duration = Duration::from_millis(DEFAULT_BACKFILL_MARGIN_MS);

    /// Backfill on: the executor ends every operation at its max wall time
    /// ([`crate::Scheduler::set_max_wall`]), for a max wall time no longer than `ceiling`,
    /// with [`Backfill::DEFAULT_MARGIN`]. Pass the ceiling the executor enforces (the lease
    /// server: the shorter of `server.max_lease_wall` and the most a job's wall limit may
    /// be), or [`Duration::MAX`] when it enforces none. A max wall time above `ceiling` is
    /// taken as none, so that work never backfills.
    #[must_use]
    pub const fn enforced(ceiling: Duration) -> Self {
        Self {
            bounds_enforced: true,
            ceiling,
            margin: Self::DEFAULT_MARGIN,
        }
    }

    /// This setting with `margin` kept spare: backfilled work ends at least `margin` before
    /// the start of the request its worker is held for, and a lease at or past its end is
    /// read as ending `margin` from now. Bounds stay enforced or not as they were.
    #[must_use]
    pub const fn with_margin(self, margin: Duration) -> Self {
        Self { margin, ..self }
    }

    /// The margin in milliseconds, at most `u64::MAX`.
    pub(crate) fn margin_ms(&self) -> u64 {
        u64::try_from(self.margin.as_millis()).unwrap_or(u64::MAX)
    }

    /// The ceiling in milliseconds (at most `u64::MAX`) when bounds are enforced: no lease
    /// runs longer, so one whose end is not known still ends by its start plus this. `None`
    /// when bounds are not enforced: such a lease's end stays unknown.
    pub(crate) fn ceiling_ms(&self) -> Option<u64> {
        let ceiling = u64::try_from(self.ceiling.as_millis()).unwrap_or(u64::MAX);
        self.bounds_enforced.then_some(ceiling)
    }

    /// What work bounded by `max_wall` and estimated at `p90_minutes` (of its key's wall
    /// minutes, read only when the bound counts) brings to the backfill check: how long it
    /// counts, in milliseconds, the estimate cut to the bound, else the bound (`None`, which
    /// never fits, when bounds are not enforced, the work has no bound, or its bound is above
    /// the ceiling), and this setting's margin.
    pub(crate) fn ask(
        &self,
        max_wall: Option<Duration>,
        p90_minutes: impl FnOnce() -> Option<u32>,
    ) -> Ask {
        Ask {
            duration: self.duration(max_wall, p90_minutes),
            margin_ms: self.margin_ms(),
        }
    }

    /// How long work counts for backfill, as [`Backfill::ask`] says.
    fn duration(
        &self,
        max_wall: Option<Duration>,
        p90_minutes: impl FnOnce() -> Option<u32>,
    ) -> Option<u64> {
        let bound = max_wall.filter(|wall| self.bounds_enforced && *wall <= self.ceiling)?;
        let bound = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        let estimate = p90_minutes().map(|minutes| u64::from(minutes).saturating_mul(60_000));
        Some(estimate.map_or(bound, |estimate| estimate.min(bound)))
    }
}

/// What one request brings to the backfill check ([`Backfill::ask`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ask {
    /// How long it counts, in milliseconds; `None` never fits.
    pub(crate) duration: Option<u64>,
    /// What it keeps spare before a start, in milliseconds.
    pub(crate) margin_ms: u64,
}

impl Ask {
    /// Whether the work may start at `now` on a worker held until `start`: it ends, with
    /// the margin to spare, no later than `start`. Fails closed when either is unknown, for
    /// a start already past, and when the end with the margin is past the end of time.
    pub(crate) fn fits(self, now: u64, start: Option<u64>) -> bool {
        fits_in_tail(now, self.duration, start, self.margin_ms)
    }
}

/// The start time a hold keeps when it is made again on the worker it held last round:
/// the earlier of the two known ones, so work placed since cannot move it later. A start
/// known before and unknown now stays; one unknown before takes the new one.
pub(crate) const fn kept(before: Option<u64>, now: Option<u64>) -> Option<u64> {
    match (before, now) {
        (Some(before), Some(now)) => Some(if now < before { now } else { before }),
        (Some(before), None) => Some(before),
        (None, now) => now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn on(ceiling_s: u64) -> Backfill {
        Backfill::enforced(Duration::from_secs(ceiling_s))
    }

    /// Whether work of `duration` fits at `now` before `start` with the default margin.
    fn fits(now: u64, duration: Option<u64>, start: Option<u64>) -> bool {
        let ask = Ask {
            duration,
            margin_ms: MIN,
        };
        ask.fits(now, start)
    }

    /// Catches: a default margin other than one minute, a margin set on a setting not kept
    /// by it (or kept by the other one), the margin dropped from the ask, and a margin
    /// that changes whether bounds are enforced or the ceiling.
    #[test]
    fn the_margin_is_a_minute_unless_set_and_rides_in_the_ask() {
        assert_eq!(Backfill::DEFAULT_MARGIN, Duration::from_secs(60));
        assert_eq!(Backfill::default().margin_ms(), MIN);
        assert_eq!(on(600).margin_ms(), MIN);
        let wide = on(600).with_margin(Duration::from_secs(5 * 60));
        assert_eq!(wide.margin_ms(), 5 * MIN);
        assert_eq!(on(600).margin_ms(), MIN);
        let wall = Some(Duration::from_secs(600));
        let ask = wide.ask(wall, || None);
        assert_eq!(
            ask,
            Ask {
                duration: Some(10 * MIN),
                margin_ms: 5 * MIN
            }
        );
        let off = Backfill::default().with_margin(Duration::from_secs(1));
        assert_eq!(
            off.ask(wall, || None),
            Ask {
                duration: None,
                margin_ms: 1_000
            }
        );
        assert_eq!(wide.ceiling_ms(), on(600).ceiling_ms());
        assert_eq!(off.ceiling_ms(), None);
        let huge = on(600).with_margin(Duration::MAX);
        assert_eq!(huge.margin_ms(), u64::MAX);
    }

    /// Catches: a ceiling read while bounds are not enforced, a ceiling in seconds read as
    /// milliseconds, and a ceiling past `u64::MAX` milliseconds that wraps.
    #[test]
    fn the_ceiling_counts_only_where_bounds_are_enforced() {
        assert_eq!(Backfill::default().ceiling_ms(), None);
        assert_eq!(on(600).ceiling_ms(), Some(10 * MIN));
        assert_eq!(
            Backfill::enforced(Duration::MAX).ceiling_ms(),
            Some(u64::MAX)
        );
    }

    /// Catches: the margin of the ask not the one the check keeps. Work of a minute at 0
    /// before a start at 3 min fits with a margin of 2 min, and not with one of 2 min and
    /// a millisecond.
    #[test]
    fn the_check_keeps_the_asks_margin() {
        let at = |margin_ms| Ask {
            duration: Some(MIN),
            margin_ms,
        };
        assert!(at(2 * MIN).fits(0, Some(3 * MIN)));
        assert!(!at(2 * MIN + 1).fits(0, Some(3 * MIN)));
    }

    /// Catches: a bound counted while bounds are not enforced (the default), a bound above
    /// the ceiling trusted, the ceiling itself refused (`<` for `<=`), work with no bound
    /// given a duration, and the P90 read for a bound that does not count or for work with
    /// no bound.
    #[test]
    fn a_duration_needs_an_enforced_bound_no_longer_than_the_ceiling() {
        let wall = Some(Duration::from_secs(600));
        let reads = std::cell::Cell::new(0);
        let p90 = || {
            reads.set(reads.get() + 1);
            Some(1)
        };
        assert_eq!(Backfill::default().duration(wall, p90), None);
        assert_eq!(on(599).duration(wall, p90), None);
        assert_eq!(reads.get(), 0);
        assert_eq!(on(600).duration(wall, p90), Some(MIN));
        assert_eq!(reads.get(), 1);
        assert_eq!(Backfill::default().duration(wall, || None), None);
        assert_eq!(Backfill::default().duration(wall, p90), None);
        assert_eq!(on(600).duration(wall, || None), Some(10 * MIN));
        assert_eq!(on(599).duration(wall, || None), None);
        assert_eq!(on(599).duration(wall, p90), None);
        assert_eq!(on(600).duration(None, p90), None);
        assert_eq!(reads.get(), 1);
        let unbounded = Backfill::enforced(Duration::MAX);
        let year = Some(Duration::from_secs(365 * 86_400));
        assert_eq!(unbounded.duration(year, || None), Some(365 * 86_400_000));
    }

    /// Catches: the estimate trusted past the bound, the bound used when a shorter estimate
    /// is known, and minutes read as seconds.
    #[test]
    fn the_p90_is_cut_to_the_bound() {
        let wall = Some(Duration::from_secs(600));
        assert_eq!(on(3_600).duration(wall, || Some(3)), Some(3 * MIN));
        assert_eq!(on(3_600).duration(wall, || Some(10)), Some(10 * MIN));
        assert_eq!(on(3_600).duration(wall, || Some(11)), Some(10 * MIN));
        assert_eq!(on(3_600).duration(wall, || Some(u32::MAX)), Some(10 * MIN));
    }

    /// Catches: the margin left out or added twice, `<` for `<=`, and a past start or an
    /// unknown one admitting work.
    #[test]
    fn work_fits_up_to_the_start_less_the_margin() {
        let now = 1_000;
        assert!(fits(now, Some(MIN), Some(now + 2 * MIN)));
        assert!(!fits(now, Some(MIN + 1), Some(now + 2 * MIN)));
        assert!(!fits(now, Some(0), Some(now)));
        assert!(!fits(now, Some(0), Some(now - 1)));
        assert!(!fits(now, None, Some(u64::MAX)));
        assert!(!fits(now, Some(1), None));
        // A start at the end of time: 30 s left is less than the margin, and a start
        // equal to now leaves nothing.
        assert!(!fits(u64::MAX - 30_000, Some(1), Some(u64::MAX)));
        assert!(!fits(u64::MAX, Some(0), Some(u64::MAX)));
        assert!(fits(u64::MAX - MIN - 1, Some(1), Some(u64::MAX)));
    }

    /// Catches: a hold made again taking a later start (work placed since would extend
    /// it), losing a known start to an unknown one, or keeping an unknown one when a start
    /// is known now.
    #[test]
    fn a_kept_hold_never_starts_later() {
        assert_eq!(kept(Some(500), Some(900)), Some(500));
        assert_eq!(kept(Some(500), Some(300)), Some(300));
        assert_eq!(kept(Some(500), None), Some(500));
        assert_eq!(kept(None, Some(300)), Some(300));
        assert_eq!(kept(None, None), None);
    }
}
