//! What the cell observes of an elastic pool's cloud, for the invariants: the record of
//! the server's calls to it, what the wrapper saw at each acquire, the scheduler's machines
//! it runs now, and which host each machine registers as.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_types::HostId;
use js_types::sched::FarmTime;

use super::DEFAULT_QUOTA_BACKOFF;
use super::record::{AcquireSeen, CloudCall, CloudEvent};

/// One cloud of the cell's elastic pool, as the cell observed it at one virtual time. A
/// server of [`crate::ServerSpec::ordered`] has one for each of its machine providers that
/// launch, in the order pass 2 asks them; `pool` is then the provider's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudFacts {
    /// The pool's name: for a server of [`crate::ServerSpec::ordered`], the machine
    /// provider's.
    pub pool: String,
    /// The pool's `max_instances`: the provider's share of the pool.
    pub max_instances: u64,
    /// The provider's place among the pool's launching providers, from 0, first asked.
    pub rank: usize,
    /// The provider's `quota_backoff`.
    pub quota_backoff: Duration,
    /// The provider's offers, by id and zone.
    pub offers: Vec<(String, Option<String>)>,
    /// What the recording wrapper saw at each acquire, in the order made; only ever
    /// appended.
    pub seen: Vec<AcquireSeen>,
    /// The machines of this scheduler (launched by an acquire, not foreign) that the cloud
    /// runs now, by the cloud's ids: read from the fake, not from the record.
    pub running: BTreeSet<String>,
    /// Every call the server made to the cloud and every change of the pool's brake the
    /// cell read, in order, from the recording wrapper; only ever appended.
    pub calls: Vec<CloudCall>,
    /// Each machine the cell saw the cloud launch for this scheduler, by the cloud's id.
    pub machines: BTreeMap<String, Machine>,
}

/// One machine the cloud launched for this scheduler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    /// Its number in the cell, in launch order from zero: how a trace line names it.
    pub number: usize,
    /// The host id its agent registers under (the name the scheduler gave it).
    pub host: HostId,
    /// Whether the server's listing has shown its host.
    pub joined: bool,
    /// The acquire id it was launched for, as the cloud holds it; `None` where a case did
    /// not say.
    pub acquire: Option<String>,
}

impl Default for CloudFacts {
    fn default() -> Self {
        Self {
            pool: String::new(),
            max_instances: 0,
            rank: 0,
            quota_backoff: DEFAULT_QUOTA_BACKOFF,
            offers: Vec::new(),
            seen: Vec::new(),
            running: BTreeSet::new(),
            calls: Vec::new(),
            machines: BTreeMap::new(),
        }
    }
}

impl CloudFacts {
    /// The cloud of pool `pool` of at most `max_instances` machines, before any call.
    #[must_use]
    pub fn new(pool: &str, max_instances: u64) -> Self {
        Self {
            pool: pool.to_owned(),
            max_instances,
            ..Self::default()
        }
    }

    /// This cloud with `event` recorded at `at`, after the calls it has.
    #[must_use]
    pub fn with_call(mut self, at: FarmTime, event: CloudEvent) -> Self {
        self.calls.push(CloudCall { at, event });
        self
    }

    /// This cloud running machine `id` of this scheduler, registered as `host` and
    /// numbered after the machines it has, its host not yet listed.
    #[must_use]
    pub fn with_running(mut self, id: &str, host: HostId) -> Self {
        let number = self.machines.len();
        let machine = Machine {
            number,
            host,
            joined: false,
            acquire: None,
        };
        self.machines.insert(id.to_owned(), machine);
        self.running.insert(id.to_owned());
        self
    }

    /// This cloud, the `rank`th of its pool's launching providers (from 0) with a
    /// `quota_backoff` of `backoff`.
    #[must_use]
    pub fn ranked(mut self, rank: usize, backoff: Duration) -> Self {
        self.rank = rank;
        self.quota_backoff = backoff;
        self
    }

    /// This cloud, offering `offer` in `zone` after the offers it has.
    #[must_use]
    pub fn with_offer(mut self, offer: &str, zone: Option<&str>) -> Self {
        self.offers
            .push((offer.to_owned(), zone.map(str::to_owned)));
        self
    }

    /// This cloud with `seen` recorded after what it has.
    #[must_use]
    pub fn with_seen(mut self, seen: AcquireSeen) -> Self {
        self.seen.push(seen);
        self
    }

    /// This cloud having launched machine `id` for the acquire id `acquire`, registered as
    /// `host` and numbered after the machines it has, whether it still runs or not.
    #[must_use]
    pub fn with_launched(mut self, id: &str, host: HostId, acquire: &str) -> Self {
        let number = self.machines.len();
        let machine = Machine {
            number,
            host,
            joined: false,
            acquire: Some(acquire.to_owned()),
        };
        self.machines.insert(id.to_owned(), machine);
        self
    }
}
