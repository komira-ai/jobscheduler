//! The listing's view of the hosts ([`Registry::hosts`], [`Registry::snapshot`]): what each
//! host is, read from the parts of the registry's state that say it.

use js_lease::Timestamp;
use js_provider::HostBook;
use js_types::HostId;

use super::record::Record;
use super::{HostMark, HostState, HostView, Registry, State};
use crate::runs::Runs;
use crate::whole::reserve::Marks;

impl Registry {
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
