//! The server's pass over the queued jobs ([`Agents::place_jobs`]): it reads the hosts the
//! registry would let take a job now and whose last job listing was clean, each with the
//! network ceiling its hello advertises, the hosts that said hello to this server, and the
//! pool's whole-node settings, and hands them to the placer's pass
//! ([`SchedPlacer::place_jobs_with`](super::place::SchedPlacer::place_jobs_with), whose
//! module, [`js_sched_placer::engine::jobs`], says what a pass does, and
//! [`js_sched_placer::engine::whole_jobs`] what it does for the jobs that book the whole
//! host).

use std::collections::BTreeSet;

use js_lease::Clock;
use js_registry::WholeNodeConfig;
use js_sched::ceiling::ceiling_of;
use tracing::warn;

use super::engine::jobs::{JobHosts, NoStarts, open_to_jobs};
use crate::agents::Agents;
use crate::metrics::PassSink;

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
    pub(crate) fn place_jobs(&self) {
        let (Some(jobs), Some(placer)) = (&self.jobs, &self.placer) else {
            return;
        };
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
            // The ceiling is read again on every pass, from the hello the host last said; a
            // host whose hello is not held (after a restart) advertises none.
            let claim = view.claim.as_ref();
            let ceiling = claim.map(|claim| ceiling_of(claim.capabilities()));
            (view.id, ceiling.unwrap_or_default())
        });
        let hosts = JobHosts {
            open: &open.collect(),
            hello: &hello,
            whole: self.registry.whole_node(),
        };
        let sink = PassSink(self.meter().sink());
        if let Err(error) = placer.place_jobs_with(jobs, &hosts, (&NoStarts, &sink)) {
            self.fence.note(&error);
            warn!(%error, "the queued jobs could not be read; the next pass places them");
        }
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "jobs_ports_tests.rs"]
mod ports_tests;
