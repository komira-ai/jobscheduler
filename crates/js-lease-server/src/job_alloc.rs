//! The one way the server frees an allocation (design section 6.2).
//!
//! An allocation is freed only when the host confirms the job's container is gone
//! ([`Freed::HostConfirmed`]) or the provider reads the host absent
//! ([`Freed::ProviderAbsent`]). A host that is only silent frees nothing: unknown is not
//! absent. Every path that frees is in this module: [`free_allocation`], which records why
//! in the allocation's row from the job's end, [`answer_retired`], which ends a cancelled
//! job and frees its allocation in the same commit ([`cancel_and_free`] is that end, which a
//! whole-host job whose host has left `preparing` takes too, [`crate::job_whole`]), and
//! [`lose_on_absent`], which ends a job whose host the provider read absent as lost and frees
//! its allocation in the same commit. A whole-host job whose prepare failed is freed by
//! [`js_jobs::JobStore::fail_prepare_with`] with the same history row ([`history`]).
//!
//! Each free also records the allocation in the usage history ([`history`]): the row of
//! `usage/ended/<ended-at>/<job id>` and its index entry, written by
//! [`js_usage::ended_writes`] in the transaction that frees, so a free is never without its
//! row and a row never without its free. An allocation already freed is not freed again,
//! so it is recorded once. A lease's booking is recorded by the registry when it stops it.

use std::time::Duration;

use js_jobs::{
    AllocationRecord, AllocationStep, Booked, EndReason, IdSource, JobEnd, JobId, JobRecord,
    JobState, JobStore, JobStoreError, Output, ResultReason, Row, StartFailure,
};
use js_state::Store;
use js_types::HostId;
use js_usage::{AllocationId, Booked as UsageBooked, DrainOutcome, EndedReason, EndedRow};

/// What says an allocation's container is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freed {
    /// The host listed the job's container gone: its result was acknowledged and its
    /// cleanup checks ran clean, so the agent dropped it from its listing.
    HostConfirmed,
    /// The provider read the host absent.
    ProviderAbsent,
}

/// Frees the allocation of the job `id` because `freed` (design section 6.2): booked or
/// releasing becomes freed in one commit, with the reason the job's end gives it. An
/// allocation already freed is returned as it is, unwritten: the first reason stands.
///
/// A host's confirmation frees only an allocation whose job has ended or was asked to
/// cancel, since a job still meant to run is not gone because its host stopped listing it;
/// a provider's absence frees whatever the job's state (the job is then lost, which its own
/// path commits).
///
/// The free and the allocation's usage history row ([`history`]) are one commit.
///
/// # Errors
/// [`JobStoreError::NotReleasable`] for a confirmation of a job that has neither ended nor
/// been asked to cancel (nothing written); otherwise as [`JobStore::free`].
pub(crate) fn free_allocation<S: Store, I: IdSource>(
    store: &JobStore<S, I>,
    id: &JobId,
    freed: Freed,
) -> Result<AllocationRecord, JobStoreError> {
    let job = store.get_job(id)?;
    let reason = match freed {
        Freed::ProviderAbsent => EndReason::HostAbsent,
        Freed::HostConfirmed => match &job {
            Some(job) if !job.state.is_ended() & !job.cancel_requested => {
                return Err(JobStoreError::NotReleasable { id: id.clone() });
            }
            Some(job) => reason_of(job),
            // A job's row is deleted at its retention end while its allocation stays as
            // history; such a job ended long ago, and how is in no row any longer.
            None => EndReason::Exited,
        },
    };
    store.free_with(id, reason, &|freed| history(job.as_ref(), freed))
}

/// Answers the host `host` retiring the job `id`: it was told to stop a job it does not hold,
/// and that answer counts as stopped and gone (design section 5.3, the cancel bullets, and
/// section 8, row 38). When the job's live allocation is on `host`:
///
/// - a job with a cancel intent that has not ended becomes cancelled and its allocation
///   freed in one commit, so the host is not left booked and the job is never started after;
/// - a job that has ended (a start timeout, say) has its allocation freed, as on any
///   confirmation.
///
/// Anything else (an id the store does not know, an allocation freed or on another host, a
/// job neither ended nor asked to cancel) is left as it is: nothing is revived or made.
///
/// # Errors
/// As [`JobStore::end_and_free`] and [`free_allocation`].
pub(crate) fn answer_retired<S: Store, I: IdSource>(
    store: &JobStore<S, I>,
    host: &HostId,
    id: &JobId,
) -> Result<(), JobStoreError> {
    let (job, allocation) = (store.get_job(id)?, store.get_allocation(id)?);
    let held_here = |allocation: &AllocationRecord| {
        (allocation.host == *host) & (allocation.step != AllocationStep::Freed)
    };
    let Some(job) = job.filter(|_| allocation.as_ref().is_some_and(held_here)) else {
        return Ok(());
    };
    if job.state.is_ended() {
        free_allocation(store, id, Freed::HostConfirmed)?;
    } else if job.cancel_requested {
        cancel_and_free(store, &job)?;
    }
    Ok(())
}

/// Ends `job`, which has a cancel intent, as cancelled and frees its allocation in one commit,
/// with the allocation's usage history row: the end of a job whose host holds no container for
/// it (a start never delivered, a whole-host prepare that has ended).
///
/// # Errors
/// As [`JobStore::end_and_free_with`].
pub(crate) fn cancel_and_free<S: Store, I: IdSource>(
    store: &JobStore<S, I>,
    job: &JobRecord,
) -> Result<(), JobStoreError> {
    let cancelled = JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    };
    let beside = |freed: &AllocationRecord| history(Some(job), freed);
    store.end_and_free_with(&job.id, cancelled, EndReason::Cancelled, &beside)?;
    Ok(())
}

/// Answers the provider reading the host of the job `id`'s live allocation absent (design
/// section 6.3, section 8 rows 28, 41 and 42): a job that has not ended becomes lost and its
/// allocation is freed for [`Freed::ProviderAbsent`] in one commit, with the allocation's usage
/// history row, whatever the job's cancel intent (lost wins: no host report exists, and the
/// record keeps the intent). A job that has ended, or whose row was deleted at its retention
/// end, keeps what it has and only its allocation is freed ([`free_allocation`]). Returns the
/// job this call made lost, if it made one.
///
/// # Errors
/// As [`JobStore::end_and_free`] and [`free_allocation`].
pub(crate) fn lose_on_absent<S: Store, I: IdSource>(
    store: &JobStore<S, I>,
    id: &JobId,
) -> Result<Option<JobRecord>, JobStoreError> {
    let live = store.get_job(id)?.filter(|job| !job.state.is_ended());
    let Some(job) = live else {
        free_allocation(store, id, Freed::ProviderAbsent)?;
        return Ok(None);
    };
    let beside = |freed: &AllocationRecord| history(Some(&job), freed);
    let reason = EndReason::HostAbsent;
    let (lost, _) = store.end_and_free_with(id, JobEnd::Lost, reason, &beside)?;
    // A job another writer ended between the read and the commit keeps that end.
    Ok(Some(lost).filter(|job| job.state == JobState::Lost))
}

/// The usage history rows of `freed`, the allocation of `job` (`None` for a job whose row
/// was deleted at its retention end), as [`js_usage::ended_writes`] gives them: named by the
/// job id (or, for a job id with a letter the history's ids do not take, by the allocation
/// id), on its host, with what it booked; queued from the job's submit to this placement,
/// run from its start to its end (zero for a job that never ran, unknown for a job whose
/// row is gone), ended at the free, for the reason it was freed.
pub(crate) fn history(job: Option<&JobRecord>, freed: &AllocationRecord) -> Vec<Row> {
    let end = freed.freed_at.unwrap_or(freed.created_at);
    let ran = |job: &JobRecord| {
        let ended = job.ended_at.unwrap_or(end);
        millis(ended.since(job.started_at.unwrap_or(ended)))
    };
    let hex = freed.allocation_id.to_string();
    let named = AllocationId::parse(freed.job.as_str()).or_else(|_| AllocationId::parse(&hex));
    let row = |id| EndedRow {
        id,
        profile: None,
        host: Some(freed.host.clone()),
        booked: Some(booked_of(freed.booked)),
        queued_ms: job.map(|job| millis(freed.created_at.since(job.submitted_at))),
        run_ms: job.map(ran),
        ended_reason: ended_reason(freed.end_reason),
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: millis(end.as_duration()),
        error_ms: None,
        used: None,
    };
    let rows = named.map(|id| js_usage::ended_writes(&row(id)).to_vec());
    rows.unwrap_or_default()
}

/// What `booked` comes to in the usage history.
fn booked_of(booked: Booked) -> UsageBooked {
    match booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            ..
        } => UsageBooked {
            whole_host: false,
            cpu_milli: cpu_millis,
            memory_bytes,
        },
        Booked::WholeHost => UsageBooked {
            whole_host: true,
            cpu_milli: 0,
            memory_bytes: 0,
        },
    }
}

/// The usage history's reason for an allocation freed for `reason`: an end the job came to
/// on its host is a release, a wall limit is a stop at it, an absent host is a loss, and a
/// job that never got to run failed to start.
fn ended_reason(reason: Option<EndReason>) -> EndedReason {
    match reason {
        Some(EndReason::WallLimit) => EndedReason::StoppedAtWallLimit,
        Some(EndReason::HostAbsent) => EndedReason::Lost,
        Some(EndReason::FailedPrepare | EndReason::StartTimeout | EndReason::Refused) => {
            EndedReason::FailedToStart
        }
        Some(EndReason::Exited | EndReason::Memory | EndReason::Cancelled) | None => {
            EndedReason::Released
        }
    }
}

/// `duration` in whole milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Why the allocation of `job`, ended or asked to cancel, is freed once its host confirms the
/// container gone.
fn reason_of(job: &JobRecord) -> EndReason {
    match job.state {
        JobState::Cancelled => EndReason::Cancelled,
        JobState::Lost => EndReason::HostAbsent,
        JobState::FailedToStart(StartFailure::StartTimeout) => EndReason::StartTimeout,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady) => EndReason::FailedPrepare,
        JobState::FailedToStart(
            StartFailure::AgentRefused | StartFailure::NoCapacity | StartFailure::InputUnavailable,
        ) => EndReason::Refused,
        JobState::Succeeded | JobState::Failed => {
            match job.result.as_ref().map(|result| result.reason) {
                Some(ResultReason::WallLimit) => EndReason::WallLimit,
                Some(ResultReason::Memory) => EndReason::Memory,
                Some(ResultReason::Cancelled) => EndReason::Cancelled,
                Some(ResultReason::Exit | ResultReason::InputFailed) | None => EndReason::Exited,
            }
        }
        // Not ended, with a cancel asked for: the stop is what ends it.
        JobState::Queued | JobState::Placed | JobState::Starting | JobState::Running => {
            EndReason::Cancelled
        }
    }
}

#[cfg(test)]
#[path = "job_alloc_tests.rs"]
mod tests;
