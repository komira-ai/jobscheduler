//! The operator's cordons, as the cell records them for [`crate::Invariant::CordonNeverKillsRunningJob`].
//!
//! A cordon takes a host out of placement for a time: the server places no new job on it
//! while the cordon holds, and goes on with the jobs already there. The cell cordons a host
//! as an operator does, through the job service's route (`POST /v1/hosts/{id}:cordon`, and
//! `:uncordon`; [`crate::Cell::cordon_host`]), and writes down each call the server answered
//! 200 for, in the order made ([`Observations::cordons`], [`Observations::uncordons`]).
//!
//! A cordon is a window of virtual time on one host: from the call to the end of its length
//! ([`Cordon::until`]), or to the first uncordon of the host at or after the call, whichever
//! is first. A later cordon of the same host renews it (the server keeps the later end and
//! never shortens), so a host's cordon is the union of its windows, and a host cordoned
//! again after one lapsed has two.

use js_types::HostId;
use js_types::sched::FarmTime;

use crate::observe::Observations;

/// A cordon the server answered 200: `host` was cordoned at `at` until `until`, unless a
/// renewal or an uncordon changed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cordon {
    /// The host.
    pub host: HostId,
    /// When the operator's call was made.
    pub at: FarmTime,
    /// When the cordon of that call ends, on the cell's virtual clock.
    pub until: FarmTime,
}

/// An uncordon the server answered 200: the cordon of `host` was lifted at `at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uncordon {
    /// The host.
    pub host: HostId,
    /// When the operator's call was made.
    pub at: FarmTime,
}

/// A window in which one host is cordoned: from `from` until `to`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) host: HostId,
    pub(crate) from: FarmTime,
    pub(crate) to: FarmTime,
}

impl Observations {
    /// These observations with `host` cordoned by a call at `at`, until `until`, after the
    /// cordons before.
    #[must_use]
    pub fn with_cordon(mut self, host: HostId, at: FarmTime, until: FarmTime) -> Self {
        self.cordons.push(Cordon { host, at, until });
        self
    }

    /// These observations with the cordon of `host` lifted by a call at `at`, after the
    /// calls before.
    #[must_use]
    pub fn with_uncordon(mut self, host: HostId, at: FarmTime) -> Self {
        self.uncordons.push(Uncordon { host, at });
        self
    }

    /// The window of each cordon: it ends at its `until`, or at the first uncordon of its
    /// host at or after the call (an uncordon in the same millisecond as the cordon lifts
    /// it).
    pub(crate) fn windows(&self) -> Vec<Window> {
        let window = |cordon: &Cordon| {
            let lifts = self.uncordons.iter();
            let lifted = lifts.filter(|lift| (lift.host == cordon.host) & (lift.at >= cordon.at));
            let first = lifted.map(|lift| lift.at).min();
            Window {
                host: cordon.host.clone(),
                from: cordon.at,
                to: first.map_or(cordon.until, |lift| lift.min(cordon.until)),
            }
        };
        self.cordons.iter().map(window).collect()
    }
}
