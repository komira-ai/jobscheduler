//! The provider side of the registry: the booking, stop and observation calls the lease
//! table makes through [`crate::RegisteredProvider`].

use std::collections::BTreeMap;

use js_pool_static::SeatError;
use js_provider::{HostCapacity, Observed};
use js_types::{Admission, HostId, LeaseId};

use super::{HostState, Record, Registry, State};
use crate::credential::Binding;
use crate::whole::exclusive::Exclusive;

impl Registry {
    /// Frees the booking at once, as the book does, and asks every live run of the lease
    /// to stop. The host takes no new lease until a heartbeat lists none of those runs.
    /// After a whole-node lease it takes none until a second heartbeat has followed the
    /// stop (the first may have crossed it) and the last one shows it serving with
    /// nothing held (design `whole-node.md` section 8).
    pub(crate) fn stop(&self, lease: &LeaseId) -> Observed {
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            requests,
            ..
        } = &mut *guard;
        runs.stop_lease(lease);
        requests.release(lease);
        if let Observed::Present { host } = book.read(lease) {
            let record = records.get_mut(&host);
            record.into_iter().for_each(|record| {
                record.exclusive = record.exclusive.stopped();
            });
        }
        book.stop(lease)
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
    /// first hello bind it. See [`crate::RegisteredProvider::seat`].
    pub(crate) fn seat(
        &self,
        host: &HostId,
        lease: LeaseId,
        binding: Option<Binding>,
    ) -> Result<(), SeatError> {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State { book, records, .. } = &mut *guard;
        let seated = match book.seat(host, lease) {
            Err(SeatError::Unlisted(_)) => {
                book.insert_host(host.clone(), self.shared.config.booked().clone());
                records.insert(host.clone(), Record::awaiting(binding, now));
                book.seat(host, lease)
            }
            settled => settled,
        };
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
}
