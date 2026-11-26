//! The listing's view of the hosts ([`Registry::hosts`]): what each host is, read from the
//! parts of the registry's state that say it.

use js_types::Timestamp;
use js_types::{HostId, Shape};

use super::marks::Leaving;
use super::record::Record;
use super::{HostState, HostView, Registry, State};
use crate::config::placement_shape;

impl HostView {
    /// The shape the registry places the host on: [`Self::shape`], what the host is booked
    /// at (what it reports, capped by the pool's numbers), with the `accelerator.*`
    /// capabilities of the host's validated [`Self::claim`] in place of the pool's, so its
    /// real family, variant facts and measured memory are matched; with
    /// [`Self::by_hello`], also with the claim's other capabilities that [`Self::shape`]
    /// does not name. Admission reads a host's facts from it.
    #[must_use]
    pub fn placement_shape(&self) -> Shape {
        let percent = self.memory.percent();
        placement_shape(&self.shape, self.claim.as_ref(), self.by_hello, percent)
    }
}

impl Registry {
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
        record.silent_since(now, self.pool_of(record).grace_ms())
    }

    pub(super) fn host_views(&self, state: &State, now: Timestamp) -> Vec<HostView> {
        let view = self.viewer(now);
        state
            .records
            .iter()
            .map(|(id, record)| view(id, record))
            .collect()
    }

    /// The listing's view of one host, given the parts of the state that it reads: the
    /// shape and the memory are what the host reported capped by its pool's, and the timing
    /// is its pool's.
    pub(super) fn viewer(&self, now: Timestamp) -> impl Fn(&HostId, &Record) -> HostView + '_ {
        move |id, record| {
            let pool = self.pool_of(record);
            let state = record.state(now, pool.grace_ms());
            HostView {
                id: id.clone(),
                state,
                load: record
                    .load
                    .latest
                    .clone()
                    .filter(|_| state != HostState::Lost),
                load_seconds: match state {
                    HostState::Lost => Default::default(),
                    _ => record.load.seconds.clone(),
                },
                phase: record.phase,
                lease: None,
                leases: Vec::new(),
                shape: pool.fit_of(record.claim.as_ref()),
                memory: pool.headroom_of(record.claim.as_ref()),
                claim: record.claim.clone(),
                by_hello: pool.places_by_hello(),
                labels: record.labels.clone(),
                agent_version: record.agent_version.clone(),
                epoch: record.epoch,
                heard_since_hello: record.session.is_none() | (record.beats > 0),
                releasing: !record.quiet(),
                taint: record.taint.clone(),
                mark: record.leaving.map(Leaving::mark),
                reserved_for: None,
                drain_deadline: None,
                cordon_until: record.cordon.filter(|until| *until > now),
                cordon_holder: record.cordon_holder_at(now),
            }
        }
    }
}
