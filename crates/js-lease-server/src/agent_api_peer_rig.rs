//! A second server over the job tables of a first, and rows written by hand, for the tests of
//! what a server reads of the `node/` rows another server's hellos left. Both servers read one
//! manual clock, which the first rig's [`Rig::advance`] moves.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_agent_wire::{HOST_JOBS_CAPABILITY, Phase};
use js_jobs::{NodeHello, Retention};
use js_types::{HostId, Resources, Shape};

use super::job_rig::{OneClock, Rig, rig_on};
use crate::clock::SharedClock;
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;

/// Limits long enough that no job of a test runs out of time.
pub(crate) const NEVER: JobLimits = JobLimits {
    max_queue_wait: std::time::Duration::from_secs(365 * 24 * 3600),
    start_timeout: std::time::Duration::from_secs(365 * 24 * 3600),
};

/// A placing server named `name` over the job tables and the clock of `first`, whose registry
/// holds no host: the hosts that said hello to `first` are known to it by their rows alone.
pub(crate) fn peer_of(name: &str, first: &Rig) -> Rig {
    let extra = super::job_rig::Extra {
        tables: Some((Arc::clone(&first.store), Arc::clone(&first.jobs))),
        ..Default::default()
    };
    let clock = Arc::clone(&first.clock);
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let kept = (NEVER, Retention::DEFAULT);
    rig_on(name, (true, true), kept, (clock, shared), extra)
}

/// A shape of eight cores and 16 GiB with the capabilities `caps`.
pub(crate) fn shape_with(caps: &[(&str, &str)]) -> Shape {
    let resources = Resources::new(8000, 17_179_869_184, 0, None).unwrap();
    let caps: BTreeMap<String, String> = caps
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    Shape::new(resources, caps)
}

/// What a row written by [`write_row`] announces.
pub(crate) struct Row<'a> {
    /// The host.
    pub(crate) host: &'a str,
    /// The pool the host joined.
    pub(crate) pool: &'a str,
    /// Whether the host's claim reports `host-jobs`; a host with no claim when `None`.
    pub(crate) host_jobs: Option<bool>,
    /// Whether the row has a booked shape.
    pub(crate) booked: bool,
}

/// Writes the `node/` row of a host as its first hello would, in `phase`, the way a server
/// that took the hello does, without a registry.
pub(crate) fn write_row(jobs: &ServerJobs, row: &Row<'_>, phase: Phase) {
    let claim = row.host_jobs.map(|reports| {
        let value = if reports { "true" } else { "false" };
        shape_with(&[("arch", "x86_64"), (HOST_JOBS_CAPABILITY, value)])
    });
    let hello = NodeHello {
        session: None,
        pool: row.pool.to_owned(),
        claim,
        shape: row.booked.then(|| shape_with(&[("arch", "x86_64")])),
        labels: BTreeMap::from([("zone".to_owned(), "z1".to_owned())]),
        phase,
        agent_process: None,
    };
    jobs.hello_node(&HostId::new(row.host).unwrap(), &hello)
        .unwrap();
}
