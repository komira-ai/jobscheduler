//! The provider side of the registry: the booking, stop and observation calls the lease
//! table makes through [`crate::RegisteredProvider`], and what the placer is told of the
//! books between starts ([`crate::Placer::host_up`], [`crate::Placer::heard`],
//! [`crate::Placer::ended`], [`crate::Placer::host_gone`]).

use std::collections::BTreeMap;

use js_pool_static::SeatError;
use js_provider::{HostCapacity, Observed};
use js_types::sched::usage::NodeLoad;
use js_types::{Admission, HostId, LeaseId, RequestId, Resources};

use js_lease::Timestamp;

use super::record::Bound;
use super::{HostState, Record, Registry, State};
use crate::credential::Binding;
use crate::lives;
use crate::whole::exclusive::Exclusive;

impl Registry {
    /// Frees the booking at once, as the book does, and asks every live run of the lease
    /// to stop. The host takes no new lease until a heartbeat lists none of those runs.
    /// After a whole-node lease it takes none until a second heartbeat has followed the
    /// stop (the first may have crossed it) and the last one shows it serving with
    /// nothing held (design `whole-node.md` section 8). A whole-node request the lease
    /// presented and that still waits for a host is cancelled at once: it leaves the wait
    /// list and the host set aside for it returns to serving (`whole-node.md` section 3).
    pub(crate) fn stop(&self, lease: &LeaseId) -> Observed {
        let ended_at = self.shared.clock.now().millis();
        let source = self.shared.lives.get().map(|lives| &**lives);
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            requests,
            tail,
            waiting,
            marks,
            ..
        } = &mut *guard;
        // A cancelled request leaves no trace: no mark, and no record of its failures, so
        // the id names a fresh request if it is used again.
        let cancelled = waiting.cancel_lease(lease);
        cancelled.iter().for_each(|id| {
            marks.drop_request(id);
            requests.forget(id);
        });
        runs.stop_lease(lease);
        requests.release(lease);
        if let Observed::Present { host } = book.read(lease) {
            let record = records.get_mut(&host);
            record.into_iter().for_each(|record| {
                // A lease ends: the error of its estimate, at the time of this call on the
                // registry's clock. The host keeps a bound and a start for each lease it
                // started, so the stop of any of them counts. A lease put back after a
                // restart has neither unless a source of lives was set when it was put back
                // (see `seat`), so without one its stop counts nothing.
                let started: Vec<(LeaseId, u64)> = record
                    .bounds
                    .iter()
                    .filter(|bound| bound.lease == *lease)
                    .map(|bound| (bound.lease, bound.started_at))
                    .collect();
                let estimates = lives::est_ends(source, runs, &started);
                estimates
                    .into_iter()
                    .for_each(|estimate| tail.ended(ended_at, estimate));
                record.exclusive = record.exclusive.stopped();
            });
        }
        let read = book.stop(lease);
        if let Some(placer) = self.shared.placer.get() {
            placer.ended(*lease);
        }
        read
    }

    /// Binds each restored waiting request of `held` to the lease the lease file keeps it
    /// with, so that a stop of that lease cancels it even before the lease has presented it
    /// again. Call it once at a start, after [`Registry::restore_reservations`]. A request
    /// that is not waiting is left alone. How many were bound.
    pub fn bind_waiting(&self, held: &[(LeaseId, RequestId)]) -> usize {
        let mut state = self.lock();
        let bound = held
            .iter()
            .filter(|(lease, id)| state.waiting.bind(id, *lease));
        bound.count()
    }

    /// Whether the whole-node request lease `lease` presented last waits for a host
    /// (see [`js_provider::Provider::waits`]).
    pub(crate) fn waits(&self, lease: &LeaseId) -> bool {
        self.lock().waiting.holds_lease(lease)
    }

    /// `Present` only while the host holding the lease is live and, for a whole-node
    /// lease, ready; a lost or awaiting host, and a whole-node host not yet ready, is
    /// `Unknown`, never `Absent`, so the booking stays.
    pub(crate) fn observe(&self, lease: &LeaseId) -> Observed {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        self.advance_at(&mut state, now);
        match state.book.read(lease) {
            Observed::Present { host } => {
                let record = state.records.get(&host);
                let live = record
                    .is_some_and(|record| record.state(now, self.grace_ms()) == HostState::Live);
                let ready = record.is_some_and(|record| !record.exclusive.awaits_ready());
                match [live, ready].into_iter().all(|ok| ok) {
                    true => Observed::Present { host },
                    false => Observed::Unknown,
                }
            }
            absent => absent,
        }
    }

    pub(crate) fn capacity(&self) -> Vec<HostCapacity> {
        self.lock().book.hosts()
    }

    pub(crate) fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.lock().book.booked()
    }

    /// Puts back a booking the registry lost with its process. A host the registry does
    /// not know becomes an awaiting host: held, unobservable, not bookable, until its
    /// agent says hello; `binding` is what that hello must match, or `None` to let the
    /// first hello bind it. `times` is when the lease started and the latest it can end,
    /// as the lease file records them: with a source of lives set they are kept as the
    /// lease's start and bound. See [`crate::RegisteredProvider::seat_booked`].
    pub(crate) fn seat(
        &self,
        host: &HostId,
        lease: LeaseId,
        binding: Option<Binding>,
        shares: Option<Resources>,
        times: Option<(Timestamp, Timestamp)>,
    ) -> Result<(), SeatError> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State { book, records, .. } = &mut *guard;
        let seated = match book.seat_as(host, lease, shares.clone()) {
            Err(SeatError::Unlisted(_)) => {
                self.shared.config.list_in(book, host.clone());
                records.insert(host.clone(), Record::awaiting(binding, now));
                book.seat_as(host, lease, shares)
            }
            settled => settled,
        };
        // Only with the estimate feed on: without it a lease put back has no bound and
        // counts as ending now, as it always did.
        let feed = self.shared.lives.get().is_some();
        if let (Ok(()), true, Some((started, end))) = (&seated, feed, times) {
            let here = book.leases().remove(host).unwrap_or_default();
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                let kept = |bound: &Bound| here.contains(&bound.lease) & (bound.lease != lease);
                record.bounds.retain(kept);
                record.bounds.push(Bound {
                    lease,
                    end,
                    started_at: started.millis(),
                });
            });
        }
        // A restored whole-node booking is committed and not ready: the agent's
        // preparation is in memory only, so it must report ready for this lease again
        // before the lease reads present, a run is queued or the host books to anyone.
        // A host that lists held runs is ready: only the ready lease could have runs
        // (see `RegisteredProvider::seat` for where that fails).
        let whole = matches!(self.shared.config.class().admission(), Admission::Whole);
        if let (Ok(()), true) = (&seated, whole) {
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                let resumed = !record.held.is_empty();
                record.exclusive = Exclusive::seated(lease, now, resumed);
            });
        }
        seated
    }

    /// Tells the placer, if there is one, that `host` said hello, booked as the pool's
    /// shape.
    pub(super) fn tell_up(&self, host: &HostId) {
        if let Some(placer) = self.shared.placer.get() {
            placer.host_up(host, self.shared.config.booked());
        }
    }

    /// Tells the placer, if there is one, that `host` is no longer listed.
    pub(super) fn tell_gone(&self, host: &HostId) {
        if let Some(placer) = self.shared.placer.get() {
            placer.host_gone(host);
        }
    }

    /// Tells the placer, if there is one, that a heartbeat of `host` was accepted, with
    /// the reading it brought ([`crate::Placer::heard`]).
    pub(super) fn tell_heard(&self, host: &HostId, reading: Option<&NodeLoad>) {
        if let Some(placer) = self.shared.placer.get() {
            placer.heard(host, reading);
        }
    }
}
