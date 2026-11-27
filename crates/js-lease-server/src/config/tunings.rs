//! `--score-balance`, `--score-slack-tolerance-ppm` and `--key-share-percent`, with the
//! keys `server.score_balance`, `server.score_slack_tolerance_ppm` and
//! `server.key_share_percent`: the three scoring settings of js-sched's placement engine,
//! which the operator tunes from measurements. Each is a whole number in the unit its name
//! says, and each left out is js-sched's own default ([`LoadConfig::default`]), which is not
//! written again here, so the two cannot drift. The score ranks the hosts a job fits on; it
//! admits nothing, so no value here lets a host be charged past its CPU slack.
//!
//! The other fields of js-sched's `LoadConfig` stay at js-sched's defaults and have no
//! setting: see "Where the code differs" in `docs/design/placement-by-load.md`.

use std::ops::RangeInclusive;

use clap::builder::RangedI64ValueParser;
use js_sched::{LoadConfig, MAX_BALANCE};

/// What `--score-balance` accepts: js-sched's range, 0 to [`MAX_BALANCE`].
pub(crate) const BALANCE: RangeInclusive<u32> = 0..=MAX_BALANCE;

/// What `--score-slack-tolerance-ppm` accepts: none of a host's capacity to all of it.
pub(crate) const SLACK_TOLERANCE_PPM: RangeInclusive<u32> = 0..=1_000_000;

/// What `--key-share-percent` accepts: none of a host's CPU to all of it.
pub(crate) const KEY_SHARE_PERCENT: RangeInclusive<u32> = 0..=100;

/// The scoring settings of the placement engine, each the value the flag (or the file's key)
/// gave, or `None` for js-sched's default. A value outside its range is refused at start,
/// never clamped, so a typo is not run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::Args)]
pub struct Tunings {
    /// How much of the other resource's leftover the placement score's fit counts beside the
    /// binding one, from 0 to 4. Without the flag the engine's own default is used, which is
    /// 1. Needs `--agent-pool-config`.
    #[arg(long, value_parser = whole(BALANCE))]
    pub score_balance: Option<u32>,
    /// How far a host may be charged above its capacity, in millionths of the capacity from 0
    /// to 1000000, before the placement score counts the excess against it. Without the flag
    /// the engine's own default is used, which is 10000 (1%). Needs `--agent-pool-config`.
    #[arg(long, value_parser = whole(SLACK_TOLERANCE_PPM))]
    pub score_slack_tolerance_ppm: Option<u32>,
    /// The share of a host's CPU, in percent from 0 to 100, that the leases of one action key
    /// may take before the placement score prefers another host. Each job is its own key (its
    /// request is named by its job id), so for jobs it is the share one job may take. Without
    /// the flag the engine's own default is used, which is 25. Needs `--agent-pool-config`.
    #[arg(long, value_parser = whole(KEY_SHARE_PERCENT))]
    pub key_share_percent: Option<u32>,
}

impl Tunings {
    /// `load` with each value given here in place of its own, and every other setting as it
    /// was.
    pub(crate) fn apply(self, load: LoadConfig) -> LoadConfig {
        LoadConfig {
            score_balance: self.score_balance.unwrap_or(load.score_balance),
            score_slack_tol_ppm: self
                .score_slack_tolerance_ppm
                .map_or(load.score_slack_tol_ppm, u64::from),
            key_share_percent: self.key_share_percent.unwrap_or(load.key_share_percent),
            ..load
        }
    }
}

/// The flag's parse of a whole number within `range`; any other text is refused.
fn whole(range: RangeInclusive<u32>) -> RangedI64ValueParser<u32> {
    clap::value_parser!(u32).range(i64::from(*range.start())..=i64::from(*range.end()))
}

#[cfg(test)]
#[path = "tunings_tests.rs"]
mod tests;
