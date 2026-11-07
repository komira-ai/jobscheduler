//! What is remembered of a whole-node request between its placements: how many prepares
//! failed, the hosts it must not be placed on, and the last failure ([`RequestRecord`]).
//! The placer keeps these records (the engine of `js-sched-placer`) for every whole-node
//! request, those told apart by their content included, through
//! [`crate::Placer::prepare_failed`], [`crate::Placer::whole_commit`] and their siblings,
//! and hands its counts and bars to the reservation ledger and takes them back at a start
//! ([`crate::Placer::whole_ledger`], [`crate::Placer::whole_restore`]).
//!
//! A request is told apart by its id when its start carried one
//! ([`js_provider::StartSpec::with_request_id`]). A start without an id falls back to the
//! content (the whole [`Request`]): two equal requests share one record ([`RequestKey`]).
//!
//! # How long a record lives
//!
//! - It is removed when the agent reports a lease of the request ready.
//! - A request that used up `max_replace` is *terminal*: its next start is refused
//!   `ExclusiveNotReady` (wire `exclusive-not-ready`), and that refusal removes the
//!   record and its exclusions, so the start after it is a fresh request.
//!   [`Registry::not_ready`] holds the reason until the refusal has been delivered.
//! - A record with no committed lease whose request has not been started for
//!   `Rules::idle_horizon_ms` is dropped, terminal or not, with its exclusions
//!   (`ForLife` ones included): the consumer is gone, and the records stay bounded by the
//!   requests in flight rather than by every request that ever failed.
//!
//! No request gives a host up, so the give-up count is always zero: the ledger writes zero,
//! and a count an older file holds is read and ignored. The failed prepares the allocation
//! log records are the registry's ([`FailedPrepare`]).
//!
//! [`Registry::not_ready`]: crate::Registry::not_ready

use std::collections::BTreeMap;

use js_agent_wire::RefusalReason;
use js_lease::Timestamp;
use js_provider::StartSpec;
use js_types::{HostId, LeaseId, Request, RequestId};
use js_usage::ProfileKey;

use crate::config::PoolConfig;
use crate::whole_config::WholeNodeConfig;

use super::reasons::{Cause, HostEffect};

/// What a request's record is kept under: the consumer's request id when the start
/// carried one, else the content of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKey {
    /// The id the consumer gave the request.
    Id(RequestId),
    /// No id was given: two equal requests are one.
    Content(Request),
}

impl RequestKey {
    /// The key of the request `spec` starts.
    pub(crate) fn of(spec: &StartSpec) -> Self {
        match spec.request_id() {
            Some(id) => Self::Id(id.clone()),
            None => Self::Content(spec.request().clone()),
        }
    }
}

impl From<&Request> for RequestKey {
    fn from(request: &Request) -> Self {
        Self::Content(request.clone())
    }
}

impl From<&RequestId> for RequestKey {
    fn from(id: &RequestId) -> Self {
        Self::Id(id.clone())
    }
}

/// Until when a host is left out of a request's placements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// Left out while the clock reads before this instant.
    Until(Timestamp),
    /// Left out for the rest of the request's life.
    ForLife,
}

impl Exclusion {
    /// Whether the host is still left out at `now`.
    pub(crate) fn holds(&self, now: Timestamp) -> bool {
        match self {
            Self::Until(end) => now < *end,
            Self::ForLife => true,
        }
    }
}

/// The last prepare failure of a request: the code and the detail as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReady {
    /// The reason code (`prepare-timeout` for a deadline that passed).
    pub code: String,
    /// The text the agent sent, kept as received, never parsed; empty for a timeout.
    pub detail: String,
}

/// One request's record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestRecord {
    /// Prepares that failed, by a refusal or a timeout.
    pub prepare_failures: u32,
    /// Deprecated, always zero: a reservation is never given up. Kept so that the
    /// records and the reservation ledger keep their shape.
    pub give_ups: u32,
    /// The hosts left out of the request's placements, and until when.
    pub exclusions: BTreeMap<HostId, Exclusion>,
    /// The last prepare failure, if there was one.
    pub last: Option<NotReady>,
    /// Whether the request has used up `max_replace`. Its next start is refused
    /// `ExclusiveNotReady` (see the module documentation) and removes the record.
    pub terminal: bool,
}

/// A committed whole-node lease whose prepare failed: the booking it freed, kept by the
/// registry for the record of its end until the lock is released (it is the allocation
/// log's, not a request's record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedPrepare {
    pub(crate) lease: LeaseId,
    pub(crate) host: HostId,
    pub(crate) at: Timestamp,
    /// The first profile name a run of the lease was queued with, if one was.
    pub(crate) profile: Option<ProfileKey>,
}

/// How long the row of `cause` leaves the failing host out of the request's placements, a
/// back-off ending at `backoff_end` or the request's life; `None` for a row that taints the
/// host instead.
pub(crate) fn exclusion(cause: Cause, backoff_end: Timestamp) -> Option<Exclusion> {
    match cause.row().host {
        HostEffect::ExcludeForBackoff => Some(Exclusion::Until(backoff_end)),
        HostEffect::ExcludeForLife => Some(Exclusion::ForLife),
        HostEffect::Taint => None,
    }
}

/// The exclusion the table of reasons gives the host of a failed prepare, for the placement
/// engine to apply to a whole-host job: `refused` is the code the agent refused the prepare
/// with, or `None` when no ready report came by the deadline (which acts as `setup-failed`).
/// The answer is the same as the one a whole-node lease gets: a back-off ending at
/// `backoff_end` ([`Exclusion::Until`]) for a reason that leaves the host out for a while,
/// [`Exclusion::ForLife`] for one that leaves it out for good, and `None` for one that
/// taints the host (`host-unsafe`), which [`crate::Registry::taint_after_prepare`] records.
#[must_use]
pub fn prepare_exclusion(
    refused: Option<RefusalReason>,
    backoff_end: Timestamp,
) -> Option<Exclusion> {
    exclusion(refused.map_or(Cause::Timeout, Cause::Refused), backoff_end)
}

/// The settings a failed prepare reads, from the pool's whole-node settings or, for a
/// pool without any, the defaults of a dedicated pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rules {
    pub(crate) regain_grace_ms: u64,
    pub(crate) prepare_timeout_ms: u64,
    pub(crate) reserve_backoff_ms: u64,
    pub(crate) max_replace: u32,
}

/// The whole-node settings of `config`, or the defaults of a dedicated pool for a pool
/// without any.
pub(crate) fn whole_settings(config: &PoolConfig) -> WholeNodeConfig {
    let whole = config.whole_node().cloned();
    whole.unwrap_or_else(|| WholeNodeConfig::dedicated(config.max_machines()))
}

impl Rules {
    /// How long a request may go without a start before its record is dropped:
    /// `reserve_backoff` for each of the `max_replace + 1` tries, so a record outlives
    /// every exclusion it holds and the tries it can still make, but at least one
    /// `prepare_timeout` (the settings are non-zero, and a short back-off must not make
    /// the registry forget a failure the moment the lease ends).
    pub(crate) fn idle_horizon_ms(&self) -> u64 {
        let tries = u64::from(self.max_replace).saturating_add(1);
        let backoffs = self.reserve_backoff_ms.saturating_mul(tries);
        backoffs.max(self.prepare_timeout_ms)
    }

    pub(crate) fn of(config: &PoolConfig) -> Self {
        let whole = whole_settings(config);
        Self {
            regain_grace_ms: whole.regain_grace_ms,
            prepare_timeout_ms: whole.prepare_timeout_ms,
            reserve_backoff_ms: whole.reserve_backoff_ms,
            max_replace: whole.max_replace,
        }
    }
}
