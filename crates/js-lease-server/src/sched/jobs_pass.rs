//! The server's pass over the queued jobs ([`Agents::place_jobs`], which the tick runs, and
//! [`Agents::place_jobs_after_beat`], which a heartbeat runs and which is skipped when nothing
//! the pass depends on changed since the last one): it reads the hosts the
//! registry would let take a job now and whose last job listing was clean, each with the
//! network ceiling its hello advertises, whether it reported that it runs host jobs and the
//! machine provider of the Default platform its binding names, with its position in the
//! platform's order ([`super::rank`]; a host of a provider the platform does not list is not
//! offered), the
//! hosts that said hello to this server, and the pool's whole-node settings, and hands them to
//! the placer's pass
//! ([`SchedPlacer::place_jobs_with`](super::place::SchedPlacer::place_jobs_with), whose
//! module, [`js_sched_placer::engine::jobs`], says what a pass does, and
//! [`js_sched_placer::engine::whole_jobs`] what it does for the jobs that book the whole
//! host).
//!
//! A live host that holds a taint is offered to no job; the pass logs it once for its taint
//! ([`super::taint_skips::TaintSkips`]), so that a job that stays queued has its cause in the
//! journal.
//!
//! A job with host isolation taints the host it is placed on. The placement commit writes
//! the taint row of the job store; right after it, still inside the placer's pass, the taint
//! is put on the registry's record of the host ([`js_registry::Registry::taint_host`]: reason
//! [`HOST_JOB_TAINT`], run id the job id), which closes the host to every other job; the pass
//! returns the hosts it tainted so that the caller writes their bindings
//! ([`Agents::keep_taints`]). One pass of the server runs at a time ([`Agents::placing`]),
//! from its read of the open hosts to its last taint, so a pass never offers a host that an
//! earlier pass has tainted. A server that drains ([`crate::drain`]) runs no pass.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use js_agent_wire::Taint;
use js_jobs::{Capacity, HOST_JOB_TAINT, JobId};
use js_registry::{HostState, Registry, WholeNodeConfig};
use js_sched::ceiling::ceiling_of;
use js_types::Clock;
use js_types::{HostId, Shape};
use tracing::warn;

use super::engine::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics, open_to_jobs};
use super::rank::rank;
use crate::admit::reports_host_jobs;
use crate::agents::Agents;
use crate::config::CpuSlack;
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
    /// The hosts this pass tainted, in the order of their commits.
    hosts: Mutex<Vec<HostId>>,
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
        hosts.extend(fresh.then(|| host.clone()));
    }
}

/// Whether a pass over the queued jobs runs whatever changed (the tick's) or only when
/// something it depends on changed (a heartbeat's).
#[derive(Debug, Clone, Copy)]
enum Pass {
    Always,
    IfChanged,
}

/// What a host of `shape` can book: the shape the pool books for it, with the CPU slack the
/// placement engine allows on its cores, which is the most the engine places on it. The
/// capacity is the bound a placement is held to in its own transaction against the
/// allocations the store holds, so it must not be tighter than what the engine allows.
fn capacity_of(shape: &Shape, slack: CpuSlack) -> Capacity {
    let resources = shape.resources();
    let cpu = u128::from(resources.cpu_millis()) * (1000 + u128::from(slack.permille())) / 1000;
    Capacity {
        cpu_millis: u64::try_from(cpu).unwrap_or(u64::MAX),
        memory_bytes: resources.memory_bytes(),
        disk_bytes: resources.disk_bytes(),
        accelerators: resources.gpus(),
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
    /// read of the tables is logged; the next pass reads them again.
    ///
    /// A host is offered to a job with host isolation only when its hello reported
    /// `host-jobs`. Each host such a job was placed on is tainted in the registry
    /// (`host-job`, the job's id as the run id) as soon as the placement is committed, and
    /// returned: the caller writes the bindings of the hosts returned
    /// ([`Agents::keep_taints`]). A host the registry no longer holds, or that was tainted
    /// before, is not returned.
    ///
    /// The tick runs this pass whatever changed; a heartbeat runs
    /// [`Agents::place_jobs_after_beat`].
    pub(crate) fn place_jobs(&self) -> Vec<HostId> {
        self.pass_over_queue(Pass::Always)
    }

    /// The pass a heartbeat runs: [`Agents::place_jobs`], skipped when nothing it depends on
    /// changed since the last pass that ran
    /// ([`SchedPlacer::place_jobs_if_changed`](super::place::SchedPlacer::place_jobs_if_changed)):
    /// the job tables, the engine's picture of the hosts, the hosts offered with what each
    /// offers, the hosts that said hello and what pass 2 told. The hosts are read from the
    /// registry first either way.
    pub(crate) fn place_jobs_after_beat(&self) -> Vec<HostId> {
        self.pass_over_queue(Pass::IfChanged)
    }

    /// [`Agents::place_jobs`], or [`Agents::place_jobs_after_beat`] as `pass` says. Nothing
    /// once the server drains ([`crate::drain`]): its hosts are leaving for another server,
    /// which places the queued jobs as they say hello there.
    fn pass_over_queue(&self, pass: Pass) -> Vec<HostId> {
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
        let views = self.registry.hosts();
        let skipped = views.iter().filter(|view| view.state == HostState::Live);
        self.taint_skips
            .note(skipped.filter_map(|view| view.taint.as_ref().map(|taint| (&view.id, taint))));
        let hello: BTreeSet<_> = views
            .iter()
            .filter(|view| view.claim.is_some())
            .map(|view| view.id.clone())
            .collect();
        // A host whose last job listing named a job it holds no live allocation for is
        // still running that job: its capacity is not counted until a listing is clean.
        let open = views.into_iter();
        let open = open.filter(|view| open_to_jobs(view, now) && self.listing_clean(&view.id));
        // The machine provider of each host, from its binding alone; a host of a provider the
        // Default platform does not list is no candidate here (it stays in the host table).
        let bindings = self.registry.bindings();
        let open = open.filter_map(|view| {
            let ranked = rank(&self.providers, bindings.get(&view.id))?;
            // The ceiling and the host-jobs capability are read again on every pass, from the
            // hello the host last said; a host whose hello is not held (after a restart)
            // advertises no network and no host jobs.
            let capabilities = view.claim.as_ref().map(|claim| claim.capabilities());
            let offer = OpenHost {
                ceiling: capabilities.map(ceiling_of).unwrap_or_default(),
                host_jobs: capabilities.is_some_and(reports_host_jobs),
                rank: Some(ranked),
                capacity: Some(capacity_of(&view.shape, self.cpu_slack)),
            };
            Some((view.id, offer))
        });
        let hosts = JobHosts {
            open: &open.collect(),
            hello: &hello,
            whole: self.registry.whole_node(),
        };
        let starts = Tainted {
            registry: &self.registry,
            wakes: [&self.wakes, &self.dispatching.wakes],
            hosts: Mutex::default(),
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
