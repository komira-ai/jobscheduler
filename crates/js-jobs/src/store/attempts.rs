//! A job's next attempt ([`JobStore::retry_txn`]) and the row of an attempt that ran
//! ([`JobStore::attempt_row`]) (`docs/design/job-retry-and-completion-callback.md`, sections
//! 3.2, 3.5, 3.7 and 3.8).
//!
//! The retry is the one move of a live job back to queued after it was placed for a run, an
//! operation of its own beside [`JobStore::end_job`] and outside the moves a job's state
//! allows, as the requeues before a run are: nothing else moves a job that ran back to the
//! queue, and only a job whose retry policy has attempts left can take it. The decision of
//! whether an attempt retries is the caller's (`js_types::decide`); the store carries it out
//! in one commit, so the job never reads as ended between two attempts.

use js_state::{Cond, Store, StoreTime, Txn};

use super::index::put_job;
use super::{JobStore, ensure};
use crate::attempt::{AttemptRecord, AttemptRow, AttemptTrail, RETRY_HOST_BAR};
use crate::codec::decode_attempt_row;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{Exclusion, JobRecord, JobState};

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Ends the current attempt of the job `id` as `end` says and queues its next attempt,
    /// placeable from `not_before`, in one commit beside [`JobStore::end_job`]'s, and answers
    /// the job as queued. In that commit:
    ///
    /// - the attempt goes into the job's history as retried, its host into the hosts that held
    ///   an attempt, and its row is written (`attempt/<id>/<n>`); the job's host, placement and
    ///   start times, result and refusal code, which were the attempt's, are cleared;
    /// - the counts a requeue before a run keeps (the failed prepares, the held and the
    ///   capacity refusals) start again from none, so the next attempt has its own bounds, and
    ///   the attempt's host is kept off the job until the store's clock now plus
    ///   [`RETRY_HOST_BAR`], in place of any bar it had;
    /// - the job is queued again with `not_before` as its
    ///   [`AttemptTrail::retry_not_before`], keeping its priority and its submit time, so its
    ///   place in the queue;
    /// - its allocation moves to releasing and its start leaves its host's dispatch queue, with
    ///   a stop queued where the start may have reached the host for an end the server decided
    ///   (a start that timed out, a host lost), exactly as [`JobStore::end_job`] does.
    ///
    /// The job keeps its files' contents and gets no retention end: it has not ended. The
    /// attempt number stays the one that ended until the next placement counts the next.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`;
    /// [`JobStoreError::IllegalTransition`] from the job's state to queued when the job is not
    /// placed, starting or running, a cancel was asked for (a cancel wins: the job never starts
    /// another attempt), or its retry policy leaves no attempt after the current one (a job
    /// without a policy has one attempt); [`JobStoreError::Stale`] when the job or its
    /// allocation changed before the commit; as for any read and write.
    pub fn retry_txn(
        &self,
        id: &JobId,
        end: AttemptRecord,
        not_before: StoreTime,
    ) -> Result<JobRecord, JobStoreError> {
        let (job, version) = self.job_row(id)?;
        let live = matches!(
            job.state,
            JobState::Placed | JobState::Starting | JobState::Running
        );
        let number = job.attempt.number();
        let left = job
            .spec
            .retry
            .is_some_and(|policy| policy.max_attempts > number);
        ensure(
            live & left & !job.cancel_requested,
            JobStoreError::IllegalTransition {
                id: id.clone(),
                from: job.state,
                to: JobState::Queued,
            },
        )?;
        let now = self.store.now()?;
        let stop = !end.told_by_host();
        let attempt = AttemptTrail::closed(&job, end, now, true).waiting(not_before);
        let bar = Exclusion::Until(now.saturating_add(RETRY_HOST_BAR));
        let mut prepare_bars = job.prepare_bars.clone();
        prepare_bars.extend(job.host.clone().map(|host| (host, bar)));
        let queued = JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            started_at: None,
            result: None,
            start_failure_detail: None,
            failed_prepares: 0,
            prepare_bars,
            held_refusals: Vec::new(),
            capacity_refusals: Vec::new(),
            attempt,
            ..job.clone()
        };
        let txn = put_job(Txn::new(), Some(&job), &queued, Cond::Version(version))?;
        let txn = self.released(txn, &job, stop, now)?;
        self.commit(id, txn)?;
        Ok(queued)
    }

    /// The row of attempt `attempt` (counted from 1) of the job `id`: its host and when it
    /// was placed and ended. `None` for an attempt that never ran, or has not ended: 0, one
    /// past the job's current attempt, the current attempt while it runs, and an attempt the
    /// job ended while it waited for. A row an earlier job of the same id left (placed before
    /// this job was submitted) is none of this job's, and is `None` too.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`; [`JobStoreError::UnknownRow`]
    /// or [`JobStoreError::Corrupt`] when the job's row or the attempt's cannot be read (an
    /// attempt's row that names another job or number is corrupt); a store error when a row
    /// cannot be read.
    pub fn attempt_row(
        &self,
        id: &JobId,
        attempt: u32,
    ) -> Result<Option<AttemptRow>, JobStoreError> {
        let (job, _) = self.job_row(id)?;
        if (attempt == 0) | (attempt > job.attempt.number()) {
            return Ok(None);
        }
        let key = keys::attempt(id, attempt);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        let (named, number, found) =
            decode_attempt_row(&row.value).map_err(|why| JobStoreError::row(&key, why))?;
        let corrupt = JobStoreError::Corrupt {
            key,
            why: crate::DecodeError::WrongKey,
        };
        ensure((named == *id) & (number == attempt), corrupt)?;
        Ok((found.placed_at >= job.submitted_at).then_some(found))
    }
}
