//! The prepare of a whole-node lease the engine commits from its queue under
//! `--placement js-sched` ([`Engine::commit_whole`]), and what the engine keeps of a
//! request whose prepare failed: the rules of `js-registry`'s `whole/exclusive.rs`,
//! `whole/requests.rs` and `whole/ending.rs` for a request the engine queues, held here
//! instead of in the registry (design 5.4, `whole-node.md` section 5).
//!
//! - **Prepare.** At the commit the engine takes the lease's deadline: the commit time plus
//!   `prepare_timeout`. The registry asks whether it has passed only as the lease table's
//!   tick reads the pending leases ([`Engine::prepare_overdue`]), never at a heartbeat, so a
//!   ready report that arrives before that tick wins. A report the registry takes (exactly
//!   this lease, from a clean host that is committed to it) ends the deadline
//!   ([`Engine::prepare_ready`]); one for another lease, from a tainted host or for a host
//!   in another state never reaches the engine. A hello that turns a ready host back to
//!   committed gives the agent a whole `prepare_timeout` from the hello
//!   ([`Engine::prepare_again`]: the hello rule as built).
//! - **Failure.** A typed refusal or the deadline ends the lease as a failed prepare
//!   ([`Engine::prepare_failed`]). The request counts one failure toward `max_replace`, the
//!   host is kept from it by a hard bar as the row of the cause says (until
//!   `reserve_backoff` after the failure, or for the request's life; a row that taints the
//!   host bars nothing, since a tainted host takes no request), and the lease and its
//!   operation are let go. The request keeps the time it was first queued, its give-ups
//!   and its bars: its next presentation is queued with them ([`Scheduler::restore_whole`]),
//!   so it is placed again without waiting `drain_after` afresh and is never granted a
//!   barred host. The operation is not kept queued in between: the registry takes the
//!   request off its wait list at the commit, and a queued operation with no consumer
//!   presenting it would be granted a host it never takes.
//! - **`max_replace`.** At `max_replace` failures the request is terminal: its next start
//!   is refused `exclusive-not-ready` once, which lets its record and bars go, so the start
//!   after it is a fresh request ([`Engine::whole_start`]).
//! - **Record.** The record of a request goes when a lease of it is reported ready, when its
//!   terminal refusal is delivered, and when it has had no start and no failure for the
//!   registry's idle horizon and holds no committed lease ([`Engine::whole_sweep`]): its
//!   bars for life go with it; and when the request is cancelled while it waits
//!   ([`Engine::whole_forget`]). The registry keeps the key the request is told apart by and
//!   the horizon.
//!
//! A lease that ends otherwise (released, expired, lost with its host) counts nothing: its
//! deadline goes with it ([`Engine::ended`]).
//!
//! [`Scheduler::restore_whole`]: js_sched::Scheduler::restore_whole

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_lease::Timestamp;
use js_registry::{Exclusion, NotReady, PrepareFailure, RequestRecord};
use js_sched::Bar;
use js_types::sched::{FarmTime, OperationId, WorkerId};
use js_types::{LeaseId, RequestId};

use super::Engine;
use crate::map;

/// A whole-node lease the engine committed, from its commit until it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Preparing {
    /// The request the lease was committed for.
    id: RequestId,
    /// The worker it holds.
    worker: WorkerId,
    /// When the agent must have reported it ready; `None` while it is ready.
    deadline: Option<FarmTime>,
    /// When the request was first queued.
    queued_at: FarmTime,
    /// The drain give-ups the request had at the commit.
    give_ups: u32,
    /// The workers barred to the request at the commit by a hard bar (a give-up's back-off
    /// stays the scheduler's soft bar and is not kept).
    bars: BTreeMap<WorkerId, Bar>,
}

/// One request as [`Prepare::ledger`] gives it: its id, failed prepares, give-ups and bars.
pub(super) type LedgerRow<'a> = (&'a RequestId, u32, u32, &'a BTreeMap<WorkerId, Bar>);

/// What the engine keeps of a request whose prepare failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    /// Prepares that failed, by a refusal or a timeout.
    failures: u32,
    /// The drain give-ups the request had when its prepare last failed.
    give_ups: u32,
    /// The workers barred to the request, and how.
    bars: BTreeMap<WorkerId, Bar>,
    /// The last failure: what a terminal refusal names. `None` for a record put back from
    /// the reservation ledger, which keeps no text ([`Prepare::restore`]).
    last: Option<NotReady>,
    /// Whether the request has used up `max_replace`.
    terminal: bool,
    /// When the request was first queued.
    queued_at: FarmTime,
    /// When the request last started or failed.
    touched: FarmTime,
}

/// The prepares of the leases the engine committed and the records of the requests whose
/// prepare failed (see the module documentation).
#[derive(Debug, Clone, Default)]
pub(super) struct Prepare {
    /// The committed leases, by the registry's lease.
    preparing: BTreeMap<LeaseId, Preparing>,
    /// The records, by request.
    records: BTreeMap<RequestId, Record>,
    /// When each request the engine queues was first queued.
    queued_at: BTreeMap<RequestId, FarmTime>,
    /// `prepare_timeout` of the last round's settings.
    timeout_ms: u64,
    /// `max_replace` of the last round's settings.
    max_replace: u32,
}

impl Prepare {
    /// Takes `prepare_timeout` and `max_replace` from the settings of a round: a lease is
    /// committed only from a round, so they are set before the first commit.
    pub(super) fn take_rules(&mut self, prepare_timeout_ms: u64, max_replace: u32) {
        self.timeout_ms = prepare_timeout_ms;
        self.max_replace = max_replace;
    }

    /// The time request `id`, queued now at `since`, is queued from: when it was first
    /// queued if its prepare failed before, else `since`. Recorded until the request is
    /// committed or leaves the queue.
    pub(super) fn queue(&mut self, id: &RequestId, since: FarmTime) -> FarmTime {
        let first = self
            .records
            .get(id)
            .map_or(since, |record| record.queued_at);
        self.queued_at.insert(id.clone(), first);
        first
    }

    /// The give-ups and bars request `id` keeps from its failed prepares, to queue it with.
    pub(super) fn kept(&self, id: &RequestId) -> (u32, BTreeMap<WorkerId, Bar>) {
        let record = self.records.get(id);
        let give_ups = record.map_or(0, |record| record.give_ups);
        let bars = record.map(|record| record.bars.clone()).unwrap_or_default();
        (give_ups, bars)
    }

    /// Request `id` left the engine's queue without a commit.
    pub(super) fn dequeue(&mut self, id: &RequestId) {
        self.queued_at.remove(id);
    }

    /// Puts back the record of request `id` a restart's ledger kept: `failures` failed
    /// prepares (one at least), its give-ups and bars, whether it is terminal, and when it
    /// was first queued, touched at `now`. The ledger keeps no text of the last failure.
    pub(super) fn restore(
        &mut self,
        id: &RequestId,
        (failures, give_ups, bars): (u32, u32, BTreeMap<WorkerId, Bar>),
        (terminal, queued_at): (bool, FarmTime),
        now: FarmTime,
    ) {
        let record = Record {
            failures,
            give_ups,
            bars,
            last: None,
            terminal,
            queued_at,
            touched: now,
        };
        self.records.insert(id.clone(), record);
    }

    /// What the records and the committed leases keep of each request for the reservation
    /// ledger: its failed prepares, its give-ups and the bars of its failures.
    pub(super) fn ledger(&self) -> Vec<LedgerRow<'_>> {
        let held = self.preparing.values();
        let held = held.map(|held| (&held.id, 0, held.give_ups, &held.bars));
        let failed = self.records.iter();
        let failed =
            failed.map(|(id, record)| (id, record.failures, record.give_ups, &record.bars));
        held.chain(failed).collect()
    }

    /// Lease `lease` ended: its deadline goes, and it counts nothing.
    pub(super) fn end(&mut self, lease: &LeaseId) {
        self.preparing.remove(lease);
    }
}

impl Engine {
    /// The engine committed `lease` of request `id`, granted `worker` by `operation`, at
    /// `now`: the agent has until `prepare_timeout` from now to report it ready.
    pub(super) fn start_prepare(
        &mut self,
        (lease, id): (LeaseId, &RequestId),
        (operation, worker): (OperationId, &WorkerId),
        now: FarmTime,
    ) {
        let timeout = Duration::from_millis(self.prepare.timeout_ms);
        let queued_at = self.prepare.queued_at.remove(id).unwrap_or(now);
        let preparing = Preparing {
            id: id.clone(),
            worker: worker.clone(),
            deadline: Some(now.saturating_add(timeout)),
            queued_at,
            give_ups: self.sched.whole_give_ups(operation),
            bars: hard(self.sched.whole_barred(operation, now)),
        };
        self.prepare.preparing.insert(lease, preparing);
    }

    /// Whether the prepare of `lease` has passed its deadline at `now`: `None` for a lease
    /// the engine did not commit from its queue, `Some(false)` for one reported ready.
    pub fn prepare_overdue(&self, lease: &LeaseId, now: FarmTime) -> Option<bool> {
        let preparing = self.prepare.preparing.get(lease)?;
        Some(preparing.deadline.is_some_and(|deadline| now >= deadline))
    }

    /// The agent reported `lease` ready: its deadline ends, and its request's record (failed
    /// prepares, bars, give-ups) goes. Nothing for a lease the engine did not commit.
    pub fn prepare_ready(&mut self, lease: &LeaseId) {
        let Some(preparing) = self.prepare.preparing.get_mut(lease) else {
            return;
        };
        preparing.deadline = None;
        self.prepare.records.remove(&preparing.id);
    }

    /// The host of `lease`, ready, said hello again at `now`: the agent has a whole
    /// `prepare_timeout` from now to report it ready again. A lease still waiting for its
    /// first report keeps its deadline, so an agent that keeps restarting cannot put it off.
    pub fn prepare_again(&mut self, lease: &LeaseId, now: FarmTime) {
        let timeout = Duration::from_millis(self.prepare.timeout_ms);
        let preparing = self.prepare.preparing.get_mut(lease);
        preparing.into_iter().for_each(|preparing| {
            preparing
                .deadline
                .get_or_insert(now.saturating_add(timeout));
        });
    }

    /// The prepare of `lease` failed at `now` as `failure` says (see the module
    /// documentation): `true` when the engine committed the lease and has counted the
    /// failure and let the lease go, `false` for any other lease.
    pub fn prepare_failed(
        &mut self,
        lease: &LeaseId,
        failure: &PrepareFailure,
        now: FarmTime,
    ) -> bool {
        let Some(preparing) = self.prepare.preparing.remove(lease) else {
            return false;
        };
        let max_replace = self.prepare.max_replace;
        let record = self.prepare.records.entry(preparing.id.clone());
        let record = record.or_insert_with(|| Record {
            failures: 0,
            give_ups: 0,
            bars: BTreeMap::new(),
            last: None,
            terminal: false,
            queued_at: preparing.queued_at,
            touched: now,
        });
        record.failures = record.failures.saturating_add(1);
        record.give_ups = record.give_ups.max(preparing.give_ups);
        let barred = failure
            .exclusion
            .map(|exclusion| (preparing.worker, bar_of(exclusion)));
        for (worker, bar) in preparing.bars.into_iter().chain(barred) {
            let kept = record.bars.get(&worker).copied();
            record.bars.insert(worker, stronger(kept, bar));
        }
        record.terminal = record.failures >= max_replace;
        record.last = Some(failure.not_ready.clone());
        record.queued_at = preparing.queued_at;
        record.touched = now;
        self.ended(*lease);
        true
    }

    /// A start of request `id` at `now`: `true` once for a terminal request, whose record
    /// then goes (the start is refused, and the next one is a fresh request); `false` for any
    /// other, whose record, if it has one, is marked as in use.
    pub fn whole_start(&mut self, id: &RequestId, now: FarmTime) -> bool {
        let Some(record) = self.prepare.records.get_mut(id) else {
            return false;
        };
        record.touched = now;
        let terminal = record.terminal;
        if terminal {
            self.prepare.records.remove(id);
        }
        terminal
    }

    /// The record of request `id` as the registry shows it: `None` while none of its
    /// prepares has failed since it was last placed.
    pub fn whole_record(&self, id: &RequestId) -> Option<RequestRecord> {
        let record = self.prepare.records.get(id)?;
        let exclusions = record.bars.iter().filter_map(|(worker, bar)| {
            let host = map::host_id(worker).ok()?;
            Some((host, exclusion_of(*bar)))
        });
        Some(RequestRecord {
            prepare_failures: record.failures,
            give_ups: record.give_ups,
            exclusions: exclusions.collect(),
            last: record.last.clone(),
            terminal: record.terminal,
        })
    }

    /// Request `id` was cancelled while it waited: its record, its bars and its operation go
    /// at once, so the id names a fresh request if it is used again.
    pub fn whole_forget(&mut self, id: &RequestId) {
        self.prepare.records.remove(id);
        self.prepare.dequeue(id);
        self.carried.remove(id);
        self.restored.remove(id);
        // The request leaves the queue at once, with the bars its record set there.
        if let Some(operation) = self.whole.remove(id) {
            let _ = self.sched.cancel_whole(operation);
        }
    }

    /// Drops at `now` the records of the requests with no committed lease that have had no
    /// start and no failure for `horizon`, bars for life and all, and the give-ups and bars a
    /// restart put back for requests not queued within `horizon` of it.
    pub fn whole_sweep(&mut self, horizon: Duration, now: FarmTime) {
        let lapsed = |at: &FarmTime| now.saturating_duration_since(*at) >= horizon;
        let gone: Vec<RequestId> = self
            .restored
            .iter()
            .filter(|(_, at)| lapsed(at))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.restored.remove(&id);
            self.carried.remove(&id);
        }
        let Prepare {
            preparing, records, ..
        } = &mut self.prepare;
        let committed: BTreeSet<&RequestId> = preparing.values().map(|held| &held.id).collect();
        records.retain(|id, record| {
            let idle = now.saturating_duration_since(record.touched);
            committed.contains(id) | (idle < horizon)
        });
    }
}

/// The hard bars of `bars`: a give-up's back-off is soft (the scheduler may still grant the
/// worker when no other could take the request), so the registry is never told to keep the
/// request off it.
fn hard(bars: BTreeMap<WorkerId, Bar>) -> BTreeMap<WorkerId, Bar> {
    let hard = bars
        .into_iter()
        .filter(|(_, bar)| !matches!(bar, Bar::Backoff(_)));
    hard.collect()
}

/// The hard bar of `exclusion`: the worker is neither marked for the request nor granted to
/// it while it holds.
pub(super) fn bar_of(exclusion: Exclusion) -> Bar {
    match exclusion {
        Exclusion::Until(end) => Bar::Until(FarmTime::from_millis(end.millis())),
        Exclusion::ForLife => Bar::ForLife,
    }
}

/// How the registry shows `bar`: until its end, or for life.
pub(super) fn exclusion_of(bar: Bar) -> Exclusion {
    match bar {
        Bar::Backoff(end) | Bar::Until(end) => {
            Exclusion::Until(Timestamp::from_millis(end.as_millis()))
        }
        Bar::ForLife => Exclusion::ForLife,
    }
}

/// The bar a worker keeps when `bar` is set on it over `kept`: a hard bar over a back-off,
/// then the one that ends later (a bar for life never ends).
pub(super) fn stronger(kept: Option<Bar>, bar: Bar) -> Bar {
    let strength = |bar: Bar| {
        let end = match bar {
            Bar::Backoff(end) | Bar::Until(end) => end.as_millis(),
            Bar::ForLife => u64::MAX,
        };
        (!matches!(bar, Bar::Backoff(_)), end)
    };
    kept.filter(|kept| strength(*kept) > strength(bar))
        .unwrap_or(bar)
}

#[cfg(test)]
#[path = "prepare_tests.rs"]
mod tests;
