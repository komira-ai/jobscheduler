//! The server's pass over the queued jobs ([`Agents::place_jobs`]): it reads the hosts the
//! registry would let take a job now and whose last job listing was clean, each with the
//! network ceiling its hello advertises and whether it reported that it runs host jobs, the
//! hosts that said hello to this server, and the pool's whole-node settings, and hands them to
//! the placer's pass
//! ([`SchedPlacer::place_jobs_with`](super::place::SchedPlacer::place_jobs_with), whose
//! module, [`js_sched_placer::engine::jobs`], says what a pass does, and
//! [`js_sched_placer::engine::whole_jobs`] what it does for the jobs that book the whole
//! host).
//!
//! A job with host isolation taints the host it is placed on. The placement commit writes
//! the taint row of the job store; right after it, still inside the placer's pass, the taint
//! is put on the registry's record of the host ([`js_registry::Registry::taint_host`]: reason
//! [`HOST_JOB_TAINT`], run id the job id), which closes the host to every other job; the pass
//! returns the hosts it tainted so that the caller writes their bindings
//! ([`Agents::keep_taints`]). One pass of the server runs at a time ([`Agents::placing`]),
//! from its read of the open hosts to its last taint, so a pass never offers a host that an
//! earlier pass has tainted.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use js_agent_wire::Taint;
use js_jobs::{HOST_JOB_TAINT, JobId};
use js_lease::Clock;
use js_registry::{Registry, WholeNodeConfig};
use js_sched::ceiling::ceiling_of;
use js_types::HostId;
use tracing::warn;

use super::engine::jobs::{JobHosts, JobStarts, OpenHost, open_to_jobs};
use crate::admit::reports_host_jobs;
use crate::agents::Agents;
use crate::metrics::PassSink;

/// The [`JobStarts`] of the server's pass: a start is built from the job table on the host's
/// next heartbeat, so a placement hands nothing on; a taint is put on the registry's record of
/// the host as soon as its placement is committed, and the host kept when that record had no
/// taint before.
struct Tainted<'a> {
    /// The registry the taints are put on.
    registry: &'a Registry,
    /// The hosts this pass tainted, in the order of their commits.
    hosts: Mutex<Vec<HostId>>,
}

impl JobStarts for Tainted<'_> {
    fn placed(&self, _: &JobId, _: &HostId) {}

    fn tainted(&self, host: &HostId, job: &JobId) {
        let taint = Taint {
            reason: HOST_JOB_TAINT.to_owned(),
            run_id: Some(job.to_string()),
            check: None,
        };
        let fresh = self.registry.taint_host(host, taint);
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        hosts.extend(fresh.then(|| host.clone()));
    }
}

impl Agents {
    /// The whole-node rules a whole-host job's prepare runs under: the pool's settings, or,
    /// for a pool that has none, the defaults of a dedicated pool of one host, as a lease's
    /// prepare takes them.
    pub(crate) fn whole_rules(&self) -> WholeNodeConfig {
        let settings = self.registry.whole_node().cloned();
        settings.unwrap_or_else(|| WholeNodeConfig::dedicated(1))
    }

    /// One pass over the queued jobs
    /// ([`SchedPlacer::place_jobs_with`](super::place::SchedPlacer::place_jobs_with)) on the
    /// hosts the registry would let take a job now and whose last job listing was clean
    /// ([`Agents::listing_clean`]), when this server holds the job tables and its registry
    /// has the placer; otherwise nothing (an agent side a test built without a placer
    /// places no job). The hosts that said hello to this server (those the registry
    /// holds a claim of) and the pool's whole-node settings go with them: a queued job that
    /// books the whole host is marked, placed and prepared only under those settings. A failed
    /// read of the tables is logged; the next pass reads them again, unless the store said it
    /// has a newer writer, which trips the fence ([`crate::fence`]).
    ///
    /// A host is offered to a job with host isolation only when its hello reported
    /// `host-jobs`. Each host such a job was placed on is tainted in the registry
    /// (`host-job`, the job's id as the run id) as soon as the placement is committed, and
    /// returned: the caller writes the bindings of the hosts returned
    /// ([`Agents::keep_taints`]). A host the registry no longer holds, or that was tainted
    /// before, is not returned.
    pub(crate) fn place_jobs(&self) -> Vec<HostId> {
        let (Some(jobs), Some(placer)) = (&self.jobs, &self.placer) else {
            return Vec::new();
        };
        // Held from the read of the hosts to the last taint: a pass that read the hosts
        // before another pass tainted one would offer it again.
        let _placing = self.placing.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.clock.now();
        let views = self.registry.hosts();
        let hello: BTreeSet<_> = views
            .iter()
            .filter(|view| view.claim.is_some())
            .map(|view| view.id.clone())
            .collect();
        // A host whose last job listing named a job it holds no live allocation for is
        // still running that job: its capacity is not counted until a listing is clean.
        let open = views.into_iter();
        let open = open.filter(|view| open_to_jobs(view, now) && self.listing_clean(&view.id));
        let open = open.map(|view| {
            // The ceiling and the host-jobs capability are read again on every pass, from the
            // hello the host last said; a host whose hello is not held (after a restart)
            // advertises no network and no host jobs.
            let capabilities = view.claim.as_ref().map(|claim| claim.capabilities());
            let offer = OpenHost {
                ceiling: capabilities.map(ceiling_of).unwrap_or_default(),
                host_jobs: capabilities.is_some_and(reports_host_jobs),
            };
            (view.id, offer)
        });
        let hosts = JobHosts {
            open: &open.collect(),
            hello: &hello,
            whole: self.registry.whole_node(),
        };
        let starts = Tainted {
            registry: &self.registry,
            hosts: Mutex::default(),
        };
        let sink = PassSink(self.meter().sink());
        if let Err(error) = placer.place_jobs_with(jobs, &hosts, (&starts, &sink)) {
            self.fence.note(&error);
            warn!(%error, "the queued jobs could not be read; the next pass places them");
        }
        let hosts = starts.hosts.into_inner();
        hosts.unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "jobs_ports_tests.rs"]
mod ports_tests;

#[cfg(test)]
#[path = "jobs_taint_tests.rs"]
mod taint_tests;
