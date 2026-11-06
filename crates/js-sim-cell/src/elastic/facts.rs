//! What the cell observes of an elastic pool's cloud, for the invariants: the record of
//! the server's calls to it, the scheduler's machines it runs now, and which host each
//! machine registers as.

use std::collections::{BTreeMap, BTreeSet};

use js_types::HostId;
use js_types::sched::FarmTime;

use super::record::{CloudCall, CloudEvent};

/// The cloud of the cell's elastic pool, as the cell observed it at one virtual time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudFacts {
    /// The pool's name.
    pub pool: String,
    /// The pool's `max_instances`.
    pub max_instances: u64,
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
        };
        self.machines.insert(id.to_owned(), machine);
        self.running.insert(id.to_owned());
        self
    }
}
