//! The end of a machine's reserved block of time, as the provider's listing states it.

use js_elastic::Held;

use crate::record::Record;
use crate::time::Millis;

/// The end of the block `held` runs in, or `None` when the machine is not in a block or the
/// provider did not state the end. Saturates, as [`Millis`] does.
pub(crate) fn end_of(held: &Held) -> Option<Millis> {
    held.ends_unix_seconds
        .map(|seconds| Millis::new(seconds.saturating_mul(1000)))
}

impl Record {
    /// Whether the machine is in a block that has not ended at `now`. The block is paid
    /// for whether the machine is used or not, so such a machine is never released as idle
    /// and never retired at `max_age`.
    pub(crate) fn block_running(&self, now: Millis) -> bool {
        self.ends_at.is_some_and(|end| now < end)
    }

    /// Whether the machine is in a block that has ended at `now` (the end itself counts).
    /// The cloud takes such a machine back as planned, so its going is no loss.
    pub(crate) fn block_ended(&self, now: Millis) -> bool {
        self.ends_at.is_some_and(|end| now >= end)
    }
}
