//! The server's pass over the queued jobs ([`Agents::place_jobs`], which the tick runs, and
//! [`Agents::place_jobs_after_beat`], which a heartbeat runs and which is skipped when nothing
//! the pass depends on changed since the last one): it reads the hosts it offers from the job
//! store before each pass ([`super::jobs_rows`]: every host whose `node/` row is live, whichever
//! server took its hello, each with the network ceiling its hello advertises, whether it
//! reported that it runs host jobs and the machine provider its binding names, with its position
//! in the Default platform's order, [`super::rank`]; a host of a provider the platform does not
//! list is not offered), the hosts heard from, and the pool's whole-node settings, and hands
//! them to the placer's pass
//! ([`SchedPlacer::place_jobs_with`](super::place::SchedPlacer::place_jobs_with), whose
//! module, [`js_sched_placer::engine::jobs`], says what a pass does, and
//! [`js_sched_placer::engine::whole_jobs`] what it does for the jobs that book the whole
//! host). Every placement is decided again in its own transaction, on the host's rows
//! ([`js_jobs::JobStore::place_claimed`]): its taint, its scale-down mark, its cordon and the
//! ports its live allocations hold.
//!
//! A job with host isolation taints the host it is placed on. The placement commit writes
//! the taint row of the job store; right after it, still inside the placer's pass, the taint
//! is put on the registry's record of the host ([`js_registry::Registry::taint_host`]: reason
//! [`HOST_JOB_TAINT`], run id the job id), which closes the host to every other job; the pass
//! returns the hosts it tainted, each with its job, so that the caller writes their bindings
//! ([`Agents::keep_taints`]). One pass of the server runs at a time ([`Agents::placing`]),
//! from its read of the open hosts to its last taint, so a pass never offers a host that an
//! earlier pass has tainted. A server that drains ([`crate::drain`]) runs no pass: a placement
//! already inside its transaction commits or rolls back.

use std::sync::{Mutex, PoisonError};

use js_agent_wire::Taint;
use js_jobs::{HOST_JOB_TAINT, JobId};
use js_registry::{Registry, WholeNodeConfig};
use js_types::Clock;
use js_types::HostId;
use tracing::warn;

use super::engine::jobs::{JobHosts, JobStarts, PassMetrics};
use crate::agents::Agents;
use crate::held_beat::Wakes;
use crate::metrics::PassSink;

/// The [`JobStarts`] of the server's pass: a start is built from the job table on the host's
/// heartbeat, or by its dispatch poll, so a placement wakes the host's heartbeat and the
/// host's dispatch poll this server holds, if it holds either ([`crate::held_beat`],
/// [`crate::dispatch_state`]), whose reply then carries the start; a taint is put on the
/// registry's record of the host as soon as its placement is committed, and the host kept when
/// that record had no taint before.
struct Tainted<'a> {
    /// The registry the taints are put on.
    registry: &'a Registry,
    /// The held heartbeats and the held dispatch polls a placement wakes.
    wakes: [&'a Wakes; 2],
    /// The hosts this pass tainted, each with its job, in the order of their commits.
    hosts: Mutex<Vec<(HostId, JobId)>>,
}

impl JobStarts for Tainted<'_> {
    fn placed(&self, _: &JobId, host: &HostId) {
        self.wakes.iter().for_each(|wakes| wakes.wake(host));
    }

    fn tainted(&self, host: &HostId, job: &JobId) {
        let taint = Taint {
            reason: HOST_JOB_TAINT.to_owned(),
            run_id: Some(job.to_string()),
            check: None,
        };
        let fresh = self.registry.taint_host(host, taint);
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        hosts.extend(fresh.then(|| (host.clone(), job.clone())));
    }
}

/// Whether a pass over the queued jobs runs whatever changed (the tick's) or only when
/// something it depends on changed (a heartbeat's).
#[derive(Debug, Clone, Copy)]
enum Pass {
    Always,
    IfChanged,
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
    /// hosts [`Agents::offered`] reads before the pass: every host whose `node/` row is live
    /// and every host this server's registry holds live, when nothing closes it and its last
    /// job listing was clean; when this server holds the job tables and its registry has the
    /// placer; otherwise nothing (an agent side a test built without a placer places no job).
    /// The hosts heard from (whose rows are live, or that said hello here) and the pool's
    /// whole-node settings go with them: a queued job that books the whole host is marked,
    /// placed and prepared only under those settings. A failed read of the hosts or the tables
    /// is logged; the next pass reads them again.
    ///
    /// A host is offered to a job with host isolation only when its hello reported
    /// `host-jobs`. Each host such a job was placed on is tainted in the registry
    /// (`host-job`, the job's id as the run id) as soon as the placement is committed, and
    /// returned with the job: the caller writes the bindings of the hosts returned
    /// ([`Agents::keep_taints`]). A host the registry does not hold, or that was tainted
    /// before, is not returned: its `taint/<host>` row stays, closing it to every server, until
    /// the pass of the server that holds it takes the taint on. The taint rows such a pass took
    /// on are returned too.
    ///
    /// The tick runs this pass whatever changed; a heartbeat runs
    /// [`Agents::place_jobs_after_beat`].
    pub(crate) fn place_jobs(&self) -> Vec<(HostId, JobId)> {
        self.pass_over_queue(Pass::Always)
    }

    /// The pass a heartbeat runs: [`Agents::place_jobs`], skipped when nothing it depends on
    /// changed since the last pass that ran
    /// ([`SchedPlacer::place_jobs_if_changed`](super::place::SchedPlacer::place_jobs_if_changed)):
    /// the job tables, the engine's picture of the hosts, the hosts offered with what each
    /// offers, the hosts heard from and what pass 2 told. The hosts are read from the rows and
    /// the registry first either way, so a changed row brings the pass back.
    pub(crate) fn place_jobs_after_beat(&self) -> Vec<(HostId, JobId)> {
        self.pass_over_queue(Pass::IfChanged)
    }

    /// [`Agents::place_jobs`], or [`Agents::place_jobs_after_beat`] as `pass` says. Nothing
    /// once the server drains ([`crate::drain`]): its hosts are leaving for another server,
    /// which places the queued jobs as they say hello there.
    fn pass_over_queue(&self, pass: Pass) -> Vec<(HostId, JobId)> {
        let (Some(jobs), Some(placer)) = (&self.jobs, &self.placer) else {
            return Vec::new();
        };
        if self.draining.begun() {
            return Vec::new();
        }
        // Held from the read of the hosts to the last taint: a pass that read the hosts
        // before another pass tainted one would offer it again.
        let _placing = self.placing.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.clock.now();
        let offered = match pass {
            Pass::Always => self.offered(jobs, placer, now),
            Pass::IfChanged => self.offered_after_beat(jobs, placer, now),
        };
        let offered = match offered {
            Ok(offered) => offered,
            Err(error) => {
                warn!(%error, "the hosts could not be read; the next pass places the queued jobs");
                return Vec::new();
            }
        };
        let hosts = JobHosts {
            open: &offered.open,
            hello: &offered.hello,
            whole: self.registry.whole_node(),
        };
        let starts = Tainted {
            registry: &self.registry,
            wakes: [&self.wakes, &self.dispatching.wakes],
            hosts: Mutex::new(offered.adopted),
        };
        let sink = PassSink(self.meter().sink());
        let sinks = (&starts as &dyn JobStarts, &sink as &dyn PassMetrics);
        let placed = match pass {
            Pass::Always => placer.place_jobs_with(jobs, &hosts, sinks).map(drop),
            Pass::IfChanged => placer.place_jobs_if_changed(jobs, &hosts, sinks).map(drop),
        };
        if let Err(error) = placed {
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

#[cfg(test)]
#[path = "jobs_cost_tests.rs"]
mod cost_tests;

#[cfg(test)]
#[path = "jobs_two_servers_tests.rs"]
mod two_servers_tests;

#[cfg(test)]
#[path = "jobs_rows_two_tests.rs"]
mod rows_two_tests;

#[cfg(test)]
#[path = "jobs_rows_tests.rs"]
mod rows_tests;
