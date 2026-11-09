//! Holding the platform matches of a placement round for the rounds after it.
//!
//! A placement round matches each platform request and lease kind it looks at against
//! every live worker's capabilities, once per round ([`crate::servable`]). A caller that runs
//! many rounds over the same workers, such as the placer, which runs one round per queued job
//! of a pass, holds the matches for those rounds instead, so each worker is matched once per
//! request and kind for all of them. What is placed is the same either way: the scheduler
//! forgets the kept matches when it writes a worker's capabilities, and a round takes them
//! only over the same live workers ([`crate::servable::Matches`]).

use super::Scheduler;
use crate::servable::Matches;

impl Scheduler {
    /// From now on, until [`Scheduler::drop_matches`], keeps the platform matches each
    /// placement round makes for the rounds after it: a later round over the same live
    /// workers, with no worker's capabilities written since (by [`crate::Event::WorkerUp`] or
    /// [`crate::Event::Capacity`]), matches only the requests no earlier round matched. The
    /// placements are the same as without it.
    pub const fn hold_matches(&mut self) {
        self.hold_matches = true;
    }

    /// Stops keeping the platform matches between rounds and forgets those kept: each round
    /// matches afresh, as before [`Scheduler::hold_matches`].
    pub fn drop_matches(&mut self) {
        self.hold_matches = false;
        self.matches.forget();
    }

    /// Takes `matches`, what a round left, for the next round if they are held, else
    /// forgets them.
    pub(super) fn keep_matches(&mut self, matches: Matches) {
        self.matches = matches;
        if !self.hold_matches {
            self.matches.forget();
        }
    }

    /// How many times a placement round matched a live worker against a platform request and
    /// lease kind, since the scheduler was made.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn match_evaluations(&self) -> u64 {
        self.matches.evaluations()
    }
}

#[cfg(test)]
#[path = "matches_tests.rs"]
mod tests;
