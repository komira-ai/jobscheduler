//! The server's pass over the queued jobs ([`Agents::place_jobs`]): it reads the hosts the
//! registry would let take a job now and whose last job listing was clean, each with the network ceiling its hello advertises,
//! and hands them to the placer's pass
//! ([`SchedPlacer::place_jobs`](super::place::SchedPlacer::place_jobs), whose module,
//! [`js_sched_placer::engine::jobs`], says what a pass does).

use js_lease::Clock;
use js_sched::ceiling::ceiling_of;
use tracing::warn;

use super::engine::jobs::{NoStarts, open_to_jobs};
use crate::agents::Agents;
use crate::metrics::PassSink;

impl Agents {
    /// One pass over the queued jobs
    /// ([`SchedPlacer::place_jobs`](super::place::SchedPlacer::place_jobs)) on the hosts the
    /// registry would let take a job now and whose last job listing was clean
    /// ([`Agents::listing_clean`]), when this server holds the job tables and places with
    /// `js-sched` (`--placement js-sched`); otherwise nothing, so no job is placed under the
    /// registry's placement. A failed read of the tables is logged; the next pass reads them
    /// again, unless the store said it has a newer writer, which trips the fence
    /// ([`crate::fence`]).
    pub(crate) fn place_jobs(&self) {
        let (Some(jobs), Some(placer)) = (&self.jobs, &self.placer) else {
            return;
        };
        let now = self.clock.now();
        let hosts = self.registry.hosts().into_iter();
        // A host whose last job listing named a job it holds no live allocation for is
        // still running that job: its capacity is not counted until a listing is clean.
        let open = hosts.filter(|view| open_to_jobs(view, now) && self.listing_clean(&view.id));
        let open = open.map(|view| {
            // The ceiling is read again on every pass, from the hello the host last said; a
            // host whose hello is not held (after a restart) advertises none.
            let claim = view.claim.as_ref();
            let ceiling = claim.map(|claim| ceiling_of(claim.capabilities()));
            (view.id, ceiling.unwrap_or_default())
        });
        if let Err(error) = placer.place_jobs(
            jobs,
            &open.collect(),
            (&NoStarts, &PassSink(self.meter().sink())),
        ) {
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
