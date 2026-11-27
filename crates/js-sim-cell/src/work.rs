//! One unit of client work: a run of [`crate::SIM_RUN_IMAGE`] that lasts a fixed time,
//! the resources it asks for, whether it wants a whole host,
//! and how long the client waits for it before it gives up.
//!
//! A work may also ask for a retry policy ([`Work::retry`]) and a completion callback
//! ([`Work::callback`]); a job front sends them with the job, and a work without them is
//! submitted exactly as it was before they existed.
//!
//! A [`Work`] says nothing of how it reaches the server: a [`crate::front::Front`] turns
//! it into calls. The cell numbers the items it submits from zero, in submission order
//! ([`crate::ItemId`]), and records what each asked for ([`Ask`]) when it is submitted,
//! so no check takes the server's word for it.

use std::time::Duration;

use js_types::Resources;
use js_types::sched::FarmTime;

use crate::observe::Ask;

/// How long an item is waited for unless [`Work::give_up_after`] says otherwise: one
/// hour.
pub const DEFAULT_GIVE_UP: Duration = Duration::from_secs(3600);

/// What an item needs unless [`Work::needs`] says otherwise: one core, 1 KiB of memory and
/// one `nvidia-a100` GPU (the job front asks for the cpu and memory only).
#[must_use]
pub fn default_needs() -> Resources {
    // One GPU with its class: `Resources::new` refuses only a count without a class or a
    // class without a count, so the default is never taken.
    Resources::new(1_000, 1_024, 1, Some("nvidia-a100".to_owned())).unwrap_or_default()
}

/// The retry policy a work asks for: how many attempts, and optionally the waits between
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// The most attempts, the first included.
    pub max_attempts: u32,
    /// The wait before the second attempt; `None` leaves the server's default.
    pub initial_backoff: Option<Duration>,
    /// The longest wait; `None` leaves the server's default.
    pub max_backoff: Option<Duration>,
}

/// The completion callback a work asks for: the receiver is the cell's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Callback {
    /// How long the server keeps trying after the job's end; `None` leaves the server's
    /// default.
    pub deadline: Option<Duration>,
}

/// One unit of client work: a run that lasts a fixed time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    runs_for: Duration,
    give_up_after: Duration,
    needs: Resources,
    whole_host: bool,
    retry: Option<Retry>,
    callback: Option<Callback>,
}

impl Work {
    /// A run that ends by itself `duration` after its launch, asking for
    /// [`default_needs`], given up after [`DEFAULT_GIVE_UP`].
    #[must_use]
    pub fn run_for(duration: Duration) -> Self {
        Self {
            runs_for: duration,
            give_up_after: DEFAULT_GIVE_UP,
            needs: default_needs(),
            whole_host: false,
            retry: None,
            callback: None,
        }
    }

    /// A run that ends by itself `duration` after its launch on a host it holds whole:
    /// the client submits a job that asks for a whole host. Otherwise as
    /// [`Work::run_for`].
    #[must_use]
    pub fn whole_host(duration: Duration) -> Self {
        Self {
            whole_host: true,
            ..Self::run_for(duration)
        }
    }

    /// This work, given up `bound` after it is submitted: the client reports
    /// [`crate::HowEnded::GaveUp`] unless the item ended before.
    #[must_use]
    pub fn give_up_after(mut self, bound: Duration) -> Self {
        self.give_up_after = bound;
        self
    }

    /// This work, asking for `resources` in place of [`default_needs`].
    #[must_use]
    pub fn needs(mut self, resources: Resources) -> Self {
        self.needs = resources;
        self
    }

    /// This work, asking for at most `max_attempts` attempts with the server's default
    /// waits between them.
    #[must_use]
    pub fn retry(self, max_attempts: u32) -> Self {
        self.retry_with_backoff(max_attempts, None, None)
    }

    /// This work, asking for at most `max_attempts` attempts, the wait before the second
    /// `initial_backoff` and the longest wait `max_backoff` (each, when given, in place of
    /// the server's default).
    #[must_use]
    pub fn retry_with_backoff(
        mut self,
        max_attempts: u32,
        initial_backoff: Option<Duration>,
        max_backoff: Option<Duration>,
    ) -> Self {
        self.retry = Some(Retry {
            max_attempts,
            initial_backoff,
            max_backoff,
        });
        self
    }

    /// This work, asking for a completion callback to the cell's receiver, tried for the
    /// server's default time after the job's end.
    #[must_use]
    pub fn callback(mut self) -> Self {
        self.callback = Some(Callback { deadline: None });
        self
    }

    /// This work, asking for a completion callback to the cell's receiver, tried for
    /// `deadline` after the job's end.
    #[must_use]
    pub fn callback_for(mut self, deadline: Duration) -> Self {
        self.callback = Some(Callback {
            deadline: Some(deadline),
        });
        self
    }

    /// The retry policy it asks for, if any.
    #[must_use]
    pub fn retry_policy(&self) -> Option<&Retry> {
        self.retry.as_ref()
    }

    /// The completion callback it asks for, if any.
    #[must_use]
    pub fn callback_spec(&self) -> Option<&Callback> {
        self.callback.as_ref()
    }

    /// How long the run lasts once launched.
    #[must_use]
    pub fn runs_for(&self) -> Duration {
        self.runs_for
    }

    /// How long after its submission the item is given up.
    #[must_use]
    pub fn gives_up_after(&self) -> Duration {
        self.give_up_after
    }

    /// The resources it asks for.
    #[must_use]
    pub fn resources(&self) -> &Resources {
        &self.needs
    }

    /// Whether it wants a whole host.
    #[must_use]
    pub fn wants_whole_host(&self) -> bool {
        self.whole_host
    }

    /// What an item of this work submitted at `now` asks for.
    #[must_use]
    pub fn ask(&self, now: FarmTime) -> Ask {
        Ask {
            whole_host: self.whole_host,
            resources: self.needs.clone(),
            gives_up_at: now.saturating_add(self.give_up_after),
        }
    }
}
