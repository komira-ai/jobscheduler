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
    AllocationRecord, AllocationStep, AttemptOutcome, AttemptRecord, Booked, EndReason, IdSource,
    JobEnd, JobId, JobRecord, JobState, JobStore, JobStoreError, Output, ResultReason, Row,
    StartFailure,
};
use js_state::{Store, StoreTime};
use js_types::{CpuSeries, HostId, MemorySeries};
use js_usage::{
    AllocationId, Booked as UsageBooked, DrainOutcome, EndedReason, EndedRow, ProfileKey, Used,
};

use crate::per_job::attempt_over;

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
/// A host's confirmation frees only an allocation whose job has ended, was asked to cancel, or
/// is queued again because a retry ended the attempt the allocation held
/// ([`crate::job_retry`]), since a job still meant to run is not gone because its host stopped
/// listing it; a provider's absence frees whatever the job's state (the job is then lost or
/// retried, which its own path commits).
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
            Some(job) if !attempt_over(job) & !job.cancel_requested => {
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
/// - a job that has ended (a start timeout, say), or whose attempt there a retry ended, has its
///   allocation freed, as on any confirmation.
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
    if attempt_over(&job) {
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
/// record keeps the intent). A job that has ended, one queued again whose previous attempt the
/// allocation held (a retry ended that attempt), and one whose row was deleted at its retention
/// end, keep what they have and only the allocation is freed ([`free_allocation`]). A job whose
/// attempt retries when its host is lost is not ended here: its path retries it first
/// ([`crate::job_loss::lose_absent`]). Returns the job this call made lost, if it made one.
///
/// # Errors
/// As [`JobStore::end_and_free`] and [`free_allocation`].
pub(crate) fn lose_on_absent<S: Store, I: IdSource>(
    store: &JobStore<S, I>,
    id: &JobId,
) -> Result<Option<JobRecord>, JobStoreError> {
    let live = store.get_job(id)?.filter(|job| !attempt_over(job));
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
/// id), on its host, with what it booked; queued from when its attempt became placeable (the
/// job's submit for the first) to this placement, run from the attempt's start to its end
/// ([`attempt_times`]: zero for an attempt that never ran, unknown for a job whose row is
/// gone), ended at the free, for the reason it was freed, with what it was measured to
/// use ([`used_of`]), its CPU series ([`series_of`]) and its memory series
/// ([`memory_series_of`]), with the name of its class as it is
/// ([`js_sched_placer::sizes::class_name`]: a full build target's name kept byte for byte),
/// under that name as its profile when it is a profile key ([`profile_of`]), and with the job's
/// attempt when it is not the first: the allocation is the job's current attempt's, since the
/// next attempt is placed only once it is freed.
pub(crate) fn history(job: Option<&JobRecord>, freed: &AllocationRecord) -> Vec<Row> {
    let end = freed.freed_at.unwrap_or(freed.created_at);
    let ran = |job: &JobRecord| {
        let (started_at, ended_at, _) = attempt_times(job);
        let ended = ended_at.unwrap_or(end);
        millis(ended.since(started_at.unwrap_or(ended)))
    };
    let hex = freed.allocation_id.to_string();
    let named = AllocationId::parse(freed.job.as_str()).or_else(|_| AllocationId::parse(&hex));
    let row = |id| EndedRow {
        id,
        profile: job.and_then(profile_of),
        host: Some(freed.host.clone()),
        booked: Some(booked_of(freed.booked)),
        queued_ms: job.map(|job| millis(freed.created_at.since(attempt_times(job).2))),
        run_ms: job.map(ran),
        ended_reason: ended_reason(freed.end_reason),
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: millis(end.as_duration()),
        error_ms: None,
        used: job.and_then(used_of),
        cpu_series: job.and_then(series_of),
        class: job.map(js_sched_placer::sizes::class_name),
        attempt: job
            .map(|job| job.attempt.number())
            .filter(|number| *number > 1),
        memory_series: job.and_then(memory_series_of),
    };
    let rows = named.map(|id| js_usage::ended_writes(&row(id)).to_vec());
    rows.unwrap_or_default()
}

/// When the current attempt of `job` (the one a freed allocation of it booked for) started,
/// ended and became placeable: from its entry in the job's history once it has ended there,
/// since a retry clears them from the job when it queues the next attempt; else the job's own
/// start and end, and its submit for a first attempt or the end of the backoff for a later one.
fn attempt_times(job: &JobRecord) -> (Option<StoreTime>, Option<StoreTime>, StoreTime) {
    let number = job.attempt.number();
    let closed = job.attempt.history().last();
    let closed = closed.filter(|attempt| attempt.number == number);
    let queued = job.attempt.retry_not_before().unwrap_or(job.submitted_at);
    closed.map_or((job.started_at, job.ended_at, queued), |attempt| {
        (
            attempt.started_at,
            Some(attempt.ended_at),
            attempt.queued_at,
        )
    })
}

/// The history key of `job`'s row: the name of its class
/// ([`js_sched_placer::sizes::class_name`]), the same for every run of what it builds and
/// never its id or image digest. `None` for a name that is no profile key (a `size-class`
/// label that starts with `.`, `_` or `-`, or a history key with a capital, a space, a slash, a
/// colon or over 128 bytes): the row's `class` keeps it.
fn profile_of(job: &JobRecord) -> Option<ProfileKey> {
    ProfileKey::parse(&js_sched_placer::sizes::class_name(job)).ok()
}

/// What `job` was measured to use, from the usage its host reported with its result: its peak
/// memory, and the CPU it used per second of its run (from its start to its end), as both the
/// highest and the mean CPU (a job's host reports one total, not one per minute; a run of no
/// time used none). `None` for a job with no result, or whose host reported no usage.
fn used_of(job: &JobRecord) -> Option<Used> {
    let result = job.result.as_ref()?;
    let usage = result.usage?;
    let timing = &result.timing;
    let ran = timing
        .started_at
        .map(|started| timing.ended_at.since(started));
    let ran = ran.unwrap_or_default().as_micros();
    let per_second = (usage.cpu_time.as_micros() * 1000).checked_div(ran);
    let cpu_milli = per_second.map_or(0, |cpu| u64::try_from(cpu).unwrap_or(u64::MAX));
    Some(Used {
        cpu_milli,
        memory_bytes: usage.peak_memory_bytes,
        cpu_mean_milli: cpu_milli,
    })
}

/// The CPU series `job`'s host reported with its result, as the usage history keeps it; `None`
/// for a job with no result and for one whose host reported none. It is kept as reported,
/// whether or not the host also reported a usage and whatever total CPU it comes to.
fn series_of(job: &JobRecord) -> Option<CpuSeries> {
    job.result.as_ref()?.cpu_series.clone()
}

/// The memory series `job`'s host reported with its result, as the usage history keeps it;
/// `None` for a job with no result and for one whose host reported none. It is kept as
/// reported, as the CPU series is, whatever peak memory the usage gives.
fn memory_series_of(job: &JobRecord) -> Option<MemorySeries> {
    job.result.as_ref()?.memory_series.clone()
}

/// What `booked` comes to in the usage history.
pub(crate) fn booked_of(booked: Booked) -> UsageBooked {
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
        // Queued again: a retry ended the attempt the allocation held, as its history's last
        // entry, marked retried, says. Otherwise a cancel was asked for, as for the states below.
        JobState::Queued => job
            .attempt
            .history()
            .last()
            .filter(|attempt| attempt.retried)
            .map_or(EndReason::Cancelled, |attempt| retried_reason(&attempt.end)),
        // Not ended, with a cancel asked for: the stop is what ends it.
        JobState::Placed | JobState::Starting | JobState::Running => EndReason::Cancelled,
    }
}

/// Why the allocation of an attempt that `end` says ended, and that a retry followed, is
/// freed: its host lost, its start timed out, or its start refused (a retry follows nothing
/// else).
fn retried_reason(end: &AttemptRecord) -> EndReason {
    match (end.outcome, end.start_failure) {
        (AttemptOutcome::HostLost, _) => EndReason::HostAbsent,
        (_, Some(StartFailure::StartTimeout)) => EndReason::StartTimeout,
        _ => EndReason::Refused,
    }
}

#[cfg(test)]
#[path = "job_alloc_tests.rs"]
mod tests;
