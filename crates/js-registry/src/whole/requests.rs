//! What the registry remembers of a whole-node request between its placements: how many
//! prepares failed, the hosts it must not be placed on, and the last failure.
//!
//! A request is told apart by its id when its start carried one
//! ([`js_provider::StartSpec::with_request_id`]). A start without an id falls back to the
//! content (the whole [`Request`]): two equal requests share one record.
//!
//! # How long a record lives
//!
//! - It is removed when the agent reports a lease of the request ready (`granted`).
//! - A request that used up `max_replace` is *terminal*: its next start is refused
//!   `ExclusiveNotReady` (wire `exclusive-not-ready`), and that refusal removes the
//!   record and its exclusions, so the start after it is a fresh request.
//!   [`Registry::not_ready`] holds the reason until the refusal has been delivered.
//! - A record with no committed lease whose request has not been started for
//!   `Rules::idle_horizon_ms` is dropped, terminal or not, with its exclusions
//!   (`ForLife` ones included): the consumer is gone, and the records stay bounded by the
//!   requests in flight rather than by every request that ever failed.
//!
//! The failed-prepare count and the exclusions of a request that carries an id are kept in
//! the lease file (the registry's reservation ledger) and put back at a start by
//! [`Requests::restore`], so a restart does not reset `max_replace`. No request gives a
//! host up, so the give-up count is always zero: the ledger writes zero, and a count an
//! older file holds is read and ignored. What is not kept: the last failure's text (a restored record
//! has none) and the records of requests without an id, which are told apart by their
//! content and forgotten by a restart.
//!
//! Under `--placement js-sched` the engine keeps all of this (`js-sched-placer`'s
//! `engine::prepare`) for every whole-node request, those told apart by their content
//! included, through [`crate::Placer::prepare_failed`], [`crate::Placer::whole_commit`] and
//! their siblings, and these records are never called ([`crate::Placer::keeps_wait_list`]).
//! The engine hands its counts and bars to the ledger and takes them back at a start
//! ([`crate::Placer::whole_ledger`], [`crate::Placer::whole_restore`]), so a restart under
//! that placement keeps them too. The failed prepares the allocation log records are the
//! registry's under both placements ([`FailedPrepare`]).
//!
//! [`Registry::not_ready`]: crate::Registry::not_ready

use std::collections::BTreeMap;

use js_lease::Timestamp;
use js_provider::StartSpec;
use js_types::{HostId, LeaseId, Request, RequestId};
use js_usage::ProfileKey;

use crate::config::PoolConfig;
use crate::whole_config::WholeNodeConfig;

use super::calls::Calls;
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
    records: Vec<Entry>,
    /// The calls the records have had ([`crate::Registry::machine_calls`]).
    pub(crate) calls: Calls,
}

/// A committed whole-node lease whose prepare failed: the booking it freed, kept by the
/// registry for the record of its end until the lock is released (under every placement:
/// it is the allocation log's, not a request's record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedPrepare {
    pub(crate) lease: LeaseId,
    pub(crate) host: HostId,
    pub(crate) at: Timestamp,
    /// The first profile name a run of the lease was queued with, if one was.
    pub(crate) profile: Option<ProfileKey>,
}

/// A record, the request it belongs to and when the request last asked for a host or
/// failed one.
#[derive(Debug)]
struct Entry {
    key: RequestKey,
    record: RequestRecord,
    touched: Timestamp,
}

impl Requests {
    /// `lease` was committed for `request`.
    pub(crate) fn commit(&mut self, lease: LeaseId, key: RequestKey) {
        self.calls.note();
        self.placed.insert(lease, key);
    }

    /// Whether a lease other than `lease` is committed for the request under `key`.
    pub(crate) fn held_by_other(&self, key: &RequestKey, lease: &LeaseId) -> bool {
        self.calls.note();
        self.placed
            .iter()
            .any(|(held, kept)| (kept == key) & (held != lease))
    }

    /// Drops the record of request `id`: its consumer cancelled it, so a later request
    /// under the id starts with no failures or exclusions.
    pub(crate) fn forget(&mut self, id: &RequestId) {
        self.calls.note();
        let key = RequestKey::from(id);
        self.records.retain(|entry| entry.key != key);
    }

    /// The lease ended; it no longer stands for its request.
    pub(crate) fn release(&mut self, lease: &LeaseId) {
        self.calls.note();
        self.placed.remove(lease);
    }

    /// The agent reported `lease` ready: its request is placed, and the record of its
    /// failed tries is no longer needed.
    pub(crate) fn granted(&mut self, lease: &LeaseId) {
        self.calls.note();
        let key = self.placed.get(lease);
        key.into_iter().for_each(|key| {
            self.records.retain(|entry| entry.key != *key);
        });
    }

    /// A start of the request under `key` at `now`. A terminal request's record is
    /// removed and `true` is returned, so the start is refused and the next start of the
    /// request is a fresh one; any other record is marked as in use.
    pub(crate) fn start(&mut self, key: &RequestKey, now: Timestamp) -> bool {
        self.calls.note();
        let at = self.records.iter().position(|entry| entry.key == *key);
        let Some(at) = at else {
            return false;
        };
        let terminal = self.records[at].record.terminal;
        match terminal {
            true => {
                self.records.remove(at);
            }
            false => self.records[at].touched = now,
        }
        terminal
    }

    /// Drops the records whose request has no committed lease and has not started or
    /// failed for `horizon_ms`.
    pub(crate) fn sweep(&mut self, now: Timestamp, horizon_ms: u64) {
        self.calls.note();
        let placed = &self.placed;
        self.records.retain(|entry| {
            let idle = now.millis().saturating_sub(entry.touched.millis());
            let committed = placed.values().any(|key| *key == entry.key);
            committed || idle < horizon_ms
        });
    }

    /// Puts back the record of the request under `key` from the ledger, touched at `now`
    /// so the idle horizon counts from the start. Called once per request, at a start,
    /// before the request has a record of its own.
    pub(crate) fn restore(&mut self, key: RequestKey, record: RequestRecord, now: Timestamp) {
        self.calls.note();
        self.records.push(Entry {
            key,
            record,
            touched: now,
        });
    }

    /// The records of the requests that carry an id, for the ledger.
    pub(crate) fn recorded(&self) -> impl Iterator<Item = (&RequestId, &RequestRecord)> {
        self.calls.note();
        self.records.iter().filter_map(|entry| match &entry.key {
            RequestKey::Id(id) => Some((id, &entry.record)),
            RequestKey::Content(_) => None,
        })
    }

    /// The record of the request under `key`, if a prepare of it failed.
    pub(crate) fn record(&self, key: &RequestKey) -> Option<&RequestRecord> {
        self.calls.note();
        let found = self.records.iter().find(|entry| entry.key == *key);
        found.map(|entry| &entry.record)
    }

    /// Whether `host` is left out of the placements of the request under `key` at `now`.
    pub(crate) fn excluded(&self, key: &RequestKey, host: &HostId, now: Timestamp) -> bool {
        self.calls.note();
        let record = self.record(key);
        let exclusion = record.and_then(|record| record.exclusions.get(host));
        exclusion.is_some_and(|exclusion| exclusion.holds(now))
    }

    /// Whether `host` is left out of the placements of the request under `key` for the
    /// rest of its life: no wait makes it eligible.
    pub(crate) fn barred_for_life(&self, key: &RequestKey, host: &HostId) -> bool {
        self.calls.note();
        let record = self.record(key);
        let exclusion = record.and_then(|record| record.exclusions.get(host));
        exclusion == Some(&Exclusion::ForLife)
    }

    /// The record under `key`, made empty when there is none, marked as in use at `now`.
    fn entry(&mut self, key: RequestKey, now: Timestamp) -> &mut RequestRecord {
        let at = self.records.iter().position(|entry| entry.key == key);
        let at = at.unwrap_or_else(|| {
            self.records.push(Entry {
                key,
                record: RequestRecord::default(),
                touched: now,
            });
            self.records.len() - 1
        });
        self.records[at].touched = now;
        &mut self.records[at].record
    }

    /// Counts one prepare failure of the request `lease` was committed for, as the row
    /// of `cause` says, and releases the lease. A lease with no request (one put back
    /// after a restart) counts nowhere.
    pub(crate) fn fail(&mut self, lease: &LeaseId, cause: Cause, detail: &str, ctx: Failure<'_>) {
        self.calls.note();
        let Some(key) = self.placed.remove(lease) else {
            return;
        };
        let record = self.entry(key, ctx.now);
        let row = cause.row();
        match row.request {
            RequestEffect::Requeue => {
                record.prepare_failures = record.prepare_failures.saturating_add(1);
            }
        }
        exclusion(cause, ctx.backoff_end)
            .into_iter()
            .for_each(|exclusion| record.exclude(ctx.host, exclusion));
        record.terminal = record.prepare_failures >= ctx.max_replace;
        record.last = Some(NotReady {
            code: cause.code(),
            detail: detail.to_string(),
        });
    }
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

/// Where and how a prepare failed, for [`Requests::fail`].
#[derive(Clone, Copy)]
pub(crate) struct Failure<'a> {
    pub(crate) host: &'a HostId,
    pub(crate) now: Timestamp,
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
