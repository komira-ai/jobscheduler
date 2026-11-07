//! The wait list of whole-node requests the engine keeps for the registry: the registry
//! keeps no list, and asks the engine for every presentation, place and cancel.
//!
//! The rules are those of design `whole-node.md` sections 2 and 3:
//!
//! - **Order.** A request enters at the back at its first presentation that finds some host
//!   capable of holding it whole, and keeps its place, whatever the number of presentations,
//!   until it is placed, cancelled, refused or not presented for `regain_grace`
//!   ([`Engine::whole_expire`]). The first presentation is its instant: how long it has
//!   waited, and the start of its `drain_after`, count from there. The engine's queue
//!   ([`super::whole`]) takes the requests in this order.
//! - **Unservable.** While no host could ever be given whole to it, a request keeps waiting
//!   until `regain_grace` has passed since the first presentation that saw none; it is then
//!   refused and leaves the list. One that never had a place is refused at once.
//! - **Cap.** The list holds at most [`MAX_WAITING_REQUESTS`]: a new request past that is
//!   not entered, and a request on the list is never put out by a newcomer.
//! - **Lease.** Each request belongs to the lease that presented it last (or the one a
//!   restart bound it to): a stop of that lease takes it off the list
//!   ([`Engine::whole_cancel_lease`]).
//! - **Restart.** The registry's ledger puts back each request's place and instant
//!   ([`Engine::whole_restore_place`]); what the request asks for is filled in by its next
//!   presentation, and until then it is not queued. A request put back and never presented
//!   again within `regain_grace` is dropped and counted. A host set aside for it is put back
//!   too ([`Engine::whole_revive`]) and held outside the queue: while its host has not said
//!   hello it stands until `regain_grace` after the restart, and the request is not queued;
//!   once the host has, the request is queued with the mark when it is presented. A mark
//!   whose host never came back is dropped and counted; one on a tainted host is dropped.
//!
//! A request that leaves the list leaves the engine's queue at once ([`Engine::leave`]): its
//! mark or pending grant goes, and nothing of its wait is carried to a next presentation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_registry::{
    MAX_WAITING_REQUESTS, MarkedTails, Presented, Reservation, Standing, Waiter, WaitingRequest,
    WaitingWhole, WholeHost, WholeNodeConfig, WholeRound,
};
use js_types::Timestamp;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Request, RequestId};

use super::Engine;
use crate::map;

/// One waiting request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: RequestId,
    /// What the request asks for, as first presented; `None` for an entry put back from the
    /// ledger that the consumer has not presented since.
    request: Option<Request>,
    /// The lease that last presented the request, or the one a restart bound it to.
    lease: Option<LeaseId>,
    /// The first presentation.
    since: Timestamp,
    /// The last presentation.
    seen: Timestamp,
    /// The first presentation at which no host could ever be given whole; `None` while
    /// some host could.
    unservable_since: Option<Timestamp>,
    /// Whether a host was set aside for it at any time while it waited.
    marked: bool,
}

/// A host put back from the ledger for a request the engine does not queue yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    mark: Reservation,
    /// Until when it stands while its host is unknown; `None` once the host has been seen.
    claim_by: Option<Timestamp>,
}

/// The requests that wait for a host, in first-seen order, and the marks put back for those
/// not queued yet (see the module documentation).
#[derive(Debug, Clone, Default)]
pub struct WaitList {
    entries: Vec<Entry>,
    /// The marks put back from the ledger, by request, until the request is queued.
    kept: BTreeMap<RequestId, Kept>,
    /// The reservations put back that were dropped since the registry last asked.
    dropped: u64,
}

impl WaitList {
    fn at(&self, id: &RequestId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == *id)
    }

    fn of_lease(&self, lease: &LeaseId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.lease == Some(*lease))
    }

    /// The consumer presents `id` under `lease` at `now` (see the module documentation).
    fn present(
        &mut self,
        (id, lease, request): (&RequestId, LeaseId, &Request),
        capable: bool,
        now: Timestamp,
        grace_ms: u64,
    ) -> Presented {
        let Some(at) = self.at(id) else {
            return match (capable, self.entries.len() < MAX_WAITING_REQUESTS) {
                (false, _) => Presented::Unservable,
                (true, false) => Presented::Full,
                (true, true) => {
                    self.entries.push(Entry {
                        id: id.clone(),
                        request: Some(request.clone()),
                        lease: Some(lease),
                        since: now,
                        seen: now,
                        unservable_since: None,
                        marked: false,
                    });
                    Presented::Waiting(self.entries.len() - 1)
                }
            };
        };
        let entry = &mut self.entries[at];
        entry.seen = now;
        entry.request.get_or_insert_with(|| request.clone());
        entry.lease = Some(lease);
        entry.unservable_since = match capable {
            true => None,
            false => Some(entry.unservable_since.unwrap_or(now)),
        };
        let spent = entry
            .unservable_since
            .is_some_and(|since| now.millis().saturating_sub(since.millis()) >= grace_ms);
        if !spent {
            return Presented::Waiting(at);
        }
        self.entries.remove(at);
        Presented::Unservable
    }

    /// Drops every entry not presented for `grace_ms` at `now`, counting those put back
    /// that were never presented again: the ids dropped.
    fn expire(&mut self, now: Timestamp, grace_ms: u64) -> Vec<RequestId> {
        let lapsed = |entry: &Entry| now.millis().saturating_sub(entry.seen.millis()) >= grace_ms;
        let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(lapsed);
        self.entries = kept;
        let unclaimed = gone.iter().filter(|entry| entry.request.is_none()).count();
        self.dropped += u64::try_from(unclaimed).unwrap_or(u64::MAX);
        gone.into_iter().map(|entry| entry.id).collect()
    }

    /// Takes `id` off the list, with any mark put back for it; whether it waited.
    pub(super) fn cancel(&mut self, id: &RequestId) -> bool {
        self.kept.remove(id);
        let at = self.at(id);
        at.map(|at| self.entries.remove(at)).is_some()
    }

    /// Puts `id`, first presented at `since`, back at the back at `now`; `false` when the
    /// list is full.
    fn restore(&mut self, id: &RequestId, since: Timestamp, now: Timestamp) -> bool {
        let room = self.entries.len() < MAX_WAITING_REQUESTS;
        if room {
            self.entries.push(Entry {
                id: id.clone(),
                request: None,
                lease: None,
                since,
                seen: now,
                unservable_since: None,
                marked: false,
            });
        }
        room
    }

    /// Puts back `mark` for its request, standing until `claim_by` while its host is
    /// unknown; `false` when another mark holds its host.
    fn revive(&mut self, mark: &Reservation, claim_by: Timestamp) -> bool {
        let taken = self.kept.values().any(|kept| kept.mark.host == mark.host);
        if taken {
            return false;
        }
        let kept = Kept {
            mark: mark.clone(),
            claim_by: Some(claim_by),
        };
        self.kept.insert(mark.request.clone(), kept);
        self.note_marked([&mark.request]);
        true
    }

    /// Drops the marks put back that cannot stand on `hosts` at `now`: one on a tainted
    /// host, and one on a host the registry does not list once its claim has lapsed
    /// (counted). A listed host clears the wait.
    fn stand(&mut self, hosts: &[WholeHost], now: Timestamp) {
        let mut gone = 0;
        self.kept.retain(|_, kept| {
            let host = hosts.iter().find(|host| host.id == kept.mark.host);
            let Some(host) = host else {
                let waiting = kept.claim_by.is_some_and(|by| now < by);
                gone += u64::from(kept.claim_by.is_some() & !waiting);
                return waiting;
            };
            kept.claim_by = None;
            host.standing != Standing::Tainted
        });
        self.dropped += gone;
    }

    /// The presented requests the engine may queue, front first, each with the mark put back
    /// for it, which it takes: a request whose mark waits for its host is not among them.
    fn shown(&mut self) -> Vec<(RequestId, Duration, Timestamp, Option<Reservation>)> {
        let unclaimed = |id: &RequestId, kept: &BTreeMap<RequestId, Kept>| {
            kept.get(id).is_some_and(|kept| kept.claim_by.is_some())
        };
        let mut shown = Vec::new();
        for entry in &self.entries {
            let Some(request) = &entry.request else {
                continue;
            };
            if unclaimed(&entry.id, &self.kept) {
                continue;
            }
            let mark = self.kept.remove(&entry.id).map(|kept| kept.mark);
            shown.push((entry.id.clone(), request.max_wall, entry.since, mark));
        }
        shown
    }

    /// Notes that each of `ids` holds a host now.
    fn note_marked<'a>(&mut self, ids: impl IntoIterator<Item = &'a RequestId>) {
        let ids: BTreeSet<&RequestId> = ids.into_iter().collect();
        let marked = self
            .entries
            .iter_mut()
            .filter(|entry| ids.contains(&entry.id));
        marked.for_each(|entry| entry.marked = true);
    }

    /// The marks put back that are held outside the engine's queue.
    fn kept_marks(&self) -> Vec<Reservation> {
        self.kept.values().map(|kept| kept.mark.clone()).collect()
    }
}

impl Engine {
    /// The wait list's answer to the presentation of `id`, asking for `request`, under
    /// `lease` at `now`, with `capable` whether some host could ever hold it whole. A
    /// request refused as unservable leaves the engine's queue.
    pub fn whole_present(
        &mut self,
        (id, lease, request): (&RequestId, LeaseId, &Request),
        capable: bool,
        grace: Duration,
        now: FarmTime,
    ) -> Presented {
        let at = stamp(now);
        let presented = self
            .wait
            .present((id, lease, request), capable, at, millis(grace));
        if presented == Presented::Unservable {
            self.leave(id);
        }
        presented
    }

    /// Drops at `now` the requests not presented for `grace` from the list and the queue;
    /// the reservations put back that were dropped since this was last asked.
    pub fn whole_expire(&mut self, grace: Duration, now: FarmTime) -> u64 {
        let gone = self.wait.expire(stamp(now), millis(grace));
        for id in gone {
            self.leave(&id);
        }
        std::mem::take(&mut self.wait.dropped)
    }

    /// Takes `id` off the list and out of the queue; whether it waited.
    pub fn whole_cancel(&mut self, id: &RequestId) -> bool {
        let waited = self.wait.cancel(id);
        self.leave(id);
        waited
    }

    /// Takes the request `lease` presented last off the list and out of the queue.
    pub fn whole_cancel_lease(&mut self, lease: &LeaseId) -> Option<RequestId> {
        let at = self.wait.of_lease(lease)?;
        let id = self.wait.entries[at].id.clone();
        self.whole_cancel(&id);
        Some(id)
    }

    /// Binds waiting request `id` to `lease`; whether it waits.
    pub fn whole_bind(&mut self, id: &RequestId, lease: LeaseId) -> bool {
        let at = self.wait.at(id);
        let entry = at.map(|at| &mut self.wait.entries[at]);
        entry.map(|entry| entry.lease = Some(lease)).is_some()
    }

    /// How waiting request `id` waits.
    pub fn whole_waiter(&self, id: &RequestId) -> Option<Waiter> {
        let at = self.wait.at(id)?;
        Some(self.waiter(at))
    }

    /// How the request `lease` presented last waits.
    pub fn whole_waiter_of(&self, lease: &LeaseId) -> Option<Waiter> {
        let at = self.wait.of_lease(lease)?;
        Some(self.waiter(at))
    }

    fn waiter(&self, at: usize) -> Waiter {
        let entry = &self.wait.entries[at];
        Waiter {
            id: entry.id.clone(),
            since: entry.since,
            // No hold is given up, so none is counted.
            give_ups: 0,
            marked: entry.marked,
        }
    }

    /// Every request on the list, front first, with when it was first presented.
    pub fn whole_places(&self) -> Vec<(RequestId, Timestamp)> {
        let places = self.wait.entries.iter();
        places
            .map(|entry| (entry.id.clone(), entry.since))
            .collect()
    }

    /// The presented requests on the list, front first.
    pub fn whole_waiting(&self) -> Vec<WaitingRequest> {
        let waiting = self.wait.entries.iter().filter_map(|entry| {
            Some(WaitingRequest {
                id: entry.id.clone(),
                request: entry.request.clone()?,
                since: entry.since,
            })
        });
        waiting.collect()
    }

    /// Puts `id`, first presented at `since`, back on the list at `now`; `false` when full.
    pub fn whole_restore_place(&mut self, id: &RequestId, since: Timestamp, now: FarmTime) -> bool {
        self.wait.restore(id, since, stamp(now))
    }

    /// Puts back `mark`, standing until `claim_by` while its host is unknown; `false` when
    /// its host is set aside already.
    pub fn whole_revive(&mut self, mark: &Reservation, claim_by: Timestamp) -> bool {
        self.wait.revive(mark, claim_by)
    }

    /// `host` is retired at `now`: a mark put back on it goes, and the engine stands it
    /// draining, so the request it is set aside for loses it at once and is placed again.
    pub fn whole_drop_host(&mut self, host: &HostId, now: FarmTime) {
        self.wait.kept.retain(|_, kept| kept.mark.host != *host);
        let worker = map::worker(host);
        self.phased.insert(host.clone());
        let phase = js_types::sched::NodePhase::Draining;
        let event = js_sched::Event::Phase {
            worker: worker.clone(),
            phase,
        };
        self.feed(now, event);
        self.unplace_whole_where(|on| *on == worker);
        self.whole_tick(now);
    }

    /// The round of the engine's own queue at `now`: `round`, the registry's hosts and
    /// lives, with the presented requests of the wait list, the marks put back held outside
    /// the queue, and the caps lowered by them.
    pub(super) fn own_round(&mut self, round: WholeRound, now: FarmTime) -> WholeRound {
        let at = stamp(now);
        self.wait.stand(&round.hosts, at);
        let earlier = self.reservations(now);
        let shown = self.wait.shown().into_iter();
        let waiting = shown.map(|(id, max_wall, since, kept)| {
            let mark = kept.or_else(|| earlier.iter().find(|mark| mark.request == id).cloned());
            WaitingWhole {
                id,
                max_wall,
                since,
                give_ups: 0,
                exclusions: BTreeMap::new(),
                mark,
            }
        });
        let held = self.wait.kept_marks();
        let left = |end: u64| Duration::from_millis(end.saturating_sub(at.millis()));
        let outside: MarkedTails = held
            .iter()
            .map(|mark| (mark.host.clone(), mark.tail_end.map(left)))
            .collect();
        WholeRound {
            settings: outside_caps(&round.settings, &held),
            waiting: waiting.collect(),
            outside,
            ..round
        }
    }

    /// Runs one pass of the engine's own queue over `round`, the registry's hosts and lives,
    /// at `now` ([`Engine::own_round`], [`Engine::whole_round`]).
    pub fn whole_pass(&mut self, round: WholeRound, now: FarmTime) {
        let round = self.own_round(round, now);
        let held = self.whole_round(round, now);
        self.wait.note_marked(held.iter().map(|mark| &mark.request));
    }

    /// The hosts set aside for the waiting requests at `now`, as the registry shows them:
    /// the marks and pending grants of the queue, and the marks put back held outside it.
    pub fn whole_held(&self, now: FarmTime) -> Vec<Reservation> {
        let mut held = self.reservations(now);
        held.extend(self.wait.kept_marks());
        held
    }

    /// Request `id` left the wait list: its mark put back goes, and it leaves the queue,
    /// its mark or pending grant with it.
    pub(super) fn leave(&mut self, id: &RequestId) {
        self.wait.kept.remove(id);
        let Some(operation) = self.whole.remove(id) else {
            return;
        };
        self.prepare.dequeue(id);
        let _ = self.sched.cancel_whole(operation);
    }
}

/// `settings` with the caps lowered by `held`, the hosts held outside the queue: each is one
/// host whole less under `max_whole_node` and one more kept from the ceiling's share, and
/// each that drains a busy host one less under `max_draining`, as the engine's pass
/// counts them.
fn outside_caps(settings: &WholeNodeConfig, held: &[Reservation]) -> WholeNodeConfig {
    let draining = held.iter().filter(|mark| !mark.assigned).count();
    WholeNodeConfig {
        max_whole_node: settings.max_whole_node.saturating_sub(held.len()),
        keep_for_ordinary: settings.keep_for_ordinary.saturating_add(held.len()),
        max_draining: settings.max_draining.saturating_sub(draining),
        ..settings.clone()
    }
}

/// `at` as a time on the registry's clock.
fn stamp(at: FarmTime) -> Timestamp {
    Timestamp::from_millis(at.as_millis())
}

/// `duration` in milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
