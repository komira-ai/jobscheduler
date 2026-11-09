//! The provider side of the registry: the booking, stop and observation calls the lease
//! table makes through [`crate::RegisteredProvider`], and what the placer is told of the
//! books between starts ([`crate::Placer::host_up`], [`crate::Placer::heard`],
//! [`crate::Placer::ended`], [`crate::Placer::host_gone`]) and of the prepares of
//! whole-node leases ([`crate::Placer::prepare_ready`], [`crate::Placer::prepare_again`]).

use std::collections::BTreeMap;

use js_provider::SeatError;
use js_provider::{HostCapacity, Observed};
use js_types::sched::usage::NodeLoad;
use js_types::{Admission, HostId, LeaseId, RequestId, Resources, Shape};

use js_types::Timestamp;

use super::record::{Bound, Queue};
use super::{HostState, Record, Registry, State};
use crate::credential::Binding;
use crate::placer::{LeaseTimes, SeatedLease};
use crate::whole::exclusive::Exclusive;

impl Registry {
    /// Frees the booking at once, as the book does, and asks every live run of the lease
    /// to stop. The host takes no new lease until a heartbeat lists none of those runs.
    /// After a whole-node lease it takes none until a second heartbeat has followed the
    /// stop (the first may have crossed it) and the last one shows it serving with
    /// nothing held (design `whole-node.md` section 8). A whole-node request the lease
    /// presented and that still waits for a host is cancelled at once: it leaves the wait
    /// list and the host set aside for it returns to serving (`whole-node.md` section 3).
    /// With a log set ([`Registry::set_allocation_log`]), a stop that frees a booking or
    /// cancels a waiting request hands the log the record of it, once the lock is released.
    pub(crate) fn stop(&self, lease: &LeaseId) -> Observed {
        let ended_at = self.shared.clock.now().millis();
        let mut guard = self.lock();
        // Read once: the record of the end and the tail counters see the same estimate.
        let estimate = self.estimate_of(&guard, lease);
        let ended = self.ended_row(&guard, lease, (ended_at, estimate));
        let placer = self.placer();
        let State {
            book,
            records,
            runs,
            tail,
            ..
        } = &mut *guard;
        // A cancelled request leaves no trace: no mark, and no record of its failures, so
        // the id names a fresh request if it is used again. The placer takes it off its
        // wait list and forgets it with the rest.
        let cancelled = placer.whole_cancel_lease(lease);
        cancelled.iter().for_each(|id| placer.whole_forget(id));
        runs.stop_lease(lease);
        if let Observed::Present { host } = book.read(lease) {
            let record = records.get_mut(&host);
            record.into_iter().for_each(|record| {
                // A lease ends: the error of its estimate (read once, above), at the time of
                // this call on the registry's clock. The host keeps a bound and a start for
                // each lease it started, so the stop of any of them counts. A lease put back
                // after a restart has neither unless a source of lives was set when it was
                // put back (see `seat`), so without one its stop counts nothing.
                let bounds = record.bounds.iter();
                let started = bounds.filter(|bound| bound.lease == *lease);
                started.for_each(|_| tail.ended(ended_at, estimate));
                record.exclusive = record.exclusive.stopped();
            });
        }
        let read_back = book.stop(lease);
        // The placer lets the lease go as its request's when it is told of its end.
        placer.ended(*lease);
        drop(guard);
        self.log_ended(ended);
        read_back
    }

    /// Binds each restored waiting request of `held` to the lease the lease file keeps it
    /// with, so that a stop of that lease cancels it even before the lease has presented it
    /// again. Call it once at a start, after [`Registry::restore_reservations`]. A request
    /// that is not waiting is left alone. How many were bound.
    pub fn bind_waiting(&self, held: &[(LeaseId, RequestId)]) -> usize {
        let _state = self.lock();
        let placer = self.placer();
        let bound = held
            .iter()
            .filter(|(lease, id)| placer.whole_bind(id, *lease));
        bound.count()
    }

    /// Whether the whole-node request lease `lease` presented last waits for a host
    /// (see [`js_provider::Pool::waits`]).
    pub(crate) fn waits(&self, lease: &LeaseId) -> bool {
        let _state = self.lock();
        self.placer().whole_waiter_of(lease).is_some()
    }

    /// Ends every committed whole-node lease that was not reported ready within
    /// `prepare_timeout` of its commit, as a failed prepare: the lease then reads
    /// `Absent`, so the lease table drops it by its ordinary path. A silent agent and a
    /// host gone lost are overdue the same as a slow one. The lease table's tick reads
    /// every pending lease through [`crate::RegisteredProvider`], which advances first;
    /// this does the same for a caller with a clock of its own.
    pub fn advance(&self) {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        self.advance_at(&mut state, now);
        self.release(state);
    }

    /// `Present` only while the host holding the lease is live and, for a whole-node
    /// lease, ready; a lost or awaiting host, and a whole-node host not yet ready, is
    /// `Unknown`, never `Absent`, so the booking stays.
    pub(crate) fn observe(&self, lease: &LeaseId) -> Observed {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        self.advance_at(&mut state, now);
        let observed = match state.book.read(lease) {
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
        };
        self.release(state);
        observed
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
    /// lease's start and bound. A placer is told of the lease put back, with `times`
    /// whether or not a source of lives is set ([`crate::Placer::seated`]). See
    /// [`crate::RegisteredProvider::seat_booked`].
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
                    queue: Queue::restored(self.whole_admission()),
                });
            });
        }
        let whole = matches!(self.shared.config.class().admission(), Admission::Whole);
        // The placer books the lease as the registry does from now on, as it was started
        // and bound: it drains by bounds with or without a source of lives.
        let placer = Some(self.placer()).filter(|_| seated.is_ok());
        let told = placer.zip(book.held(&lease)).map(|(placer, held)| {
            let seated = SeatedLease {
                host: host.clone(),
                held: held.clone(),
                times: times.map(|(started_at, ends_at)| LeaseTimes {
                    started_at,
                    ends_at,
                }),
                whole_node: whole & held.whole,
            };
            (placer, seated)
        });
        told.into_iter()
            .for_each(|(placer, seated)| placer.seated(&seated));
        // A restored whole-node booking is committed and not ready: the agent's
        // preparation is in memory only, so it must report ready for this lease again
        // before the lease reads present, a run is queued or the host books to anyone.
        // A host that lists held runs is ready: only the ready lease could have runs
        // (see `RegisteredProvider::seat` for where that fails).
        if let (Ok(()), true) = (&seated, whole) {
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                let resumed = !record.held.is_empty();
                record.exclusive = Exclusive::seated(lease, now, resumed);
            });
        }
        seated
    }

    /// Tells the placer that `host` said hello, listed in the book as
    /// `shape`: the pool's booked shape with what of the host's claim the pool places by
    /// (`PoolConfig::list_claimed`).
    pub(super) fn tell_up(&self, host: &HostId, shape: &Shape) {
        self.placer().host_up(host, shape);
    }

    /// Tells the placer that `host` is no longer listed.
    pub(super) fn tell_gone(&self, host: &HostId) {
        self.placer().host_gone(host);
    }

    /// Tells the placer that a heartbeat of `host` was accepted, with
    /// the reading it brought ([`crate::Placer::heard`]).
    pub(super) fn tell_heard(&self, host: &HostId, reading: Option<&NodeLoad>) {
        self.placer().heard(host, reading);
    }

    /// Tells the placer that the agent reported `lease` ready on a clean
    /// host committed to it ([`crate::Placer::prepare_ready`]).
    pub(super) fn tell_ready(&self, lease: &LeaseId) {
        self.placer().prepare_ready(lease);
    }

    /// Tells the placer that the host of `lease`, ready, said hello again,
    /// so the lease waits for ready again ([`crate::Placer::prepare_again`]).
    pub(super) fn tell_again(&self, lease: &LeaseId) {
        self.placer().prepare_again(lease);
    }
}
