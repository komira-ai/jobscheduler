//! The job pass of the tick: the half of a job's life that only time and host presence drive
//! (design section 6.1, "the one place time acts").
//!
//! [`run`] is one pass over the job and allocation tables, in the order of section 6.1:
//!
//! 1. **End what ran out of time** ([`job_expiry::expire`]): a queued job past its pool's
//!    `max_queue_wait` fails to start with `no-capacity`; a placed or starting job past its
//!    pool's `start_timeout` fails to start with `start-timeout`, its allocation moving to
//!    releasing in the same commit (the stop rides on each heartbeat reply while the host
//!    lists the job, [`crate::per_job::stops_for`]). Then the retention sweep deletes each
//!    ended job past its retention end with its result ([`js_jobs::JobStore::tick`]); a job
//!    that has not ended is never deleted, and no log object is touched.
//! 2. **Settle allocations** ([`job_loss::lose_absent`]): each host that holds a live
//!    allocation is read once; a host the provider reads absent has its allocations freed and
//!    its jobs lost.
//!
//! Presence and marks (`registry.advance()`) and placement follow in the tick
//! ([`crate::api::Shared::tick`]). Every state change is one store transaction carrying the
//! allocation change that goes with it (section 8, row 25). The pass reads no clock of its
//! own: `now` is the server's injected clock, and the store stamps what it writes with its own
//! injected clock. No rule of the pass ends a job because its host is silent (section 6.3,
//! question 25): see [`crate::job_loss`].
//!
//! The pass holds each queue-wait and start deadline to at least the job's deadline floor,
//! which [`js_jobs::JobStore::raise_at_boot`] raises, with the retention ends, to the boot plus
//! 120 s in one transaction when the server opens the job store, before its first tick
//! ([`crate::job_store::open`]): an outage longer than a queue wait, a start timeout or a
//! retention window fails no queued job, ends no start and deletes no result at the first
//! tick.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobId, JobStoreError, StartFailure};
use js_lease::Clock;
use js_registry::Registry;
use js_state::StoreTime;
use js_types::HostId;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::job_expiry::{self, JobLimits};
use crate::job_loss::{self, Observe, Seen};
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;
use crate::sched::place::SchedPlacer;

/// What one pass changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Passed {
    /// The jobs that ran out of time, with how each failed to start.
    pub(crate) expired: Vec<(JobId, StartFailure)>,
    /// The ended jobs deleted at their retention end.
    pub(crate) deleted: Vec<JobId>,
    /// The jobs lost because the provider read their host absent.
    pub(crate) lost: Vec<JobId>,
}

impl Passed {
    /// Whether the pass changed nothing.
    fn is_empty(&self) -> bool {
        self.expired.is_empty() & self.deleted.is_empty() & self.lost.is_empty()
    }
}

/// One pass over `jobs` at `now`, under `limits`, reading each host's presence through
/// `observer` (the steps of the module docs), and what it changed. Each start failure and loss
/// is told to `metrics` as its commit lands.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the next pass reads the tables again.
pub(crate) fn run(
    jobs: &ServerJobs,
    limits: JobLimits,
    (observer, metrics): (&dyn Observe, &dyn MetricsSink),
    now: StoreTime,
) -> Result<Passed, JobStoreError> {
    let expired = job_expiry::expire(jobs, limits, now, metrics)?;
    let deleted = jobs.tick()?;
    let lost = job_loss::lose_absent(jobs, observer, metrics)?;
    Ok(Passed {
        expired,
        deleted,
        lost,
    })
}

/// What the server's provider reads of a host: absent when the registry removed it
/// ([`crate::sched::place::SchedPlacer::gone`]); present while the registry hears it; unknown
/// while it is silent or the registry does not hold it. Silence is never absence.
struct Presence<'a> {
    registry: &'a Registry,
    gone: &'a BTreeMap<HostId, u64>,
}

impl Observe for Presence<'_> {
    fn observe(&self, host: &HostId) -> Result<Seen, String> {
        let seen = if self.gone.contains_key(host) {
            Seen::Absent
        } else if self.registry.own_host(host).is_some()
            & self.registry.silent_since(host).is_none()
        {
            Seen::Present
        } else {
            Seen::Unknown
        };
        Ok(seen)
    }
}

/// `millis` since the Unix epoch as a reading of the store's clock, which counts from there.
pub(crate) fn store_time(millis: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(millis))
}

impl Agents {
    /// The job pass of this tick ([`run`]) when the server holds the job tables, at the
    /// server's clock, under the pool's limits, with the hosts the registry removed read absent;
    /// then those removals are settled. A failed pass is logged and changes no removal; the
    /// next tick passes again. The pass's duration, the start failures and losses it made,
    /// and a failure of it, are counted on the agent side's meter ([`Agents::meter`]). Without
    /// the job tables, nothing.
    pub(crate) fn job_pass(&self) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let gone = self
            .placer
            .as_ref()
            .map(|placer| placer.gone())
            .unwrap_or_default();
        let observer = Presence {
            registry: &self.registry,
            gone: &gone,
        };
        let now = store_time(self.clock.now().millis());
        let meter = self.meter();
        let sink = meter.sink();
        let limits = self.job_limits;
        let (passed, took) = meter.timed(|| run(jobs, limits, (&observer, sink), now));
        sink.job_pass_took(took);
        match passed {
            Ok(passed) => {
                if !passed.is_empty() {
                    let expired = passed.expired.len();
                    let (deleted, lost) = (passed.deleted.len(), passed.lost.len());
                    info!(expired, deleted, lost, "the job pass ended or deleted jobs");
                }
                let settle = |placer: &Arc<SchedPlacer>| placer.settle_gone(&gone);
                self.placer.iter().for_each(settle);
            }
            Err(error) => {
                sink.store_write_failed();
                warn!(%error, "the job pass failed; the next tick passes again");
            }
        }
    }
}

#[cfg(test)]
#[path = "job_pass_tests.rs"]
mod tests;
