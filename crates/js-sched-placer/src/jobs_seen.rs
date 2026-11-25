//! What a pass over the queued jobs saw ([`Seen`]), so that a pass after a heartbeat runs only
//! when something it depends on changed since the last pass ([`SchedPlacer::place_jobs_if_changed`]),
//! and the job tables a pass read ([`Tables`]), so that a pass reads them again only when they
//! changed ([`SchedPlacer::tables`]), and not for a job a submit of this server just recorded,
//! which is added to them instead ([`SchedPlacer::note_inserted`]), unless a pass holds them
//! at that moment.
//!
//! A pass ([`super::jobs`]) decides from:
//!
//! - the job tables: the live allocations, the queued jobs and the marks. They change only by
//!   a commit to the job store, which this server's job store counts ([`js_jobs::JobStore::written`])
//!   and, as far as the store tells its head, so does every other server's: a job submitted,
//!   cancelled, started, ended or freed, a host's job lost, a prepare failed, the tick's expiry
//!   and loss, and the pass's own placements, marks and queue status writes;
//! - the engine's picture of the hosts, which the registry changes through the
//!   [`js_registry::Placer`] calls only. The placer counts each call that changed it
//!   ([`SchedPlacer::engine_changes`]): a hello, a heartbeat that brought a load reading or
//!   gave up an allocation, and a host forgotten; a heartbeat with no reading that gives up
//!   nothing is not counted, unless it found the engine held and was kept for it, which
//!   counts whatever it changes;
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
use std::sync::{Arc, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use js_jobs::{
    AllocationRecord, IdSource, Inserted, JobId, JobRecord, JobStore, JobStoreError, MarkRecord,
    Written, queue_order,
};
use js_registry::WholeNodeConfig;
use js_types::{HostId, Timestamp};

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
#[derive(Debug, Default, Clone)]
pub(crate) struct Tables {
    /// Every live allocation ([`JobStore::live_allocations`]).
    pub(crate) live: Vec<AllocationRecord>,
    /// Every queued job, in queue order ([`JobStore::queued`]).
    pub(crate) queued: Vec<JobRecord>,
    /// Every mark ([`JobStore::marks`]).
    pub(crate) marks: Vec<MarkRecord>,
    /// When each cordon the job store holds lapses, on the placer's clock: the time it had
    /// left when the tables were read, from then ([`JobStore::cordons`]). A cordon lapses by
    /// the clock with no write, so a pass judges each against its own time.
    pub(crate) cordons: BTreeMap<HostId, Timestamp>,
}

/// The job tables a pass read, with where the job store's writes stood before the read.
#[derive(Debug)]
pub(crate) struct Kept {
    /// Where the job store's writes stood before the read, or after the last job a submit
    /// recorded and added to them.
    at: Written,
    tables: Arc<Tables>,
    /// Whether a job a submit recorded was added since a pass last took them.
    added: bool,
}

/// Where a pass took the job tables from ([`SchedPlacer::tables`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablesFrom {
    /// The job store: the live allocations, the queued jobs, the marks and the cordons were
    /// read, as a write of the job store came since the last read.
    Store,
    /// Those the last read kept, with no write of the job store since.
    Kept,
    /// Those the last read kept, with the jobs submits of this server recorded since added to
    /// them, and no other write: read again, they would have been the same.
    Submitted,
}

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

    fn job_tables(&self, from: TablesFrom) {
        self.metrics.job_tables(from);
    }

    fn refused(&self, host: &HostId, reason: &'static str) {
        self.metrics.refused(host, reason);
    }

    fn curve_wait_expired(&self) {
        self.metrics.curve_wait_expired();
    }
}

/// When each of `cordons` lapses, each with the time it had left at `now`.
fn cordons_from(
    cordons: BTreeMap<HostId, js_types::cordon::Cordon<Duration>>,
    now: Timestamp,
) -> BTreeMap<HostId, Timestamp> {
    let lapse = |(host, cordon): (HostId, js_types::cordon::Cordon<Duration>)| {
        (host, now.plus(cordon.until))
    };
    cordons.into_iter().map(lapse).collect()
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `mutex` locked, when no one holds it now; `None`, without waiting, when someone does.
fn lock_if_free<T>(mutex: &std::sync::Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
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
        let now = self.seen_now(jobs, hosts)?;
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
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read: whether the tables changed
    /// is then not known, and the pass is not skipped.
    pub(crate) fn seen_now<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
    ) -> Result<Seen, JobStoreError> {
        Ok(Seen {
            jobs: jobs.written()?,
            engine: self.engine_changes(),
            open: hosts.open.clone(),
            hello: hosts.hello.clone(),
            whole: hosts.whole.cloned(),
        })
    }

    /// The job tables of `jobs`: those the last pass read when no write of the job store came
    /// since but the jobs this server's submits recorded and added to them
    /// ([`SchedPlacer::note_inserted`]), and otherwise read now, with the cordons the store
    /// holds. Where they came from is told to `metrics`. A failed read keeps nothing.
    ///
    /// # Errors
    /// As [`JobStore::live_allocations`], [`JobStore::queued`], [`JobStore::marks`] and
    /// [`JobStore::cordons`].
    pub(crate) fn tables<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        metrics: &dyn PassMetrics,
    ) -> Result<Arc<Tables>, JobStoreError> {
        let written = jobs.written()?;
        let mut kept = lock(&self.tables);
        if let Some(read) = kept.as_mut()
            && read.at == written
        {
            let added = std::mem::replace(&mut read.added, false);
            let from = if added {
                TablesFrom::Submitted
            } else {
                TablesFrom::Kept
            };
            metrics.job_tables(from);
            return Ok(Arc::clone(&read.tables));
        }
        *kept = None;
        let tables = Arc::new(Tables {
            live: jobs.live_allocations()?,
            queued: jobs.queued()?,
            marks: jobs.marks()?,
            cordons: cordons_from(jobs.cordons()?, self.clock.now()),
        });
        metrics.job_tables(TablesFrom::Store);
        *kept = Some(Kept {
            at: written,
            tables: Arc::clone(&tables),
            added: false,
        });
        Ok(tables)
    }

    /// Adds the job a submit of this server just recorded, `inserted`, to the job tables the
    /// last pass read, when the commit that recorded it is the only write of the job store
    /// since they were read ([`js_jobs::Step::after`]): the next pass then takes them
    /// without reading the four tables again ([`TablesFrom::Submitted`]). Otherwise, and for
    /// a job the call did not record, nothing changes, and the next pass reads them as any
    /// write makes it. Whether the job was added.
    ///
    /// A job is added once, in queue order ([`queue_order`]): the tables may have been read
    /// after the commit landed and hold it already.
    ///
    /// It never waits: while a pass holds the tables (it reads them again, which takes as long
    /// as the four reads do), nothing is added and the answer is `false`, so a submit that
    /// calls it answers without waiting for the pass. That pass, or the next, reads the job
    /// from the store, as any write of the job store makes it.
    pub fn note_inserted(&self, inserted: &Inserted) -> bool {
        let Some(mut kept) = lock_if_free(&self.tables) else {
            return false;
        };
        let Some(read) = kept.as_mut() else {
            return false;
        };
        let Some(after) = inserted.step.and_then(|step| step.after(read.at)) else {
            return false;
        };
        let job = &inserted.record;
        let queued = &mut Arc::make_mut(&mut read.tables).queued;
        queued.retain(|other| other.id != job.id);
        let at = queued.partition_point(|other| queue_order(other, job).is_lt());
        queued.insert(at, job.clone());
        read.at = after;
        read.added = true;
        true
    }
}

#[cfg(test)]
#[path = "jobs_seen_tests.rs"]
mod tests;
