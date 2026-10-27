//! What the registry remembers of a whole-node request between its placements: how many
//! prepares failed, the hosts it must not be placed on, and the last failure. In memory
//! only here; the lease file keeps the same record in a later change.
//!
//! The registry has no request id yet, so a request is told apart by its content (the
//! whole [`Request`]): two equal requests share one record. A request id replaces the key
//! and nothing else in this module changes.

use std::collections::BTreeMap;

use js_lease::Timestamp;
use js_types::{HostId, LeaseId, Request};

use crate::config::PoolConfig;
use crate::whole_config::WholeNodeConfig;

use super::reasons::{Cause, HostEffect, RequestEffect};

/// Until when a host is left out of a request's placements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// Left out while the clock reads before this instant.
    Until(Timestamp),
    /// Left out for the rest of the request's life.
    ForLife,
}

impl Exclusion {
    fn holds(&self, now: Timestamp) -> bool {
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
    /// Drain give-ups. Nothing in the registry counts them; the reservation change does.
    pub give_ups: u32,
    /// The hosts left out of the request's placements, and until when.
    pub exclusions: BTreeMap<HostId, Exclusion>,
    /// The last prepare failure, if there was one.
    pub last: Option<NotReady>,
    /// Whether the request has used up `max_replace` and fails `exclusive-not-ready`.
    pub terminal: bool,
}

/// The records of the requests that have failed a prepare, and which request holds each
/// whole-node lease that is committed.
#[derive(Debug, Default)]
pub(crate) struct Requests {
    placed: BTreeMap<LeaseId, Request>,
    records: Vec<(Request, RequestRecord)>,
}

impl Requests {
    /// `lease` was committed for `request`.
    pub(crate) fn commit(&mut self, lease: LeaseId, request: &Request) {
        self.placed.insert(lease, request.clone());
    }

    /// The lease ended; it no longer stands for its request.
    pub(crate) fn release(&mut self, lease: &LeaseId) {
        self.placed.remove(lease);
    }

    /// The agent reported `lease` ready: its request is placed, and the record of its
    /// failed tries is no longer needed.
    pub(crate) fn granted(&mut self, lease: &LeaseId) {
        let request = self.placed.get(lease);
        request.into_iter().for_each(|request| {
            self.records.retain(|(kept, _)| kept != request);
        });
    }

    /// The record of `request`, if a prepare of it failed.
    pub(crate) fn record(&self, request: &Request) -> Option<&RequestRecord> {
        let found = self.records.iter().find(|(kept, _)| kept == request);
        found.map(|(_, record)| record)
    }

    /// Whether `host` is left out of `request`'s placements at `now`.
    pub(crate) fn excluded(&self, request: &Request, host: &HostId, now: Timestamp) -> bool {
        let record = self.record(request);
        let exclusion = record.and_then(|record| record.exclusions.get(host));
        exclusion.is_some_and(|exclusion| exclusion.holds(now))
    }

    /// Counts one prepare failure of the request `lease` was committed for, as the row
    /// of `cause` says, and releases the lease. A lease with no request (one put back
    /// after a restart) counts nowhere.
    pub(crate) fn fail(&mut self, lease: &LeaseId, cause: Cause, detail: &str, ctx: Failure<'_>) {
        let Some(request) = self.placed.remove(lease) else {
            return;
        };
        let at = self.records.iter().position(|(kept, _)| *kept == request);
        let at = at.unwrap_or_else(|| {
            self.records.push((request, RequestRecord::default()));
            self.records.len() - 1
        });
        let record = &mut self.records[at].1;
        let row = cause.row();
        match row.request {
            RequestEffect::Requeue => {
                record.prepare_failures = record.prepare_failures.saturating_add(1);
            }
        }
        let barred = match row.host {
            HostEffect::ExcludeForBackoff => Some(Exclusion::Until(ctx.backoff_end)),
            HostEffect::ExcludeForLife => Some(Exclusion::ForLife),
            HostEffect::Taint => None,
        };
        barred.into_iter().for_each(|exclusion| {
            let kept = record
                .exclusions
                .entry(ctx.host.clone())
                .or_insert(exclusion);
            *kept = (*kept).max(exclusion);
        });
        record.terminal = record.prepare_failures >= ctx.max_replace;
        record.last = Some(NotReady {
            code: cause.code(),
            detail: detail.to_string(),
        });
    }
}

/// Where and how a prepare failed, for [`Requests::fail`].
#[derive(Clone, Copy)]
pub(crate) struct Failure<'a> {
    pub(crate) host: &'a HostId,
    pub(crate) backoff_end: Timestamp,
    pub(crate) max_replace: u32,
}

/// The settings a failed prepare reads, from the pool's whole-node settings or, for a
/// pool without any, the defaults of a dedicated pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rules {
    pub(crate) prepare_timeout_ms: u64,
    pub(crate) reserve_backoff_ms: u64,
    pub(crate) max_replace: u32,
}

impl Rules {
    pub(crate) fn of(config: &PoolConfig) -> Self {
        let whole = config.whole_node().cloned();
        let whole = whole.unwrap_or_else(|| WholeNodeConfig::dedicated(config.max_machines()));
        Self {
            prepare_timeout_ms: whole.prepare_timeout_ms,
            reserve_backoff_ms: whole.reserve_backoff_ms,
            max_replace: whole.max_replace,
        }
    }
}
