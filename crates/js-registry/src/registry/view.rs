//! The listing's view of the hosts ([`Registry::hosts`], [`Registry::snapshot`]): what each
//! host is, read from the parts of the registry's state that say it; and what the leases on
//! each host book there ([`Registry::held`]).

use std::collections::BTreeMap;

use js_lease::Timestamp;
use js_provider::{Held, HostBook};
use js_types::{HostId, Shape};

use super::record::Record;
use super::{HostMark, HostState, HostView, Registry, State};
use crate::config::placement_shape;
use crate::runs::Runs;
use crate::whole::reserve::Marks;

impl HostView {
    /// The shape the registry places the host on: the pool's booked [`Self::shape`], with
    /// the `accelerator.*` capabilities of the host's validated [`Self::claim`] in place of
    /// the pool's, so its real family, variant facts and measured memory are matched while
    /// the pool's numbers stay the booking limits. Admission reads a host's facts from it.
    #[must_use]
    pub fn placement_shape(&self) -> Shape {
        placement_shape(&self.shape, self.claim.as_ref())
    }
}

impl Registry {
    /// The leases booked on each host the registry holds, by host in id order, each host's
    /// in the order they were booked, with what each books there: its own resources, or the
    /// whole shape for a lease that books the whole host. A host with no lease is left out.
    /// It is what a listing of the hosts shows as booked on them; it books nothing.
    pub fn held(&self) -> BTreeMap<HostId, Vec<Held>> {
        let state = self.lock();
        let hosts = state.book.candidates().into_iter();
        let booked = hosts.filter(|host| !host.leases.is_empty());
        booked.map(|host| (host.host, host.leases)).collect()
    }

    /// Since when `host` has been silent past the grace (design section 6.3): the instant
    /// the grace ran out after it was last heard (for a host not heard since the registry was
    /// seated, after it was seated); `None` while it is heard within the grace, and for a host
    /// the registry does not hold. It changes no host's record and ends no work; like every
    /// locking call it first runs the housekeeping, which forgets the hosts lost past the
    /// retention that hold nothing.
    pub fn silent_since(&self, host: &HostId) -> Option<Timestamp> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        let record = state.records.get(host)?;
        record.silent_since(now, self.grace_ms())
    }

    pub(super) fn host_views(&self, state: &State, now: Timestamp) -> Vec<HostView> {
        let view = self.viewer((&state.book, &state.runs, &state.marks), now);
        state
            .records
            .iter()
            .map(|(id, record)| view(id, record))
            .collect()
    }

    /// The listing's view of one host, given the parts of the state that it reads. The
    /// reads that are the same for every host are made once, here.
    pub(super) fn viewer<'a>(
        &'a self,
        (book, runs, marks): (&'a HostBook, &'a Runs, &'a Marks),
        now: Timestamp,
    ) -> impl Fn(&HostId, &Record) -> HostView + 'a {
        let booked = book.booked();
        let every = book.leases();
        let shape = self.shared.config.fit();
        let memory = self.shared.config.memory_headroom();
        let marks = marks.reservations();
        move |id, record| {
            let set_aside = |id: &HostId| marks.iter().find(|mark| mark.host == *id);
            HostView {
                id: id.clone(),
                state: record.state(now, self.grace_ms()),
                load: record
                    .load
                    .latest
                    .clone()
                    .filter(|_| record.state(now, self.grace_ms()) != HostState::Lost),
                phase: record.phase,
                lease: booked.get(id).copied(),
                leases: every.get(id).cloned().unwrap_or_default(),
                shape: shape.clone(),
                memory,
                claim: record.claim.clone(),
                labels: record.labels.clone(),
                agent_version: record.agent_version.clone(),
                epoch: record.epoch,
                heard_since_hello: record.session.is_none() | (record.beats > 0),
                releasing: !booked.contains_key(id)
                    & !(record.quiet(runs, id) & record.exclusive.is_free()),
                taint: record.taint.clone(),
                mark: match (record.leaving, set_aside(id), record.exclusive.held_since()) {
                    (Some(leaving), _, _) => Some(leaving.mark()),
                    (None, Some(_), _) => Some(HostMark::Reserved),
                    (None, None, Some(_)) => Some(HostMark::Held),
                    (None, None, None) => None,
                },
                reserved_for: set_aside(id).map(|mark| mark.request.clone()),
                drain_deadline: set_aside(id)
                    .filter(|mark| !mark.assigned)
                    .map(|mark| mark.deadline),
                cordon_until: record.cordon.filter(|until| *until > now),
            }
        }
    }
}
