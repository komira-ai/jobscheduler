//! What a call of the fake cloud is scripted to do.

use std::collections::VecDeque;
use std::time::Duration;

use js_elastic::RefusalCode;

/// How long the fake asks a caller to wait when it answers a 429.
pub const THROTTLE_WAIT: Duration = Duration::from_secs(30);

/// One of the calls the fake counts. Only [`Call::Acquire`], [`Call::List`] and
/// [`Call::Release`] can be scripted; the others are only counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Call {
    /// `catalog`.
    Catalog,
    /// `checks`.
    Checks,
    /// `host_prep`.
    HostPrep,
    /// `payload_limit`.
    PayloadLimit,
    /// `list`.
    List,
    /// `acquire`: the call that makes a machine, the purchase.
    Acquire,
    /// `release`.
    Release,
    /// `unresolved`.
    Unresolved,
    /// `is_gone`.
    IsGone,
}

/// What one call does, as a script gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
    /// The call works as the cloud's state says it should (limits, lag and all).
    #[default]
    Pass,
    /// An acquire is refused with this code and makes nothing. A list fails. A release is
    /// refused and ends nothing.
    Refuse(RefusalCode),
    /// The caller gets no answer. `landed` says whether the call took effect all the same:
    /// an acquire makes its machine, a release ends it. A list fails either way.
    Lost {
        /// Whether the effect happened.
        landed: bool,
    },
    /// The cloud answers 429. Nothing happens: an acquire is answered with a deferral of
    /// [`THROTTLE_WAIT`], a list fails, a release is not answered.
    Throttled,
}

/// The steps queued for one call, then the step it repeats once they are used up.
#[derive(Debug, Default)]
pub(crate) struct Script {
    queue: VecDeque<Step>,
    rest: Step,
}

impl Script {
    /// The next step: the oldest queued, else the one repeated forever.
    pub(crate) fn take(&mut self) -> Step {
        self.queue.pop_front().unwrap_or(self.rest)
    }

    /// Queues `steps` after the ones already queued.
    pub(crate) fn queue(&mut self, steps: &[Step]) {
        self.queue.extend(steps.iter().copied());
    }

    /// Sets the step repeated once the queue is empty.
    pub(crate) fn repeat(&mut self, step: Step) {
        self.rest = step;
    }
}
