//! What the registry remembers of a whole-node request between its placements: how many
//! prepares failed, the hosts it must not be placed on, and the last failure. In memory
//! only here; the lease file keeps the same record in a later change.
//!
//! A request is told apart by its id when its start carried one
//! ([`js_provider::StartSpec::with_request_id`]). A start without an id falls back to the
//! content (the whole [`Request`]): two equal requests share one record.

use std::collections::BTreeMap;

use js_lease::Timestamp;
use js_provider::StartSpec;
use js_types::{HostId, LeaseId, Request, RequestId};

use crate::config::PoolConfig;
use crate::whole_config::WholeNodeConfig;

use super::reasons::{Cause, HostEffect, RequestEffect};

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
    /// Drain give-ups: reservations of the request that passed their deadline with the
    /// host not quiet.
    pub give_ups: u32,
    /// The hosts left out of the request's placements, and until when.
    pub exclusions: BTreeMap<HostId, Exclusion>,
    /// The last prepare failure, if there was one.
    pub last: Option<NotReady>,
    /// Whether the request has used up `max_replace` and fails `exclusive-not-ready`.
    pub terminal: bool,
}

impl RequestRecord {
    /// Leaves `host` out of the request's placements for as long as the longer of the
    /// exclusion held already and `exclusion`.
    fn exclude(&mut self, host: &HostId, exclusion: Exclusion) {
        let kept = self.exclusions.entry(host.clone()).or_insert(exclusion);
        *kept = (*kept).max(exclusion);
    }
}

/// The records of the requests that have failed a prepare, and which request holds each
/// whole-node lease that is committed.
#[derive(Debug, Default)]
pub(crate) struct Requests {
    placed: BTreeMap<LeaseId, RequestKey>,
    records: Vec<(RequestKey, RequestRecord)>,
}

impl Requests {
    /// `lease` was committed for `request`.
    pub(crate) fn commit(&mut self, lease: LeaseId, key: RequestKey) {
        self.placed.insert(lease, key);
    }

    /// The lease ended; it no longer stands for its request.
    pub(crate) fn release(&mut self, lease: &LeaseId) {
        self.placed.remove(lease);
    }

    /// The agent reported `lease` ready: its request is placed, and the record of its
    /// failed tries is no longer needed.
    pub(crate) fn granted(&mut self, lease: &LeaseId) {
        let key = self.placed.get(lease);
        key.into_iter().for_each(|key| {
            self.records.retain(|(kept, _)| kept != key);
        });
    }

    /// The record of the request under `key`, if a prepare of it failed.
    pub(crate) fn record(&self, key: &RequestKey) -> Option<&RequestRecord> {
        let found = self.records.iter().find(|(kept, _)| kept == key);
        found.map(|(_, record)| record)
    }

    /// Whether `host` is left out of the placements of the request under `key` at `now`.
    pub(crate) fn excluded(&self, key: &RequestKey, host: &HostId, now: Timestamp) -> bool {
        let record = self.record(key);
        let exclusion = record.and_then(|record| record.exclusions.get(host));
        exclusion.is_some_and(|exclusion| exclusion.holds(now))
    }

    /// Whether `host` is left out of the placements of the request under `key` for the
    /// rest of its life: no wait makes it eligible.
    pub(crate) fn barred_for_life(&self, key: &RequestKey, host: &HostId) -> bool {
        let record = self.record(key);
        let exclusion = record.and_then(|record| record.exclusions.get(host));
        exclusion == Some(&Exclusion::ForLife)
    }

    /// The record under `key`, made empty when there is none.
    fn entry(&mut self, key: RequestKey) -> &mut RequestRecord {
        let at = self.records.iter().position(|(kept, _)| *kept == key);
        let at = at.unwrap_or_else(|| {
            self.records.push((key, RequestRecord::default()));
            self.records.len() - 1
        });
        &mut self.records[at].1
    }

    /// Counts one drain give-up of the request under `key` on `host`, which is left out
    /// of its placements until `backoff_end`.
    pub(crate) fn give_up(&mut self, key: RequestKey, host: &HostId, backoff_end: Timestamp) {
        let record = self.entry(key);
        record.give_ups = record.give_ups.saturating_add(1);
        record.exclude(host, Exclusion::Until(backoff_end));
    }

    /// The drain give-ups counted for the request under `key`.
    pub(crate) fn give_ups(&self, key: &RequestKey) -> u32 {
        self.record(key).map_or(0, |record| record.give_ups)
    }

    /// Counts one prepare failure of the request `lease` was committed for, as the row
    /// of `cause` says, and releases the lease. A lease with no request (one put back
    /// after a restart) counts nowhere.
    pub(crate) fn fail(&mut self, lease: &LeaseId, cause: Cause, detail: &str, ctx: Failure<'_>) {
        let Some(key) = self.placed.remove(lease) else {
            return;
        };
        let record = self.entry(key);
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
        barred
            .into_iter()
            .for_each(|exclusion| record.exclude(ctx.host, exclusion));
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
