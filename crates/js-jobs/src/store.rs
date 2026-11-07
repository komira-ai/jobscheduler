//! [`JobStore`]: the job, allocation, mark and taint tables over a state [`Store`].

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::{Commit, Cond, ResumeToken, ScanRange, Store, Txn, Version, Versioned};
use js_types::HostId;

use crate::codec::{Carried, DecodeError, decode_allocation, decode_job};
use crate::error::JobStoreError;
use crate::ids::{AllocationId, ClientName, IdSource, JobId, RandomIds};
use crate::keys;
use crate::record::{
    AllocationRecord, AllocationStep, Booked, EndReason, HostBooking, JobRecord, JobSpec, JobState,
    may_move,
};
use crate::result::{JobEnd, Output, Retention};

mod ending;
mod hosts;
mod index;
mod marks;
mod queue;
mod taints;
mod whole;

pub use index::Backfill;

pub use queue::queue_order;

pub use whole::{FailedPrepare, Placement};

/// The schema version of the rows this build writes.
pub const SCHEMA_VERSION: u32 = 1;

/// How many ids [`JobStore::insert_queued`] mints for a job without one before it gives up.
pub const MINT_TRIES: usize = 8;

/// The most jobs one [`JobStore::list_jobs`] page holds, whatever limit is asked for.
pub const MAX_PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(999);

/// A job to record: its id if the client gave one, the job, its priority, and the client
/// that submits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// The client's id, or `None` for the store to mint one.
    pub id: Option<JobId>,
    /// The job.
    pub spec: JobSpec,
    /// Its priority.
    pub priority: i32,
    /// The client that submits it, recorded on a new job ([`JobRecord::client`]); not
    /// compared with a stored job of the same id.
    pub client: Option<ClientName>,
}

/// What [`JobStore::insert_queued`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inserted {
    /// The job's record: the new one, or the one already stored, as it is now.
    pub record: JobRecord,
    /// Whether this call recorded it.
    pub created: bool,
}

/// A page of [`JobStore::list_jobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPage {
    /// The jobs of the page that pass the filter, in key order.
    pub jobs: Vec<JobRecord>,
    /// The keys of the page's rows a later build wrote, left out (and logged).
    pub skipped: Vec<String>,
    /// Where the next page begins; `None` on the last page.
    pub resume: Option<ResumeToken>,
}

/// The job, allocation and mark tables over a state store. Every operation that writes is one
/// [`Store::txn`]: it commits fully or returns an error having written nothing, and the
/// record it returns is what it committed. Nothing is cached: each operation reads the rows
/// it needs, decides, and commits with the versions it read as preconditions, so a row that
/// changed in between makes the operation [`JobStoreError::Stale`].
pub struct JobStore<S, I = RandomIds> {
    store: Arc<S>,
    ids: I,
    schema: u32,
    /// How long an ended job is kept: applied when a job ends.
    retention: Retention,
    /// How long a freed allocation's row is kept after its free: applied by the tick.
    allocation_retention: Retention,
}

/// `Err(error)` unless `ok`.
fn ensure(ok: bool, error: JobStoreError) -> Result<(), JobStoreError> {
    if ok { Ok(()) } else { Err(error) }
}

/// One row a caller writes beside a free: its key and its value.
pub type Row = (String, Vec<u8>);

/// What gives the rows written beside a free ([`JobStore::free_with`],
/// [`JobStore::end_and_free_with`]), from the allocation as it is freed.
pub type Beside<'a> = dyn Fn(&AllocationRecord) -> Vec<Row> + 'a;

/// `txn` with a put of each of `rows`, whatever its key holds.
fn with_rows(txn: Txn, rows: Vec<Row>) -> Txn {
    rows.into_iter()
        .fold(txn, |txn, (key, value)| txn.put(key, value, Cond::Any))
}

/// The schema version a `meta/jobs/schema` row holds.
fn schema_of(row: Option<Versioned>) -> Result<u32, JobStoreError> {
    let corrupt = |why| JobStoreError::Corrupt {
        key: keys::SCHEMA.to_owned(),
        why,
    };
    let value = row.ok_or(corrupt(DecodeError::Missing { tag: 0 }))?.value;
    let bytes: [u8; 4] = value
        .as_slice()
        .try_into()
        .map_err(|_| corrupt(DecodeError::BadLength { tag: 0 }))?;
    Ok(u32::from_be_bytes(bytes))
}

/// `record`, read from the row `key`, when the key is `own`, the key the record names itself
/// by: a row under another job's key is [`DecodeError::WrongKey`].
fn same_key<T>(key: &str, own: String, record: T) -> Result<T, DecodeError> {
    if key == own {
        Ok(record)
    } else {
        Err(DecodeError::WrongKey)
    }
}

/// The job the row `key` holds.
fn job_at(key: &str, value: &[u8]) -> Result<JobRecord, JobStoreError> {
    decode_job(value)
        .and_then(|job| same_key(key, keys::job(&job.id), job))
        .map_err(|why| JobStoreError::row(key, why))
}

/// The allocation the row `key` holds.
fn allocation_at(key: &str, value: &[u8]) -> Result<AllocationRecord, JobStoreError> {
    decode_allocation(value)
        .and_then(|allocation| same_key(key, keys::allocation(&allocation.job), allocation))
        .map_err(|why| JobStoreError::row(key, why))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The job store over `store`, minting ids from `ids`. The schema row is read; when there
    /// is none, this build's version is written with `Cond::Absent`, and when another store
    /// wrote one first, that one is read. A version above this build's is accepted, since rows
    /// change only by adding fields. Then the index backfill runs
    /// ([`JobStore::backfill_indexes`]), so the store opened reads indexes that agree with its
    /// tables. An error of the schema step means nothing was written; one of the backfill may
    /// follow committed batches of it, which the next open completes.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store fails; [`JobStoreError::Corrupt`] when the
    /// schema row is not four bytes; as [`JobStore::backfill_indexes`].
    pub fn new(store: Arc<S>, ids: I) -> Result<JobStore<S, I>, JobStoreError> {
        let schema = match store.get(keys::SCHEMA)? {
            Some(row) => schema_of(Some(row))?,
            None => {
                let mark = Txn::new().put(
                    keys::SCHEMA,
                    SCHEMA_VERSION.to_be_bytes().to_vec(),
                    Cond::Absent,
                );
                match store.txn(mark)? {
                    Ok(_) => SCHEMA_VERSION,
                    // Another store wrote the row between the read and the commit.
                    Err(_) => schema_of(store.get(keys::SCHEMA)?)?,
                }
            }
        };
        let jobs = JobStore {
            store,
            ids,
            schema,
            retention: Retention::DEFAULT,
            allocation_retention: Retention::ALLOCATION_DEFAULT,
        };
        jobs.backfill_indexes()?;
        Ok(jobs)
    }

    /// The same job store keeping each job that ends from now on for `retention` after its
    /// end, in place of [`Retention::DEFAULT`]. The window is applied when a job ends: a job
    /// that ended before keeps the retention end it was given.
    #[must_use]
    pub fn with_retention(self, retention: Retention) -> JobStore<S, I> {
        JobStore { retention, ..self }
    }

    /// How long a job that ends from now on is kept after its end.
    #[must_use]
    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// The same job store keeping each freed allocation's row for `retention` after its free,
    /// in place of [`Retention::ALLOCATION_DEFAULT`]; [`JobStore::tick`] deletes a freed row once that
    /// window has passed by the store's clock. Unlike a job's retention end, nothing is
    /// stored: the window applies to every freed row, however long ago it was freed.
    #[must_use]
    pub fn with_allocation_retention(self, retention: Retention) -> JobStore<S, I> {
        JobStore {
            allocation_retention: retention,
            ..self
        }
    }

    /// How long a freed allocation's row is kept after its free.
    #[must_use]
    pub fn allocation_retention(&self) -> Retention {
        self.allocation_retention
    }

    /// The schema version the store holds, which a later build may have raised.
    #[must_use]
    pub fn stored_schema(&self) -> u32 {
        self.schema
    }

    /// Records a job as queued (design section 5.3, steps 2 and 4).
    ///
    /// With an id: when a job with that id exists, the same job (every field of the spec;
    /// the priority and the client are not compared) is answered from the stored record, in
    /// whatever state it is now and with the client that recorded it, with `created: false`;
    /// a different one is [`JobStoreError::JobIdConflict`]. The files of an ended job are
    /// digests ([`crate::JobFiles`]), so for one the submitted files are compared by the
    /// SHA-256 of each, with the same names.
    /// Without an id: an id is minted, and minted again if taken, at most [`MINT_TRIES`]
    /// times.
    ///
    /// # Errors
    /// [`JobStoreError::JobIdConflict`], [`JobStoreError::IdSpaceExhausted`],
    /// [`JobStoreError::UnknownRow`] when the id's row is one a later build wrote,
    /// [`JobStoreError::Store`] or [`JobStoreError::Mint`].
    pub fn insert_queued(&self, job: NewJob) -> Result<Inserted, JobStoreError> {
        let NewJob {
            id,
            spec,
            priority,
            client,
        } = job;
        match id {
            Some(id) => self.insert_with_id(id, spec, priority, client),
            None => self.insert_minted(&spec, priority, client.as_ref()),
        }
    }

    fn queued_record(
        &self,
        id: JobId,
        spec: JobSpec,
        priority: i32,
        client: Option<ClientName>,
    ) -> JobRecord {
        JobRecord {
            id,
            spec,
            priority,
            state: JobState::Queued,
            submitted_at: self.store.now(),
            placed_at: None,
            started_at: None,
            ended_at: None,
            host: None,
            cancel_requested: false,
            cancel_requested_at: None,
            result: None,
            retention_ends: None,
            deadline_floor: None,
            client,
            failed_prepares: 0,
            prepare_bars: BTreeMap::new(),
            carried: Carried::default(),
        }
    }

    /// The stored job `id` when it is the same job as `spec`, `None` when there is none.
    fn same_job(&self, id: &JobId, spec: &JobSpec) -> Result<Option<Inserted>, JobStoreError> {
        let Some((record, _)) = self.read_job(id)? else {
            return Ok(None);
        };
        ensure(
            record.spec.same_job_as(spec),
            JobStoreError::JobIdConflict { id: id.clone() },
        )?;
        Ok(Some(Inserted {
            record,
            created: false,
        }))
    }

    fn insert_with_id(
        &self,
        id: JobId,
        spec: JobSpec,
        priority: i32,
        client: Option<ClientName>,
    ) -> Result<Inserted, JobStoreError> {
        if let Some(found) = self.same_job(&id, &spec)? {
            return Ok(found);
        }
        let record = self.queued_record(id, spec, priority, client);
        let txn = index::put_job(Txn::new(), None, &record, Cond::Absent)?;
        match self.store.txn(txn)? {
            Ok(_) => Ok(Inserted {
                record,
                created: true,
            }),
            // Another writer recorded the id between the read and the commit.
            Err(_) => self
                .same_job(&record.id, &record.spec)?
                .ok_or(JobStoreError::Stale {
                    id: record.id.clone(),
                }),
        }
    }

    fn insert_minted(
        &self,
        spec: &JobSpec,
        priority: i32,
        client: Option<&ClientName>,
    ) -> Result<Inserted, JobStoreError> {
        for _ in 0..MINT_TRIES {
            let id = JobId::minted(self.ids.mint()?);
            let record = self.queued_record(id, spec.clone(), priority, client.cloned());
            let txn = index::put_job(Txn::new(), None, &record, Cond::Absent)?;
            if self.store.txn(txn)?.is_ok() {
                return Ok(Inserted {
                    record,
                    created: true,
                });
            }
        }
        Err(JobStoreError::IdSpaceExhausted { tries: MINT_TRIES })
    }

    /// Places a queued job (design section 5.3, step 5): in one transaction the job becomes placed
    /// on `host`, and its allocation is written as booked with a newly minted allocation id,
    /// holding the host ports the job declares (sorted, each once). The job must still be queued at
    /// the version read, and its allocation row must still be absent, or freed (a requeued job's
    /// earlier allocation, which the new one overwrites), at the version read. The new allocation
    /// id differs from the freed row's: a source that repeats it is drawn again, at most
    /// [`MINT_TRIES`] times. The job's mark, if it has one, is left as it is:
    /// [`JobStore::place_with`] places a job and drops its mark in the same transaction. For a
    /// job that runs on the host itself ([`crate::Isolation::Host`]) the same transaction also
    /// puts `taint/<host>`, which must be absent, so a host a host job already tainted makes
    /// the placement stale.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is not
    /// queued; [`JobStoreError::AllocationHeld`] when its allocation is not freed;
    /// [`JobStoreError::IdSpaceExhausted`] when every allocation id drawn was the freed row's;
    /// [`JobStoreError::Stale`] when either row changed before the commit; as for any read and
    /// write.
    pub fn place(
        &self,
        id: &JobId,
        host: HostId,
        booked: Booked,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let (txn, placed, allocation) =
            self.placement_txn(id, host, booked, Placement::default())?;
        self.commit(id, txn)?;
        Ok((placed, allocation))
    }

    /// Moves a job from `expected` to `next` where `next` has not ended: placed to starting,
    /// and starting to running, which sets `started_at`. A job ends only through
    /// [`JobStore::end_job`], which writes its result and retention end with the end.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is not in
    /// `expected`; [`JobStoreError::IllegalTransition`] for a move the design does not name
    /// (out of an end, or to queued or placed, which only [`JobStore::requeue_failed_prepare`]
    /// and [`JobStore::place`] do) and for a move to an end, which only
    /// [`JobStore::end_job`] makes; [`JobStoreError::Stale`]; as for any read and write.
    pub fn transition(
        &self,
        id: &JobId,
        expected: JobState,
        next: JobState,
    ) -> Result<JobRecord, JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        ensure(
            job.state == expected,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected,
                found: job.state,
            },
        )?;
        ensure(
            may_move(job.state, next) & !next.is_ended(),
            JobStoreError::IllegalTransition {
                id: id.clone(),
                from: job.state,
                to: next,
            },
        )?;
        let now = self.store.now();
        let moved = JobRecord {
            state: next,
            started_at: (next == JobState::Running)
                .then_some(now)
                .or(job.started_at),
            ..job.clone()
        };
        let txn = index::put_job(Txn::new(), Some(&job), &moved, Cond::Version(job_version))?;
        self.commit(id, txn)?;
        Ok(moved)
    }

    /// The one requeue (design section 5.4): a placed whole-host job whose prepare failed goes
    /// back to queued, with no host, and its allocation is freed with
    /// [`EndReason::FailedPrepare`] in the same transaction (no container was made, so there
    /// is nothing for the host to confirm). Its next placement gets a new allocation id. The
    /// job's failed-prepare count and bars are kept as they are, and its mark is not touched:
    /// [`JobStore::fail_prepare_with`] is the requeue that counts the failure, bars the host and
    /// drops the mark, and can end the job instead.
    ///
    /// # Errors
    /// [`JobStoreError::UnexpectedState`] when the job is not placed;
    /// [`JobStoreError::IllegalTransition`] when it does not need the whole host or a cancel
    /// was asked for (it is then cancelled, not requeued); [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::AllocationNotBooked`] when the allocation is already releasing or
    /// freed (its first end stands);
    /// [`JobStoreError::Stale`]; as for any read and write.
    pub fn requeue_failed_prepare(
        &self,
        id: &JobId,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let ((job, job_version), (allocation, allocation_version)) = self.prepare_rows(id)?;
        let now = self.store.now();
        let queued = JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            ..job.clone()
        };
        let freed = AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::FailedPrepare),
            ..allocation.clone()
        };
        let txn = index::put_job(Txn::new(), Some(&job), &queued, Cond::Version(job_version))?;
        let txn = index::put_allocation(
            txn,
            Some(&allocation),
            &freed,
            Cond::Version(allocation_version),
        )?;
        self.commit(id, txn)?;
        Ok((queued, freed))
    }

    /// The client's cancel (design section 5.3): a queued job is cancelled at once (it has no
    /// allocation to release), ended as [`JobStore::end_job`] ends it, with a result of
    /// reason cancelled and no exit code or output, and its retention end, and its mark, if it
    /// has one, deleted in the same transaction (at the version read, or its absence a
    /// precondition when there is none, so a mark written in between makes the cancel stale);
    /// a placed, starting or running job gets the cancel intent and nothing else, the job
    /// ending when the host reports it stopped; an ended job, or one whose cancel was already
    /// asked for, is returned as it is, unwritten.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::Stale`]; as for any read and write.
    pub fn request_cancel(&self, id: &JobId) -> Result<JobRecord, JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        if job.state.is_ended() | job.cancel_requested {
            return Ok(job);
        }
        let now = self.store.now();
        let asked = JobRecord {
            cancel_requested: true,
            cancel_requested_at: Some(now),
            ..job.clone()
        };
        let (txn, next) = if asked.state == JobState::Queued {
            let cancelled = JobEnd::Cancelled {
                exit_code: None,
                output: Output::default(),
            };
            let (txn, next) = self.end_txn(asked, job_version, cancelled, now)?;
            (self.without_mark(txn, id)?, next)
        } else {
            let txn = index::put_job(Txn::new(), Some(&job), &asked, Cond::Version(job_version))?;
            (txn, asked)
        };
        self.commit(id, txn)?;
        Ok(next)
    }

    /// Moves the allocation of a job that ended, or whose cancel was asked for, from booked to
    /// releasing (design section 6.2). An allocation already releasing or freed is returned
    /// as it is, unwritten.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`], [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::NotReleasable`] when the job has neither ended nor been asked to
    /// cancel; [`JobStoreError::Stale`]; as for any read and write.
    pub fn mark_releasing(&self, id: &JobId) -> Result<AllocationRecord, JobStoreError> {
        let (job, _) = self.job_row(id)?;
        let (allocation, version) = self.allocation_row(id)?;
        ensure(
            job.state.is_ended() | job.cancel_requested,
            JobStoreError::NotReleasable { id: id.clone() },
        )?;
        if allocation.step != AllocationStep::Booked {
            return Ok(allocation);
        }
        let releasing = AllocationRecord {
            step: AllocationStep::Releasing,
            ..allocation.clone()
        };
        let txn = index::put_allocation(
            Txn::new(),
            Some(&allocation),
            &releasing,
            Cond::Version(version),
        )?;
        self.commit(id, txn)?;
        Ok(releasing)
    }

    /// Frees a job's allocation (design section 6.2): booked or releasing becomes freed, with
    /// the time and `reason`. The row books nothing and is kept until [`JobStore::tick`]
    /// deletes it, the store's allocation retention after the free. An allocation already freed
    /// is returned as it is, unwritten: the first reason stands.
    ///
    /// # Errors
    /// [`JobStoreError::NoAllocation`]; [`JobStoreError::Stale`]; as for any read and write.
    pub fn free(&self, id: &JobId, reason: EndReason) -> Result<AllocationRecord, JobStoreError> {
        self.free_with(id, reason, &|_| Vec::new())
    }

    /// [`JobStore::free`], with the rows `beside` gives for the freed allocation written in
    /// the same transaction, each as `(key, value)` whatever the key holds: the free and those
    /// rows commit together or not at all. An allocation already freed is returned as it is,
    /// and `beside` is not called: nothing is written.
    ///
    /// # Errors
    /// As [`JobStore::free`].
    pub fn free_with(
        &self,
        id: &JobId,
        reason: EndReason,
        beside: &Beside<'_>,
    ) -> Result<AllocationRecord, JobStoreError> {
        let (allocation, version) = self.allocation_row(id)?;
        if allocation.step == AllocationStep::Freed {
            return Ok(allocation);
        }
        let freed = AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(self.store.now()),
            end_reason: Some(reason),
            ..allocation.clone()
        };
        let txn = index::put_allocation(
            Txn::new(),
            Some(&allocation),
            &freed,
            Cond::Version(version),
        )?;
        self.commit(id, with_rows(txn, beside(&freed)))?;
        Ok(freed)
    }

    /// The job `id`, if there is one.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] when a later build wrote its row,
    /// [`JobStoreError::Corrupt`] when it does not parse, [`JobStoreError::Store`].
    pub fn get_job(&self, id: &JobId) -> Result<Option<JobRecord>, JobStoreError> {
        Ok(self.read_job(id)?.map(|(record, _)| record))
    }

    /// The allocation of the job `id`, if it has one (freed ones included).
    ///
    /// # Errors
    /// As [`JobStore::get_job`].
    pub fn get_allocation(&self, id: &JobId) -> Result<Option<AllocationRecord>, JobStoreError> {
        Ok(self.read_allocation(id)?.map(|(record, _)| record))
    }

    /// A page of jobs in id order, at most `limit` rows of the table (and at most
    /// [`MAX_PAGE`]), from the start or from `resume`, keeping those in `state` when one is
    /// given. Filtering happens within the page, so a page can hold fewer jobs than the limit,
    /// or none, and still have a next page. A row a later build wrote is left out, logged and
    /// named in [`JobPage::skipped`].
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse; [`JobStoreError::Store`].
    pub fn list_jobs(
        &self,
        state: Option<JobState>,
        resume: Option<&ResumeToken>,
        limit: NonZeroUsize,
    ) -> Result<JobPage, JobStoreError> {
        let scan = ScanRange::new(keys::JOB, limit.min(MAX_PAGE));
        let scan = match resume {
            Some(token) => scan.resume(token),
            None => scan,
        };
        let page = self.store.scan_range(scan)?;
        let mut jobs = Vec::new();
        let mut skipped = Vec::new();
        for (key, row) in page.rows {
            match decode_job(&row.value).and_then(|job| same_key(&key, keys::job(&job.id), job)) {
                Ok(job) => {
                    if state.is_none_or(|state| state == job.state) {
                        jobs.push(job);
                    }
                }
                Err(DecodeError::Newer) => {
                    tracing::warn!(%key, "a job row a later build wrote is left out of the list");
                    skipped.push(key);
                }
                Err(why) => return Err(JobStoreError::Corrupt { key, why }),
            }
        }
        Ok(JobPage {
            jobs,
            skipped,
            resume: page.resume,
        })
    }

    /// What the live (booked and releasing) allocations on `host` book, summed. It reads
    /// the rows of the host's on-host entries and the allocation rows this build cannot read.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when an allocation row
    /// cannot be read (a booking that cannot be counted is not taken as none);
    /// [`JobStoreError::BookingOverflow`]; [`JobStoreError::Store`].
    pub fn booked_on(&self, host: &HostId) -> Result<HostBooking, JobStoreError> {
        let mut total = HostBooking::default();
        let on_host = keys::on_host_prefix(host);
        let spans = [
            index::Span::all(&on_host),
            index::Span::all(keys::UNREAD_ALLOCATION),
        ];
        for (key, row) in self.indexed_rows(&spans)? {
            let allocation = allocation_at(&key, &row.value)?;
            if allocation.host == *host && allocation.step != AllocationStep::Freed {
                total = total
                    .plus(&allocation.booked)
                    .ok_or(JobStoreError::BookingOverflow { host: host.clone() })?;
            }
        }
        Ok(total)
    }

    // ---- rows --------------------------------------------------------------------------

    fn read_job(&self, id: &JobId) -> Result<Option<(JobRecord, Version)>, JobStoreError> {
        let key = keys::job(id);
        self.store
            .get(&key)?
            .map(|row| Ok((job_at(&key, &row.value)?, row.version)))
            .transpose()
    }

    fn job_row(&self, id: &JobId) -> Result<(JobRecord, Version), JobStoreError> {
        self.read_job(id)?
            .ok_or(JobStoreError::NotFound { id: id.clone() })
    }

    fn read_allocation(
        &self,
        id: &JobId,
    ) -> Result<Option<(AllocationRecord, Version)>, JobStoreError> {
        let key = keys::allocation(id);
        self.store
            .get(&key)?
            .map(|row| Ok((allocation_at(&key, &row.value)?, row.version)))
            .transpose()
    }

    /// An allocation id for a placement, other than `earlier`, the id of the freed allocation
    /// the placement replaces.
    fn fresh_allocation_id(
        &self,
        earlier: Option<AllocationId>,
    ) -> Result<AllocationId, JobStoreError> {
        for _ in 0..MINT_TRIES {
            let drawn = AllocationId::from_bits(self.ids.mint()?);
            if Some(drawn) != earlier {
                return Ok(drawn);
            }
        }
        Err(JobStoreError::IdSpaceExhausted { tries: MINT_TRIES })
    }

    fn allocation_row(&self, id: &JobId) -> Result<(AllocationRecord, Version), JobStoreError> {
        self.read_allocation(id)?
            .ok_or(JobStoreError::NoAllocation { id: id.clone() })
    }

    /// Commits `txn`, whose preconditions are the versions of the job `id`'s rows the caller
    /// read: a conflict is [`JobStoreError::Stale`].
    fn commit(&self, id: &JobId, txn: Txn) -> Result<Commit, JobStoreError> {
        self.store
            .txn(txn)?
            .map_err(|_| JobStoreError::Stale { id: id.clone() })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::fake::Fake;
    use js_state::{Cond, ManualClock, Store, Txn, Version, Versioned};

    use super::{JobStore, SCHEMA_VERSION, schema_of};
    use crate::codec::DecodeError;
    use crate::error::JobStoreError;
    use crate::ids::RandomIds;
    use crate::keys;

    fn fake() -> Arc<Fake> {
        Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            js_state::fake::Flaw::None,
        ))
    }

    // Catches a store that writes the schema row again over a later build's, or refuses a
    // version above its own.
    #[test]
    fn the_schema_row_is_written_once_and_a_higher_version_is_accepted() {
        let store = fake();
        let jobs = JobStore::new(store.clone(), RandomIds).expect("opens");
        assert_eq!(jobs.stored_schema(), SCHEMA_VERSION);
        let row = store.get(keys::SCHEMA).expect("reads").expect("written");
        assert_eq!(row.value, SCHEMA_VERSION.to_be_bytes());

        store
            .txn(Txn::new().put(keys::SCHEMA, 7u32.to_be_bytes().to_vec(), Cond::Any))
            .expect("reachable")
            .expect("commits");
        let later = JobStore::new(store.clone(), RandomIds).expect("a higher version opens");
        assert_eq!(later.stored_schema(), 7);
        let row = store.get(keys::SCHEMA).expect("reads").expect("kept");
        assert_eq!(
            row.value,
            7u32.to_be_bytes(),
            "the later build's row is kept"
        );
    }

    // Catches a schema row of another width read as some version rather than refused.
    #[test]
    fn a_schema_row_that_is_not_four_bytes_is_corrupt() {
        let store = fake();
        store
            .txn(Txn::new().put(keys::SCHEMA, vec![1u8, 2, 3], Cond::Any))
            .expect("reachable")
            .expect("commits");
        let refused = JobStore::new(store, RandomIds).err();
        assert_eq!(
            refused,
            Some(JobStoreError::Corrupt {
                key: keys::SCHEMA.to_owned(),
                why: DecodeError::BadLength { tag: 0 },
            })
        );
        let missing = schema_of(None);
        assert_eq!(
            missing,
            Err(JobStoreError::Corrupt {
                key: keys::SCHEMA.to_owned(),
                why: DecodeError::Missing { tag: 0 },
            })
        );
        let read = schema_of(Some(Versioned {
            value: 3u32.to_be_bytes().to_vec(),
            version: Version(1),
            expires_at: None,
        }));
        assert_eq!(read, Ok(3));
    }

    /// A source whose every draw fails.
    struct Broken;

    impl crate::ids::IdSource for Broken {
        fn mint(&self) -> Result<[u8; 16], crate::ids::MintError> {
            Err(crate::ids::MintError {
                code: std::num::NonZeroU32::MIN,
            })
        }
    }

    // Catches a failed draw turned into an id (all zeros, say), or one that leaves a row behind.
    #[test]
    fn a_source_that_cannot_mint_is_an_error_and_writes_nothing() {
        let store = fake();
        let jobs = JobStore::new(store.clone(), Broken).expect("opens");
        let failed = Err(JobStoreError::Mint(crate::ids::MintError {
            code: std::num::NonZeroU32::MIN,
        }));
        let job = |id| crate::NewJob {
            id,
            spec: crate::suite::full_spec(),
            priority: 0,
            client: None,
        };
        assert_eq!(jobs.insert_queued(job(None)).map(drop), failed);
        let id = crate::JobId::parse("a").expect("an id");
        jobs.insert_queued(job(Some(id.clone())))
            .expect("a job with an id mints nothing");
        assert_eq!(
            jobs.place(&id, crate::suite::host("host-a"), crate::suite::share())
                .map(drop),
            failed
        );
        assert_eq!(
            store.scan_prefix(keys::ALLOCATION).expect("reads"),
            Vec::new()
        );
        assert_eq!(store.scan_prefix(keys::JOB).expect("reads").len(), 1);
    }

    /// A log sink a test reads back.
    #[derive(Clone, Default)]
    pub(super) struct Lines(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
        type Writer = Lines;

        fn make_writer(&'a self) -> Lines {
            self.clone()
        }
    }

    // Catches a row a later build wrote that the retention tick passes over without a word,
    // or deletes: the tick deletes nothing, keeps the row, and the warning names its key.
    #[test]
    fn a_tick_logs_each_row_it_leaves() {
        use std::io::Write as _;

        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .finish();
        let store = fake();
        store
            .txn(Txn::new().put("job/later", vec![crate::codec::FORMAT + 1], Cond::Any))
            .expect("reachable")
            .expect("commits");
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        let deleted = tracing::subscriber::with_default(subscriber, || jobs.tick());
        assert_eq!(deleted, Ok(Vec::new()));
        assert!(store.get("job/later").expect("reads").is_some());
        lines.clone().flush().expect("flushes");
        let said = String::from_utf8(lines.0.lock().expect("unpoisoned").clone()).expect("text");
        assert!(
            said.contains("a job row a later build wrote is left to its retention")
                & said.contains("key=job/later"),
            "{said}"
        );
    }

    // Catches a row a later build wrote that a list leaves out without a word: the warning
    // names the row's key.
    #[test]
    fn a_list_logs_each_row_it_skips() {
        use std::io::Write as _;

        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .finish();
        let store = fake();
        store
            .txn(Txn::new().put("job/later", vec![crate::codec::FORMAT + 1], Cond::Any))
            .expect("reachable")
            .expect("commits");
        let jobs = JobStore::new(store, RandomIds).expect("opens");
        let page = tracing::subscriber::with_default(subscriber, || {
            jobs.list_jobs(None, None, std::num::NonZeroUsize::MIN)
                .expect("lists")
        });
        assert_eq!(page.skipped, ["job/later"]);
        lines.clone().flush().expect("flushes");
        let said = String::from_utf8(lines.0.lock().expect("unpoisoned").clone()).expect("text");
        assert!(
            said.contains("a job row a later build wrote is left out of the list")
                & said.contains("key=job/later"),
            "{said}"
        );
    }
}
