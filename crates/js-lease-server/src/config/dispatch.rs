//! The timings of the dispatch poll ([`crate::agent_api`], `POST /v1/agent/dispatch`): how long
//! a server's claim of a message holds it, and how often a held poll reads the queue again.
//! Both are keys of the configuration file's `[server]` table, `dispatch_lease_secs` and
//! `dispatch_recheck_ms` ([`crate::file`]); a server started on flags has the defaults.

use std::time::Duration;

use crate::held_beat::MAX_HOLD;

/// `dispatch_lease_secs` unless the file gives it: how long a claimed message is held for its
/// agent to act on it and acknowledge it before another poll hands it out again.
pub(crate) const DEFAULT_LEASE_SECS: u64 = 10;

/// The most `dispatch_lease_secs` the file may give: an hour, past which a message lost with
/// a poll that was never answered would wait too long to be handed out again.
pub(crate) const MOST_LEASE_SECS: u64 = 3600;

/// `dispatch_recheck_ms` unless the file gives it: how often a held poll reads its host's
/// queue again whatever woke it, so a message another server wrote whose notice did not reach
/// this server ([`crate::listening`]: a notice is best effort) reaches the poll it holds within
/// it.
pub(crate) const DEFAULT_RECHECK_MS: u64 = 500;

/// The most `dispatch_recheck_ms` the file may give: below the hold ([`MAX_HOLD`], 5000 ms),
/// or the recheck would never run.
pub(crate) const MOST_RECHECK_MS: u64 = 4_999;

/// What a dispatch poll is timed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchTimes {
    /// `dispatch_lease_secs`: how long a claim holds a message.
    pub(crate) lease: Duration,
    /// `dispatch_recheck_ms`: how often a held poll reads the queue again.
    pub(crate) recheck: Duration,
    /// How long a poll with nothing to hand out is held: [`MAX_HOLD`], well inside the agent's
    /// ten-second request limit. Not a key; a test sets it to end a hold at once.
    pub(crate) hold: Duration,
}

impl Default for DispatchTimes {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(DEFAULT_LEASE_SECS),
            recheck: Duration::from_millis(DEFAULT_RECHECK_MS),
            hold: MAX_HOLD,
        }
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
