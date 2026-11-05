//! One unit of client work: a run of [`crate::SIM_RUN_IMAGE`] that lasts a fixed time,
//! the resources it asks for, whether it wants a whole host, and how long the client
//! waits for it before it gives up.
//!
//! A [`Work`] says nothing of how it reaches the server: a [`crate::front::Front`] turns
//! it into calls. The cell numbers the items it submits from zero, in submission order
//! ([`crate::ItemId`]), and records what each asked for ([`Ask`]) when it is submitted,
//! so no check takes the server's word for it.

use std::time::Duration;

use js_types::Resources;
use js_types::sched::FarmTime;

use crate::observe::Ask;

/// How long an item is waited for unless [`Work::give_up_after`] says otherwise: the
/// server's largest lease ttl (`--max-ttl`, 3600 s), so a lease asked for that long is
/// not cut short.
pub const DEFAULT_GIVE_UP: Duration = Duration::from_secs(3600);

/// What an item needs unless [`Work::needs`] says otherwise: the share the server's own
/// tests lease (`lease_body`): one core, 1 KiB of memory and one `nvidia-a100` GPU.
#[must_use]
pub fn default_needs() -> Resources {
    // One GPU with its class: `Resources::new` refuses only a count without a class or a
    // class without a count, so the default is never taken.
    Resources::new(1_000, 1_024, 1, Some("nvidia-a100".to_owned())).unwrap_or_default()
}

/// One unit of client work: a run that lasts a fixed time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    runs_for: Duration,
    give_up_after: Duration,
    needs: Resources,
    whole_host: bool,
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
        }
    }

    /// A run that ends by itself `duration` after its launch on a host it holds whole:
    /// the client asks for a whole-node lease under a request id, waits until the lease
    /// is held (after the host's agent reported ready for it), runs, and releases.
    /// Otherwise as [`Work::run_for`].
    #[must_use]
    pub fn whole_host(duration: Duration) -> Self {
        Self {
            whole_host: true,
            ..Self::run_for(duration)
        }
    }

    /// This work, given up `bound` after it is submitted: the client releases what it
    /// holds and reports [`crate::HowEnded::GaveUp`] unless the item ended before.
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
