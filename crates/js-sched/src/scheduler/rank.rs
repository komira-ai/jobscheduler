//! Which machine provider each worker's host is of ([`crate::ProviderRank`]), as the caller
//! tells it.

use js_types::sched::WorkerId;

use super::Scheduler;
use crate::provider_rank::ProviderRank;

impl Scheduler {
    /// Tells the scheduler that the host of `worker` is of the provider `rank` names, or,
    /// with `None`, of no provider its platform lists. Kept across a registration or a change
    /// of capacity; not part of the log, so the caller tells it again after a restart.
    /// Ignored for a worker that never registered.
    ///
    /// Pass 1 takes, among the workers that could take a request now, those of the first
    /// provider in order, and the score (or, where none is read, the load) decides among them
    /// alone: the rank is the first key of the comparison, never a weight a lighter load can
    /// overcome. A worker of an earlier provider that is busy, draining, cordoned or too small
    /// is not among those workers, so it never keeps a request off a free worker of a later
    /// one. A whole-machine request takes, of the quiet workers, one of the first provider;
    /// the drain order of its mark ranks by tail first, and the provider tells apart only
    /// workers that cost the same to drain ([`js_whole::cost::drain_cost`]).
    pub fn set_provider_rank(&mut self, worker: &WorkerId, rank: Option<ProviderRank>) {
        if let Some(w) = self.workers.get_mut(worker) {
            w.rank = rank;
        }
    }

    /// The provider the host of `worker` is of ([`Scheduler::set_provider_rank`]); `None` when
    /// it was told none, or the worker is not registered.
    #[must_use]
    pub fn provider_rank(&self, worker: &WorkerId) -> Option<&ProviderRank> {
        self.workers.get(worker).and_then(|w| w.rank.as_ref())
    }
}
