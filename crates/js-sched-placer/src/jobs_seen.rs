//! What a pass over the queued jobs saw ([`Seen`]), so that a pass after a heartbeat runs only
//! when something it depends on changed since the last pass ([`SchedPlacer::place_jobs_if_changed`]),
//! and the job tables a pass read ([`Tables`]), so that a pass reads them again only when they
//! changed ([`SchedPlacer::tables`]).
//!
//! A pass ([`super::jobs`]) decides from:
//!
//! - the job tables: the live allocations, the queued jobs and the marks. They change only by
//!   a write of the job store, which counts its writes ([`js_jobs::JobStore::written`]): a job
//!   submitted, cancelled, started, ended or freed, a host's job lost, a prepare failed, the
//!   tick's expiry and loss, and the pass's own placements, marks and queue status writes;
//! - the engine's picture of the hosts, which the registry changes through the
//!   [`js_registry::Placer`] calls only. The placer counts each call that changed it
//!   ([`SchedPlacer::engine_changes`]): a hello, a heartbeat that brought a load reading or
//!   gave up an allocation, and a host forgotten; a heartbeat with no reading that gives up
//!   nothing is not counted;
//! - the hosts it is offered ([`super::jobs::JobHosts`]): the hosts open to jobs, each with
//!   its ceiling, whether it runs host jobs and its machine provider (so a host whose provider
//!   rank changed makes the next pass run), the hosts that said hello, and the pool's
//!   whole-node settings. They are read from the registry before every pass, skipped or not,
//!   so a host that went lost, came back, was tainted or cordoned, finished its last run or
//!   listed a job it holds no allocation for is seen at once;
//! - what pass 2 told of the queued jobs since the last pass ([`SchedPlacer::note_outcomes`]);
//! - the clock.
//!
//! A pass after a heartbeat is skipped when the first four are as the last pass that ran
//! found them, that pass failed no read and no write, and nothing waits from pass 2. A skipped
//! pass would have placed nothing and written nothing, except what the clock alone changes:
//! a whole-host job's mark that turns at its drain time or at the end of a bar, a stored mark
//! whose host's hello is waited for past the server's start plus the regain grace, and the
//! wall limit a backfill placement gets. Those are left to the next pass that runs, at the
//! latest the tick's, which always runs ([`SchedPlacer::place_jobs_with`]).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;

use js_jobs::{
    AllocationRecord, IdSource, JobId, JobRecord, JobStore, JobStoreError, MarkRecord, Written,
};
use js_registry::WholeNodeConfig;
use js_types::HostId;

use super::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics};
use crate::place::SchedPlacer;

/// What one pass over the queued jobs depended on besides the clock and pass 2, as it stood
/// when the pass began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// Where the job store's writes stood.
    jobs: Written,
    /// How many changes the engine had been told of from outside the passes.
    engine: u64,
    /// The hosts open to jobs, each with what it offers.
    open: BTreeMap<HostId, OpenHost>,
    /// The hosts that said hello.
    hello: BTreeSet<HostId>,
    /// The pool's whole-node settings.
    whole: Option<WholeNodeConfig>,
}

/// The job tables as one pass read them.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    /// Every live allocation ([`JobStore::live_allocations`]).
    pub(crate) live: Vec<AllocationRecord>,
    /// Every queued job, in queue order ([`JobStore::queued`]).
    pub(crate) queued: Vec<JobRecord>,
    /// Every mark ([`JobStore::marks`]).
    pub(crate) marks: Vec<MarkRecord>,
}

/// The job tables a pass read, with where the job store's writes stood before the read.
pub(crate) type Read = (Written, Arc<Tables>);

/// The [`PassMetrics`] of a pass, passed on, with whether a write of the pass failed.
struct Watched<'a> {
    metrics: &'a dyn PassMetrics,
    failed: Cell<bool>,
}

impl PassMetrics for Watched<'_> {
    fn job_placed(&self, waited: Duration) {
        self.metrics.job_placed(waited);
    }

    fn store_write_failed(&self) {
        self.failed.set(true);
        self.metrics.store_write_failed();
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SchedPlacer {
    /// [`SchedPlacer::place_jobs_with`], unless nothing the pass depends on changed since the
    /// last pass that ran (the module docs): then nothing is read or placed, and the answer is
    /// `None`. The server runs it after a host's heartbeat; the tick runs every pass.
    ///
    /// # Errors
    /// As [`SchedPlacer::place_jobs_with`].
    pub fn place_jobs_if_changed<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
        sinks: (&dyn JobStarts, &dyn PassMetrics),
    ) -> Result<Option<Vec<JobId>>, JobStoreError> {
        let _pass = self.pass();
        let now = self.seen_now(jobs, hosts);
        let told = !lock(&self.outcomes).is_empty();
        if !told && lock(&self.seen).as_ref() == Some(&now) {
            #[cfg(any(test, feature = "test-support"))]
            self.skipped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(None);
        }
        self.pass_seen(jobs, hosts, sinks, now).map(Some)
    }

    /// One pass over the queued jobs, under the pass's lock, that began when it found `now`:
    /// what it found is kept for the next [`SchedPlacer::place_jobs_if_changed`] when the pass
    /// failed no read and no write, and forgotten otherwise, so the next pass runs.
    pub(crate) fn pass_seen<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
        (starts, metrics): (&dyn JobStarts, &dyn PassMetrics),
        now: Seen,
    ) -> Result<Vec<JobId>, JobStoreError> {
        *lock(&self.seen) = None;
        let watched = Watched {
            metrics,
            failed: Cell::new(false),
        };
        let placed = self.place_queued(jobs, hosts, (starts, &watched));
        if placed.is_ok() && !watched.failed.get() {
            *lock(&self.seen) = Some(now);
        }
        placed
    }

    /// What a pass over `jobs` on `hosts` would depend on, as it stands now.
    pub(crate) fn seen_now<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
    ) -> Seen {
        Seen {
            jobs: jobs.written(),
            engine: self.engine_changes(),
            open: hosts.open.clone(),
            hello: hosts.hello.clone(),
            whole: hosts.whole.cloned(),
        }
    }

    /// The job tables of `jobs`: those the last pass read when no write of the job store came
    /// since, and otherwise read now. A failed read keeps nothing.
    ///
    /// # Errors
    /// As [`JobStore::live_allocations`], [`JobStore::queued`] and [`JobStore::marks`].
    pub(crate) fn tables<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
    ) -> Result<Arc<Tables>, JobStoreError> {
        let written = jobs.written();
        let mut kept = lock(&self.tables);
        if let Some((at, tables)) = kept.as_ref()
            && *at == written
        {
            return Ok(Arc::clone(tables));
        }
        *kept = None;
        let tables = Arc::new(Tables {
            live: jobs.live_allocations()?,
            queued: jobs.queued()?,
            marks: jobs.marks()?,
        });
        *kept = Some((written, Arc::clone(&tables)));
        Ok(tables)
    }
}

#[cfg(test)]
#[path = "jobs_seen_tests.rs"]
mod tests;
