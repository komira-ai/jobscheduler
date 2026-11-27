//! `--backfill-bounds-enforced` and `server.backfill_bounds_enforced`, `--backfill-margin` and
//! `server.backfill_margin`: whether the placement engine trusts a job's wall limit for
//! backfill, and the margin it keeps (design komira-ai/jobscheduler#582, step 0).
//!
//! The job service ends every job at its wall limit, so a job's wall limit is enforced: the
//! engine trusts it unless the operator says otherwise. With it trusted, a host set aside for
//! a whole-host job takes a job that ends, with the margin to spare, before the host's tail;
//! and a job on a host whose end the engine does not know (an allocation taken up after a
//! restart, whose wall limit the engine was not told) counts, for the pick of a host to
//! drain, as ending at its start plus the ceiling: the shorter of `server.max_lease_wall` and
//! the most a job's wall limit may be (`--max-job-wall`, `server.max_job_wall`). Without it
//! nothing backfills and such a host ranks after every host whose tail is known. Either way
//! a job past its end counts as ending the margin from now.

use std::time::Duration;

use clap::Args;
use js_sched::Backfill;

use super::{Config, parse_duration};
use crate::job_api::submit::DEFAULT_MAX_JOB_WALL;

/// The backfill flags of the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Args)]
pub struct BackfillFlags {
    /// Whether the placement engine trusts a job's wall limit as its end, `true` or `false`:
    /// with `true` (the default: the job service ends every job at its wall limit) a host
    /// set aside for a whole-host job takes a job that ends, with the margin to spare, before
    /// the host is free, and a job whose end the engine does not know counts as ending at its
    /// start plus the ceiling when it picks a host to drain; with `false` nothing backfills.
    /// Needs `--agent-pool-config`.
    #[arg(long)]
    pub backfill_bounds_enforced: Option<bool>,
    /// What a backfilled job keeps spare before the host it runs on is free, and how long a
    /// job past its end is taken to hold its host from now, as a duration such as `90s` or
    /// `2m`; one minute unless given. Needs `--agent-pool-config`.
    #[arg(long, value_parser = parse_duration)]
    pub backfill_margin: Option<Duration>,
}

/// What the configuration says of backfill: the two settings, each its default when not
/// given, and the two ceilings that bound a job's wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackfillSetting {
    /// `--backfill-bounds-enforced`, `server.backfill_bounds_enforced`; true unless given.
    pub(crate) bounds_enforced: bool,
    /// `--backfill-margin`, `server.backfill_margin`; [`Backfill::DEFAULT_MARGIN`] unless
    /// given.
    pub(crate) margin: Duration,
    /// `server.max_lease_wall`, when the file sets it.
    pub(crate) max_lease_wall: Option<Duration>,
    /// `--max-job-wall`, `server.max_job_wall`, or [`DEFAULT_MAX_JOB_WALL`].
    pub(crate) max_job_wall: Duration,
}

impl Default for BackfillSetting {
    fn default() -> Self {
        Self::of((None, None), None, DEFAULT_MAX_JOB_WALL)
    }
}

impl BackfillSetting {
    /// The setting of `given` (the switch and the margin, each when given), `max_lease_wall`
    /// and `max_job_wall`.
    pub(crate) fn of(
        given: (Option<bool>, Option<Duration>),
        max_lease_wall: Option<Duration>,
        max_job_wall: Duration,
    ) -> Self {
        let (bounds_enforced, margin) = given;
        Self {
            bounds_enforced: bounds_enforced.unwrap_or(true),
            margin: margin.unwrap_or(Backfill::DEFAULT_MARGIN),
            max_lease_wall,
            max_job_wall,
        }
    }

    /// The setting of the flags `flags`, with no `server.max_lease_wall` (the file alone
    /// sets it) and `max_job_wall`.
    pub(crate) fn of_flags(flags: BackfillFlags, max_job_wall: Duration) -> Self {
        let given = (flags.backfill_bounds_enforced, flags.backfill_margin);
        Self::of(given, None, max_job_wall)
    }

    /// The engine's setting: bounds enforced up to the ceiling (the shorter of
    /// `max_lease_wall` and `max_job_wall`, no job running longer than either), or not
    /// enforced, with the margin either way.
    pub(crate) fn engine(self) -> Backfill {
        let ceiling = self
            .max_lease_wall
            .map_or(self.max_job_wall, |wall| wall.min(self.max_job_wall));
        let backfill = if self.bounds_enforced {
            Backfill::enforced(ceiling)
        } else {
            Backfill::default()
        };
        backfill.with_margin(self.margin)
    }
}

impl Config {
    /// The placement engine's backfill setting (`--backfill-bounds-enforced` and
    /// `--backfill-margin`, or `server.backfill_bounds_enforced` and `server.backfill_margin`,
    /// with the ceiling of `server.max_lease_wall` and the most a job's wall limit may be):
    /// bounds enforced with a one-minute margin unless the configuration says otherwise.
    pub fn backfill(&self) -> Backfill {
        self.agents.backfill.engine()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3_600);

    /// Asserts each answer of the engine's setting: by default bounds are enforced up to the
    /// most a job's wall limit may be with a minute's margin; a shorter `max_lease_wall`
    /// lowers the ceiling and a longer one does not raise it; a margin given is kept with
    /// either switch; and the switch off enforces nothing. Catches a default switched off, a
    /// ceiling taken from the longer of the two walls, a margin dropped on the way, and the
    /// switch ignored.
    #[test]
    fn the_engine_setting_follows_the_switch_the_margin_and_the_shorter_wall() {
        let day = 24 * HOUR;
        let plain = BackfillSetting::of((None, None), None, day);
        assert_eq!(plain.engine(), Backfill::enforced(day));
        let shorter = BackfillSetting::of((None, None), Some(HOUR), day);
        assert_eq!(shorter.engine(), Backfill::enforced(HOUR));
        let longer = BackfillSetting::of((None, None), Some(2 * day), day);
        assert_eq!(longer.engine(), Backfill::enforced(day));
        let margin = Duration::from_secs(90);
        let wide = BackfillSetting::of((Some(true), Some(margin)), Some(HOUR), day);
        assert_eq!(wide.engine(), Backfill::enforced(HOUR).with_margin(margin));
        let off = BackfillSetting::of((Some(false), Some(margin)), Some(HOUR), day);
        assert_eq!(off.engine(), Backfill::default().with_margin(margin));
        let off = BackfillSetting::of((Some(false), None), None, day);
        assert_eq!(off.engine(), Backfill::default());
    }

    /// Asserts that the default is the switch on, a minute's margin, no `max_lease_wall` and
    /// the job service's own most wall limit, and that the flags carry both values and no
    /// `max_lease_wall`. Catches a default that turns backfill off, and a flag value lost.
    #[test]
    fn the_default_and_the_flags() {
        let made = BackfillSetting::default();
        assert_eq!(
            made,
            BackfillSetting {
                bounds_enforced: true,
                margin: Duration::from_secs(60),
                max_lease_wall: None,
                max_job_wall: DEFAULT_MAX_JOB_WALL,
            }
        );
        let flags = BackfillFlags {
            backfill_bounds_enforced: Some(false),
            backfill_margin: Some(Duration::from_secs(5)),
        };
        let made = BackfillSetting::of_flags(flags, HOUR);
        assert_eq!(
            made,
            BackfillSetting {
                bounds_enforced: false,
                margin: Duration::from_secs(5),
                max_lease_wall: None,
                max_job_wall: HOUR,
            }
        );
    }
}
