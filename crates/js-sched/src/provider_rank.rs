//! The machine provider a worker's host came from, and where its platform lists that provider
//! (design section 5.6, steps 3 and 4): pass 1 places on the first provider in that order that
//! has a free capable host, and the score decides only among the hosts of that provider.
//!
//! The caller says which provider each worker's host is of
//! ([`crate::Scheduler::set_provider_rank`]); the scheduler never learns it from a host's
//! name, address or anything it reports. A worker the caller ranked nothing for ranks after
//! every ranked one, so a scheduler that is told no rank at all places as it always did.

use js_types::ProviderId;
use js_types::sched::WorkerId;

use crate::scheduler::Worker;

/// A machine provider and its position in the order its platform lists the providers, 0
/// first. Ranks order by position; the provider names whose position it is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProviderRank {
    position: u32,
    provider: ProviderId,
}

impl ProviderRank {
    /// The rank of `provider`, listed at `position` (0 first).
    #[must_use]
    pub const fn new(provider: ProviderId, position: u32) -> Self {
        Self { position, provider }
    }

    /// Where the platform lists the provider, 0 first.
    #[must_use]
    pub const fn position(&self) -> u32 {
        self.position
    }

    /// The provider.
    #[must_use]
    pub const fn provider(&self) -> &ProviderId {
        &self.provider
    }
}

impl Worker {
    /// Where this worker's provider comes in the order placement tries providers in: its
    /// rank's position, and a worker with no rank after every ranked one
    /// (`u32::MAX`, a position no platform lists).
    pub(crate) fn provider_order(&self) -> u32 {
        self.rank.as_ref().map_or(u32::MAX, ProviderRank::position)
    }
}

/// Of `items`, those whose worker (`name` of each) has the first provider in order among
/// them ([`Worker::provider_order`]), in the order given. Empty for no item.
pub(crate) fn of_first_provider<T>(
    workers: &std::collections::BTreeMap<WorkerId, Worker>,
    items: Vec<T>,
    name: impl Fn(&T) -> &WorkerId,
) -> Vec<T> {
    let order = |item: &T| {
        workers
            .get(name(item))
            .map_or(u32::MAX, Worker::provider_order)
    };
    let first = items.iter().map(order).min();
    items
        .into_iter()
        .filter(|item| Some(order(item)) == first)
        .collect()
}
