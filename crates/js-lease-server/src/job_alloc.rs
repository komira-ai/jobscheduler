//! The one way the server frees an allocation (design section 6.2).
//!
//! An allocation is freed only when the host confirms the job's container is gone
//! ([`Freed::HostConfirmed`]) or the provider reads the host absent
//! ([`Freed::ProviderAbsent`]). A host that is only silent frees nothing: unknown is not
//! absent. Every path that frees calls [`free_allocation`], which records why in the
//! allocation's row from the job's end, so there is no second way to free.

use js_jobs::{
    AllocationRecord, EndReason, IdSource, JobId, JobRecord, JobState, JobStore, JobStoreError,
    ResultReason, StartFailure,
};
use js_state::Store;

/// What says an allocation's container is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freed {
    /// The host listed the job's container gone: its result was acknowledged and its
    /// cleanup checks ran clean, so the agent dropped it from its listing.
    HostConfirmed,
    /// The provider read the host absent.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the host-absence tick frees with it; until that lands only tests name it"
        )
    )]
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
            Some(job) if !job.state.is_ended() && !job.cancel_requested => {
                return Err(JobStoreError::NotReleasable { id: id.clone() });
            }
            Some(job) => reason_of(job),
            // A job's row is deleted at its retention end while its allocation stays as
            // history; such a job ended long ago, and how is in no row any longer.
            None => EndReason::Exited,
        },
    };
    store.free(id, reason)
}

/// Why the allocation of `job`, ended or asked to cancel, is freed once its host confirms the
/// container gone.
fn reason_of(job: &JobRecord) -> EndReason {
    match job.state {
        JobState::Cancelled => EndReason::Cancelled,
        JobState::Lost => EndReason::HostAbsent,
        JobState::FailedToStart(StartFailure::StartTimeout) => EndReason::StartTimeout,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady) => EndReason::FailedPrepare,
        JobState::FailedToStart(StartFailure::AgentRefused | StartFailure::NoCapacity) => {
            EndReason::Refused
        }
        JobState::Succeeded | JobState::Failed => {
            match job.result.as_ref().map(|result| result.reason) {
                Some(ResultReason::WallLimit) => EndReason::WallLimit,
                Some(ResultReason::Memory) => EndReason::Memory,
                Some(ResultReason::Cancelled) => EndReason::Cancelled,
                Some(ResultReason::Exit) | None => EndReason::Exited,
            }
        }
        // Not ended, with a cancel asked for: the stop is what ends it.
        JobState::Queued | JobState::Placed | JobState::Starting | JobState::Running => {
            EndReason::Cancelled
        }
    }
}
