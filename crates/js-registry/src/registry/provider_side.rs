//! The provider side of the registry: the booking, stop and observation calls the lease
//! table makes through [`crate::RegisteredProvider`].

use std::collections::{BTreeMap, BTreeSet};

use js_pool_static::{HostBook, SeatError};
use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, HostId, LeaseId, ResourceClass, Shape};

use super::{HostState, Record, Registry, State};
use crate::credential::Binding;
use crate::whole::exclusive::Exclusive;
use crate::whole::placement::{self, Asking};
use crate::whole::requests::{RequestKey, Rules, whole_settings};
use crate::whole::reserve::{self, Pass};

impl Registry {
    /// Books `spec`'s lease as [`js_pool_static::HostBook`] does. A `Whole` class is
    /// committed, not granted: the booking is made only on a host that is bookable (live,
    /// serving, clean), quiet (no held run, none live here) and clear of another
    /// whole-node lease or a restore, and the answer is `Unknown` so the lease stays
    /// pending. A request that used up its tries is answered
    /// `Refused(ExclusiveNotReady)` once (see [`Registry::not_ready`]). One request id holds at most one whole host: a start
    /// of an id that another lease of this pool holds, committed or ready, books nothing
    /// and is answered `NoCapacity` (it waits) until that lease ends. Only a `Whole` class
    /// is held to those rules; a `BookAll` start never reads a request's record. It reads `Present` once the agent reports the host ready
    /// ([`Registry::heartbeat`]), and a repeated start answers `Unknown` until then and
    /// `Started` after.
    pub(crate) fn start(&self, classes: &[ResourceClass], spec: &StartSpec) -> StartAck {
        let now = self.shared.clock.now();
        let grace = self.grace_ms();
        let mut guard = self.lock_at(now);
        let state = &mut *guard;
        let lease = spec.lease();
        let key = RequestKey::of(spec);
        let whole = matches!(spec.class().admission(), Admission::Whole);
        let held = state.book.read(&lease);
        let pending = match &held {
            Observed::Present { host } => state
                .records
                .get(host)
                .is_some_and(|record| record.exclusive.awaits_ready()),
            Observed::Absent | Observed::Unknown => false,
        };
        if pending {
            return StartAck::Unknown;
        }
        if whole {
            // One request id holds at most one whole host: a start of it under another
            // lease waits until the lease that holds it ends.
            let id_held = spec.request_id().is_some();
            if id_held & state.requests.held_by_other(&key, &lease) {
                return StartAck::Refused(RefusalCode::NoCapacity);
            }
            // A request that used up its tries is not placed again.
            if state.requests.start(&key, now) {
                return StartAck::Refused(RefusalCode::ExclusiveNotReady);
            }
        }
        let gate = HostBook::class_gate(classes, spec);
        let fresh = !matches!(held, Observed::Present { .. });
        // The pool's shape fits the request or no host ever will: the book answers
        // `NoFit` when no listed host fits, and every host is booked as the pool's shape,
        // so that happens for a request the pool's shape fits only when no host is listed
        // yet. A host could still join, which is capacity, not fit.
        let floor = self.shared.config.shape();
        let wanted = spec.request();
        let fits_pool = floor.satisfies(&wanted.resources, &wanted.capabilities);
        // A whole-node request that fits the pool is refused for lack of a host only when
        // no host could ever be given whole; otherwise it waits, if it has an id.
        let listed = state.book.hosts();
        let tainted = state
            .records
            .iter()
            .filter(|(_, record)| record.taint.is_some())
            .map(|(host, _)| host.clone())
            .collect::<BTreeSet<_>>();
        let asking = Asking {
            spec,
            key: &key,
            hosts: &listed,
            tainted: &tainted,
        };
        let capable = placement::capable(&asking, &state.requests);
        let queueing = [whole, fresh, fits_pool, gate.is_none()];
        let regain = Rules::of(&self.shared.config).regain_grace_ms;
        let place = spec
            .request_id()
            .filter(|_| queueing.into_iter().all(|ok| ok));
        let verdict = place.map(|id| state.waiting.present(id, wanted, capable, now, regain));
        let settings = whole_settings(&self.shared.config);
        let hosts = self.host_facts(state, now);
        self.place_waiting(state, &settings, &hosts, now);
        let pass = Pass {
            now,
            settings: &settings,
            hosts: &hosts,
        };
        let room = reserve::has_room(&state.marks, &pass, true);
        let State {
            book,
            records,
            runs,
            requests,
            waiting,
            marks,
            ..
        } = state;
        let eligible = |id: &HostId, _: &Shape| {
            let record = records.get(id);
            // Only a whole-node request is kept off a host its earlier tries failed on.
            let barred = whole & requests.excluded(&key, id, now);
            let bookable =
                record.is_some_and(|record| record.bookable(now, grace, record.quiet(runs, id)));
            let allowed = reserve::may_start(marks.holder(id), whole, spec.request_id(), room);
            [bookable, !barred, allowed].into_iter().all(|ok| ok)
        };
        let ack = book.start_where(spec, gate, &eligible);
        let started = matches!(ack, StartAck::Started { .. });
        let ended = matches!(ack, StartAck::Refused(RefusalCode::LeaseEnded));
        let commit = [whole, fresh].into_iter().all(|ok| ok);
        // A request that is placed, or whose lease had ended, waits no more.
        if [started & commit, ended].into_iter().any(|leaves| leaves) {
            spec.request_id().into_iter().for_each(|id| {
                waiting.cancel(id);
            });
        }
        if let (StartAck::Started { host }, true) = (&ack, fresh) {
            let end = now.plus(wanted.max_wall);
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                record.bound = Some((lease, end));
            });
        }
        if let (StartAck::Started { host }, true) = (&ack, commit) {
            // The host was eligible, so it has a record.
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                record.exclusive = Exclusive::Committed { lease, at: now };
            });
            requests.commit(lease, key);
            return StartAck::Unknown;
        }
        let declined = matches!(
            ack,
            StartAck::Refused(RefusalCode::NoFit | RefusalCode::NoCapacity)
        );
        match (ack, fits_pool, whole && declined) {
            (_, true, true) => placement::answer(verdict, capable),
            (StartAck::Refused(RefusalCode::NoFit), true, false) => {
                StartAck::Refused(RefusalCode::NoCapacity)
            }
            (other, _, _) => other,
        }
    }

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
                book.insert_host(host.clone(), self.shared.config.shape().clone());
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
