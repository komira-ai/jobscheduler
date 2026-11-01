//! The start path of the registry: the guards a start meets, the pass that places waiting
//! whole-node requests, and the host facts that pass reads.

use std::collections::BTreeSet;

use js_elastic::Demand;
use js_lease::Timestamp;
use js_pool_static::HostBook;
use js_provider::{Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, HostId, LeaseId, ResourceClass, Shape};

use super::{HostState, Record, Registry, State};
use crate::whole::caps::Capability;
use crate::whole::cost::HostLoad;
use crate::whole::exclusive::Exclusive;
use crate::whole::placement::{self, Asking};
use crate::whole::requests::{RequestKey, Rules, whole_settings};
use crate::whole::reserve::{self, HostFacts, Pass};
use crate::whole_config::WholeNodeConfig;

/// The end bound the host knows among `leases`: its latest started lease's, when that
/// lease is booked here. The other leases of a host that carries several have none.
fn known_end(record: &Record, leases: &[LeaseId]) -> Option<(LeaseId, Timestamp)> {
    record.bound.filter(|(held, _)| leases.contains(held))
}

impl Registry {
    /// Every host as the reservation pass reads it, in id order. Every host is booked as
    /// the pool's shape.
    pub(super) fn host_facts(&self, state: &State, now: Timestamp) -> Vec<HostFacts> {
        let booked = state.book.leases();
        let grace_ms = self.grace_ms();
        let facts = state.records.iter().map(|(id, record)| {
            let live = record.state(now, grace_ms) == HostState::Live;
            // A host being scaled away is leaving the pool: no reservation drains it.
            let capability = match (record.taint.is_some(), record.leaving.is_some(), live) {
                (true, _, _) => Capability::Tainted,
                (false, true, _) => Capability::Releasing,
                (false, false, true) => Capability::Capable,
                (false, false, false) => Capability::Lost,
            };
            let quiet = record.quiet(&state.runs, id);
            let leases = booked.get(id).map(Vec::as_slice).unwrap_or_default();
            let ends = known_end(record, leases);
            HostFacts {
                id: id.clone(),
                capability,
                open: record.bookable(now, grace_ms, quiet) & leases.is_empty(),
                held: record.exclusive.held_since(),
                load: HostLoad {
                    id: id.to_string(),
                    leases: leases.len(),
                    end_bounds: ends.map(|(_, end)| end.millis()).into_iter().collect(),
                    est_ends: Vec::new(),
                },
            }
        });
        facts.collect()
    }

    /// Sets hosts aside for the waiting requests, in queue order (see
    /// [`crate::whole::reserve`]). Gives up and commits nothing else, and stops nothing.
    pub(super) fn place_waiting(
        &self,
        state: &mut State,
        settings: &WholeNodeConfig,
        hosts: &[HostFacts],
        now: Timestamp,
    ) {
        let State {
            marks,
            waiting,
            requests,
            dropped,
            ..
        } = state;
        let pass = Pass {
            now,
            settings,
            hosts,
        };
        *dropped += reserve::advance(marks, waiting, requests, pass);
    }

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
        let floor = self.shared.config.booked();
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
        // A host that could be launched counts as one that could ever serve the request.
        let launch = state
            .launchable
            .as_ref()
            .is_some_and(|launchable| launchable.allows(&Demand::from_request(wanted)));
        let capable = placement::capable(&asking, &state.requests) | launch;
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
}
