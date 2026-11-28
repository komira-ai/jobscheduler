//! What a pass over the queued jobs saw ([`Seen`]), so that a pass after a heartbeat runs only
//! when something it depends on changed since the last pass ([`SchedPlacer::place_jobs_if_changed`]),
//! and the job tables a pass read ([`Tables`]), so that a pass reads them again only when they
//! changed ([`SchedPlacer::tables`]), and then only the rows that changed, from the store's feed,
//! and not for a job a submit of this server just recorded, which is added to them instead
//! ([`SchedPlacer::note_inserted`]), unless a pass holds them at that moment.
//!
//! A pass ([`super::jobs`]) decides from:
//!
//! - the job tables: the live allocations, the queued jobs, the marks and the cordons. They
//!   change only by a commit to the job store, which this server's job store counts
//!   ([`js_jobs::JobStore::written`]) and, as far as the store tells its head, so does every
//!   other server's. After such a commit the tables are brought up to date from the store's
//!   feed ([`js_jobs::JobStore::table_changes`]): each job and allocation whose row changed is
//!   taken from its last row there, and the marks or the cordons are read again when one of
//!   theirs changed; the queued jobs no commit touched are not read again. Only where the feed
//!   cannot tell (a store that tells no head, a feed trimmed or too long since) are the four
//!   tables read in full. The placer counts each change of what the tables hold
//!   ([`SchedPlacer::tables`]): a job submitted, cancelled or placed, its queue status written,
//!   an allocation booked or freed, a mark or a cordon written; a commit that changes none of
//!   them (a running job's start or end, a host's node row, a duty's row) is not counted;
//! - the engine's picture of the hosts, which the registry changes through the
//!   [`js_registry::Placer`] calls only. The placer counts each call that changed it
//!   ([`SchedPlacer::engine_changes`]): a hello, a heartbeat that gave up an allocation, and a
//!   host forgotten. A heartbeat's load reading alone is not counted: a busy host's load moves
//!   with every beat, and a pass for each would walk the whole queue every few seconds per host
//!   to place nothing; what a lower load leaves room for is placed by the next pass that runs, at
//!   the latest the tick's. A call that found the engine held is kept for it and counted when it
//!   is told, by what it changes;
//! - the hosts it is offered ([`super::jobs::JobHosts`]): the hosts open to jobs, each with
//!   its ceiling, whether it runs host jobs and its machine provider (so a host whose provider
//!   rank changed makes the next pass run), the hosts that said hello, the hosts a taint
//!   closes, and the pool's whole-node settings. They are read from the registry and the job
//!   store's host rows before every pass, skipped or not,
//!   so a host that went lost, came back, was tainted or cordoned, finished its last run or
//!   listed a job it holds no allocation for is seen at once;
//! - what pass 2 told of the queued jobs since the last pass ([`SchedPlacer::note_outcomes`]);
//! - the clock.
//!
//! A pass after a heartbeat is skipped when the first four are as the last pass that ran
//! found them, that pass failed no read and no write, and nothing waits from pass 2. A skipped
//! pass would have placed nothing and written nothing, except what the clock alone changes,
//! and what a host's load alone changes: a whole-host job's mark that turns at its drain time
//! or at the end of a bar, a stored mark whose host's hello is waited for past the server's
//! start plus the regain grace, the wall limit a backfill placement gets, a retried job whose
//! backoff ends (`jobs_retry`), a cordon that lapses, and the room a host's lower load
//! reading leaves. Those are left to the next pass that runs, at the latest the tick's, which
//! always runs ([`SchedPlacer::place_jobs_with`]). A skipped pass reads the store's feed when a
//! commit came since, and none of the tables unless a mark or a cordon changed.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use js_jobs::{
    AllocationRecord, IdSource, Inserted, JobId, JobRecord, JobStore, JobStoreError, MarkRecord,
    TableChanges, Written, queue_order,
};
use js_registry::WholeNodeConfig;
use js_state::Revision;
use js_types::{HostId, Timestamp};

use super::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics};
use crate::place::SchedPlacer;

/// What one pass over the queued jobs depended on besides the clock and pass 2, as it stood
/// when the pass began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// How many changes of what the job tables hold the placer had counted
    /// ([`SchedPlacer::tables`]).
    tables: u64,
    /// How many changes the engine had been told of from outside the passes.
    engine: u64,
    /// The hosts open to jobs, each with what it offers.
    open: BTreeMap<HostId, OpenHost>,
    /// The hosts that said hello.
    hello: BTreeSet<HostId>,
    /// The hosts a taint closes.
    tainted: BTreeSet<HostId>,
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
    /// The store's head the tables follow: every commit at or below it is in them, so the
    /// changes after it bring them up to date ([`JobStore::table_changes`]); `None` for a store
    /// that tells no head, whose tables are read in full after each write.
    head: Option<Revision>,
    tables: Arc<Tables>,
    /// Whether a job a submit recorded was added since a pass last took them.
    added: bool,
}

/// Where a pass took the job tables from ([`SchedPlacer::tables`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablesFrom {
    /// The job store: the live allocations, the queued jobs, the marks and the cordons were
    /// read, as a write of the job store came since the last read and the store's feed could
    /// not tell what it changed.
    Store,
    /// Those the last read kept, brought up to date from the store's feed with the rows that
    /// changed since ([`JobStore::table_changes`]): only the marks or the cordons, when one of
    /// theirs changed, were read again.
    Changed,
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

    fn refused(&self, host: &HostId, reason: &'static str) {
        self.metrics.refused(host, reason);
    }

    fn curve_wait_expired(&self) {
        self.metrics.curve_wait_expired();
    }

    fn queue_status_written(&self) {
        self.metrics.queue_status_written();
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

impl Tables {
    /// Brings the tables up to date with `changes`, which the store's feed told since they
    /// were read, and with `marks` and `cordons` read again when the feed said one of theirs
    /// changed: each job and allocation told takes the place of its earlier record, in queue
    /// order and in job id order, or goes. Whether what they hold changed.
    fn apply(
        &mut self,
        changes: TableChanges,
        marks: Option<Vec<MarkRecord>>,
        cordons: Option<BTreeMap<HostId, Timestamp>>,
    ) -> bool {
        let TableChanges { queued, live, .. } = changes;
        let was: BTreeMap<JobId, JobRecord> = self
            .queued
            .extract_if(.., |job| queued.contains_key(&job.id))
            .map(|job| (job.id.clone(), job))
            .collect();
        let mut changed = false;
        for (job, record) in queued {
            changed |= was.get(&job) != record.as_ref();
            if let Some(record) = record {
                let at = self
                    .queued
                    .partition_point(|other| queue_order(other, &record).is_lt());
                self.queued.insert(at, record);
            }
        }
        let held: BTreeMap<JobId, AllocationRecord> = self
            .live
            .extract_if(.., |allocation| live.contains_key(&allocation.job))
            .map(|allocation| (allocation.job.clone(), allocation))
            .collect();
        for (job, allocation) in live {
            changed |= held.get(&job) != allocation.as_ref();
            if let Some(allocation) = allocation {
                let at = self
                    .live
                    .partition_point(|other| other.job < allocation.job);
                self.live.insert(at, allocation);
            }
        }
        if let Some(marks) = marks {
            changed |= self.marks != marks;
            self.marks = marks;
        }
        // A cordon's lapse is read as the time it had left, so two reads never hold the same
        // times: one read again counts as a change.
        if let Some(cordons) = cordons {
            changed = true;
            self.cordons = cordons;
        }
        changed
    }
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
    /// last pass that ran (the module docs): then nothing is placed, and the answer is `None`.
    /// The job tables are brought up to date first ([`SchedPlacer::tables`]), which reads
    /// nothing when no commit came since and only the store's feed when one did. The server
    /// runs it after a host's heartbeat; the tick runs every pass.
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
        let (tables, now) = self.taken(jobs, hosts, sinks.1)?;
        let told = !lock(&self.outcomes).is_empty();
        if !told && lock(&self.seen).as_ref() == Some(&now) {
            #[cfg(any(test, feature = "test-support"))]
            self.skipped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(None);
        }
        self.pass_seen(jobs, hosts, sinks, (tables, now)).map(Some)
    }

    /// The job tables brought up to date ([`SchedPlacer::tables`]), and what a pass on `hosts`
    /// would depend on as it stands now. A failed read forgets what the last pass found, so
    /// the next pass after a heartbeat runs.
    ///
    /// # Errors
    /// As [`SchedPlacer::tables`].
    pub(crate) fn taken<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
        metrics: &dyn PassMetrics,
    ) -> Result<(Arc<Tables>, Seen), JobStoreError> {
        let (tables, changed) = self.tables(jobs, metrics).inspect_err(|_| {
            *lock(&self.seen) = None;
        })?;
        Ok((tables, self.seen_now(changed, hosts)))
    }

    /// One pass over the queued jobs, under the pass's lock, on `tables`, that began when it
    /// found `now`: what it found is kept for the next [`SchedPlacer::place_jobs_if_changed`]
    /// when the pass failed no write, and forgotten otherwise, so the next pass runs.
    pub(crate) fn pass_seen<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
        (starts, metrics): (&dyn JobStarts, &dyn PassMetrics),
        (tables, now): (Arc<Tables>, Seen),
    ) -> Result<Vec<JobId>, JobStoreError> {
        *lock(&self.seen) = None;
        let watched = Watched {
            metrics,
            failed: Cell::new(false),
        };
        let placed = self.place_queued(jobs, hosts, (starts, &watched), &tables);
        if !watched.failed.get() {
            *lock(&self.seen) = Some(now);
        }
        Ok(placed)
    }

    /// What a pass on `hosts` would depend on, as it stands now, with `tables` the count of
    /// the changes of the job tables it took ([`SchedPlacer::tables`]). The engine is told of
    /// the calls kept for it first, so the count of its changes holds them.
    pub(crate) fn seen_now(&self, tables: u64, hosts: &JobHosts<'_>) -> Seen {
        drop(self.engine());
        Seen {
            tables,
            engine: self.engine_changes(),
            open: hosts.open.clone(),
            hello: hosts.hello.clone(),
            tainted: hosts.tainted.clone(),
            whole: hosts.whole.cloned(),
        }
    }

    /// The job tables of `jobs`, with how many changes of what they hold the placer has
    /// counted: those the last pass read when no write of the job store came since but the
    /// jobs this server's submits recorded and added to them ([`SchedPlacer::note_inserted`]);
    /// otherwise, when they were read from `jobs` itself, those brought up to date from the
    /// store's feed, when it tells what changed since ([`JobStore::table_changes`]); and
    /// otherwise read now, with the
    /// cordons the store holds. Where they came from is told to `metrics`. A read in full, a
    /// job added, and changes that changed what the tables hold each count; tables kept, or
    /// brought up to date by changes of no row they hold, do not. A failed read keeps nothing.
    ///
    /// # Errors
    /// As [`JobStore::live_allocations`], [`JobStore::queued`], [`JobStore::marks`],
    /// [`JobStore::cordons`] and [`JobStore::table_changes`].
    pub(crate) fn tables<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        metrics: &dyn PassMetrics,
    ) -> Result<(Arc<Tables>, u64), JobStoreError> {
        // The head first: a commit to a job table after it moves the reading below, so tables
        // kept at that reading hold every such commit at or below the head.
        let head = jobs.feed_head()?;
        let written = jobs.written()?;
        let mut kept = lock(&self.tables);
        if let Some(read) = kept.as_mut()
            && read.at == written
        {
            // No commit to a job table came since they were read, so they follow the head now:
            // the commits since that changed no table (the hosts' node rows) are not read again.
            if read.head.is_some() {
                read.head = head;
            }
            let added = std::mem::replace(&mut read.added, false);
            let from = if added {
                TablesFrom::Submitted
            } else {
                TablesFrom::Kept
            };
            metrics.job_tables(from);
            return Ok((Arc::clone(&read.tables), self.tables_changed()));
        }
        if let Some(read) = kept.as_mut()
            && read.at.same_job_store(&written)
            && let Some(after) = read.head
        {
            match self.follow(jobs, read, after) {
                Ok(true) => {
                    read.at = written;
                    read.added = false;
                    metrics.job_tables(TablesFrom::Changed);
                    return Ok((Arc::clone(&read.tables), self.tables_changed()));
                }
                Ok(false) => {}
                Err(error) => {
                    *kept = None;
                    return Err(error);
                }
            }
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
            head,
            tables: Arc::clone(&tables),
            added: false,
        });
        Ok((tables, self.count_tables_changed()))
    }

    /// Brings `read`, the tables kept at the store's head `after`, up to date with the changes
    /// the store's feed tells since ([`JobStore::table_changes`]), the marks and the cordons
    /// read again when one of theirs changed; counts the change when what they hold changed.
    /// Whether the feed could tell: `false` leaves them as they were, to be read in full.
    ///
    /// # Errors
    /// As [`JobStore::table_changes`], [`JobStore::marks`] and [`JobStore::cordons`].
    fn follow<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        read: &mut Kept,
        after: Revision,
    ) -> Result<bool, JobStoreError> {
        let Some(changes) = jobs.table_changes(after)? else {
            return Ok(false);
        };
        let marks = changes.marks.then(|| jobs.marks()).transpose()?;
        let cordons = changes.cordons.then(|| jobs.cordons()).transpose()?;
        let cordons = cordons.map(|cordons| cordons_from(cordons, self.clock.now()));
        let head = changes.head;
        if Arc::make_mut(&mut read.tables).apply(changes, marks, cordons) {
            self.count_tables_changed();
        }
        read.head = Some(head);
        Ok(true)
    }

    /// How many changes of what the job tables hold the placer has counted.
    fn tables_changed(&self) -> u64 {
        self.tables_changed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Counts a change of what the job tables hold, answering the count with it.
    fn count_tables_changed(&self) -> u64 {
        let ordering = std::sync::atomic::Ordering::SeqCst;
        self.tables_changed.fetch_add(1, ordering).saturating_add(1)
    }

    /// Adds the job a submit of this server just recorded, `inserted`, to the job tables the
    /// last pass read, when the commit that recorded it is the only write of the job store
    /// since they were read ([`js_jobs::Step::after`]): the next pass then takes them
    /// without reading the four tables again ([`TablesFrom::Submitted`]). Otherwise, and for
    /// a job the call did not record, nothing changes, and the next pass reads them as any
    /// write makes it. Whether the job was added; an added job counts as a change of what the
    /// tables hold ([`SchedPlacer::tables`]).
    ///
    /// A job is added once, in queue order ([`queue_order`]): the tables may have been read
    /// after the commit landed and hold it already. The store's head the tables follow is left
    /// as it was, so the feed tells the commit again, which changes nothing then.
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
        self.count_tables_changed();
        true
    }
}

#[cfg(test)]
#[path = "jobs_seen_tests.rs"]
mod tests;
