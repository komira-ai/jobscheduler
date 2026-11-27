//! Why a job store operation did not happen. Every error means nothing was written.

use js_state::StoreError;
use js_types::HostId;

use crate::codec::DecodeError;
use crate::ids::{JobId, MintError};
use crate::record::{AllocationStep, JobState};

/// Why a [`crate::JobStore`] operation did not happen. Whatever the variant, the operation
/// wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobStoreError {
    /// The state store failed; the commit, if one was attempted, did not happen.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// No job has this id.
    #[error("no job {id}")]
    NotFound {
        /// The id asked for.
        id: JobId,
    },
    /// The job has no allocation.
    #[error("job {id} has no allocation")]
    NoAllocation {
        /// The job.
        id: JobId,
    },
    /// A job with this id exists and differs from the one submitted (other than in
    /// priority).
    #[error("job {id} exists and is a different job")]
    JobIdConflict {
        /// The id.
        id: JobId,
    },
    /// Every id minted for a job without one was taken.
    #[error("{tries} minted job ids in a row were taken")]
    IdSpaceExhausted {
        /// How many were minted.
        tries: usize,
    },
    /// The id source failed.
    #[error(transparent)]
    Mint(#[from] MintError),
    /// A new job's callback holds its key's digest, not the key, so its `callback/` row cannot
    /// be armed. Only a resubmit of a recorded job may name its callback by the digest.
    #[error("job {id} names its callback's key by its digest, so no delivery can be armed")]
    CallbackKeyMissing {
        /// The job.
        id: JobId,
    },
    /// The host's node row moved past the version the beat wrote: a newer beat or hello of the
    /// host was applied, so this beat's listing changes nothing more.
    #[error("the node row of host {host} moved past the version the beat wrote")]
    NodeMoved {
        /// The host.
        host: HostId,
    },
    /// The job or its allocation changed between the operation's read and its commit.
    #[error("job {id} changed while the operation ran")]
    Stale {
        /// The job.
        id: JobId,
    },
    /// The host's live allocations leave no room for the job's booking, as the placement's
    /// transaction found them.
    #[error("host {host} has no room for job {id}")]
    HostFull {
        /// The job.
        id: JobId,
        /// The host.
        host: HostId,
    },
    /// The host is cordoned: a live `cordon/<host>` row is there, so no job is placed on it.
    #[error("host {host} is cordoned, so job {id} is not placed on it")]
    HostCordoned {
        /// The job.
        id: JobId,
        /// The host.
        host: HostId,
    },
    /// The host is tainted: its `taint/<host>` row is there (a host job was placed on it), or its
    /// binding holds a taint, so no job is placed on it.
    #[error("host {host} is tainted, so job {id} is not placed on it")]
    HostTainted {
        /// The job.
        id: JobId,
        /// The host.
        host: HostId,
    },
    /// The host is marked to scale down: its `hostmark/<host>` row is there, so no job is placed
    /// on it.
    #[error("host {host} is marked to scale down, so job {id} is not placed on it")]
    HostMarked {
        /// The job.
        id: JobId,
        /// The host.
        host: HostId,
    },
    /// A live allocation on the host holds a host port the job declares.
    #[error("host port {port} of host {host} is held, so job {id} is not placed on it")]
    PortTaken {
        /// The job.
        id: JobId,
        /// The host.
        host: HostId,
        /// The first of the job's host ports that a live allocation on the host holds.
        port: u16,
    },
    /// The pool already holds as many hosts as it may, so the hello of a host new to it is
    /// refused ([`crate::JobStore::hello_node_in_pool`]).
    #[error("pool {pool} is full, so host {host} is not admitted")]
    PoolFull {
        /// The host.
        host: HostId,
        /// The pool.
        pool: String,
    },
    /// The host's row was heard within the grace and names another agent process than the
    /// hello's (or the hello names none), so the hello of a second agent process on a live
    /// host is refused ([`crate::JobStore::hello_node_in_pool`]).
    #[error("host {host} is live, so another agent process is not admitted on it")]
    HostLive {
        /// The host.
        host: HostId,
    },
    /// The host's cordon row changed between the read and the commit of a cordon, a lift or a
    /// restore; the caller reads it again and retries.
    #[error("the cordon of host {host} changed while the operation ran")]
    CordonConflict {
        /// The host.
        host: HostId,
    },
    /// The job is not in the state the caller expects.
    #[error("job {id} is {found:?}, not {expected:?}")]
    UnexpectedState {
        /// The job.
        id: JobId,
        /// The state the caller expects.
        expected: JobState,
        /// The state it is in.
        found: JobState,
    },
    /// The move is not one a job may make.
    #[error("job {id} may not move from {from:?} to {to:?}")]
    IllegalTransition {
        /// The job.
        id: JobId,
        /// Its state.
        from: JobState,
        /// The state asked for.
        to: JobState,
    },
    /// The job's allocation still books its host, so the job cannot be placed again.
    #[error("job {id} still has an allocation that is {step:?}")]
    AllocationHeld {
        /// The job.
        id: JobId,
        /// Where its allocation is.
        step: AllocationStep,
    },
    /// The job's allocation already began to release, or was freed, so a requeue would
    /// overwrite how it ended.
    #[error("job {id} has an allocation that is {step:?}, not booked")]
    AllocationNotBooked {
        /// The job.
        id: JobId,
        /// Where its allocation is.
        step: AllocationStep,
    },
    /// The job does not need the whole host, so it holds no mark.
    #[error("job {id} does not need the whole host")]
    NotWholeHost {
        /// The job.
        id: JobId,
    },
    /// The job neither ended nor was asked to cancel, so its allocation stays booked.
    #[error("job {id} has not ended and no cancel was asked for")]
    NotReleasable {
        /// The job.
        id: JobId,
    },
    /// A row holds a value a later build wrote, which this build cannot read. It is not
    /// treated as absent.
    #[error("the row {key} was written by a later build")]
    UnknownRow {
        /// The row.
        key: String,
    },
    /// A row cannot be read.
    #[error("the row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: DecodeError,
    },
    /// A value is too large for its row.
    #[error("field {tag} of a row is over 4 GiB")]
    TooLarge {
        /// The field.
        tag: u16,
    },
    /// The host's live allocations sum past what a count can hold.
    #[error("the bookings on host {host} overflow")]
    BookingOverflow {
        /// The host.
        host: HostId,
    },
}

impl JobStoreError {
    /// The error for the row `key` that does not decode: [`JobStoreError::UnknownRow`] when
    /// a later build wrote it, [`JobStoreError::Corrupt`] otherwise.
    pub(crate) fn row(key: &str, why: DecodeError) -> JobStoreError {
        match why {
            DecodeError::Newer => JobStoreError::UnknownRow {
                key: key.to_owned(),
            },
            why => JobStoreError::Corrupt {
                key: key.to_owned(),
                why,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::num::NonZeroU32;

    use js_state::StoreError;
    use js_types::HostId;

    use super::JobStoreError;
    use crate::codec::DecodeError;
    use crate::ids::{JobId, MintError};
    use crate::record::{AllocationStep, JobState};

    // Catches an error text that does not say what failed and for which job, row or host,
    // and a wrapped error whose cause is not reachable.
    #[test]
    fn every_error_says_what_failed_and_where() {
        let id = JobId::parse("a").expect("an id");
        let code = NonZeroU32::MIN;
        let host = HostId::new("h").expect("a host id");
        let cases = [
            (
                JobStoreError::Store(StoreError::Failed("down".to_owned())),
                "state store failure: down",
            ),
            (JobStoreError::NotFound { id: id.clone() }, "no job a"),
            (
                JobStoreError::NoAllocation { id: id.clone() },
                "job a has no allocation",
            ),
            (
                JobStoreError::JobIdConflict { id: id.clone() },
                "job a exists and is a different job",
            ),
            (
                JobStoreError::IdSpaceExhausted { tries: 8 },
                "8 minted job ids in a row were taken",
            ),
            (
                JobStoreError::Mint(MintError { code }),
                "the system random source failed (code 1)",
            ),
            (
                JobStoreError::Stale { id: id.clone() },
                "job a changed while the operation ran",
            ),
            (
                JobStoreError::HostCordoned {
                    id: id.clone(),
                    host: host.clone(),
                },
                "host h is cordoned, so job a is not placed on it",
            ),
            (
                JobStoreError::HostTainted {
                    id: id.clone(),
                    host: host.clone(),
                },
                "host h is tainted, so job a is not placed on it",
            ),
            (
                JobStoreError::HostMarked {
                    id: id.clone(),
                    host: host.clone(),
                },
                "host h is marked to scale down, so job a is not placed on it",
            ),
            (
                JobStoreError::PortTaken {
                    id: id.clone(),
                    host: host.clone(),
                    port: 8080,
                },
                "host port 8080 of host h is held, so job a is not placed on it",
            ),
            (
                JobStoreError::PoolFull {
                    host: host.clone(),
                    pool: "burst".to_owned(),
                },
                "pool burst is full, so host h is not admitted",
            ),
            (
                JobStoreError::HostLive { host: host.clone() },
                "host h is live, so another agent process is not admitted on it",
            ),
            (
                JobStoreError::CordonConflict { host: host.clone() },
                "the cordon of host h changed while the operation ran",
            ),
            (
                JobStoreError::UnexpectedState {
                    id: id.clone(),
                    expected: JobState::Queued,
                    found: JobState::Placed,
                },
                "job a is Placed, not Queued",
            ),
            (
                JobStoreError::IllegalTransition {
                    id: id.clone(),
                    from: JobState::Succeeded,
                    to: JobState::Running,
                },
                "job a may not move from Succeeded to Running",
            ),
            (
                JobStoreError::AllocationHeld {
                    id: id.clone(),
                    step: AllocationStep::Booked,
                },
                "job a still has an allocation that is Booked",
            ),
            (
                JobStoreError::AllocationNotBooked {
                    id: id.clone(),
                    step: AllocationStep::Freed,
                },
                "job a has an allocation that is Freed, not booked",
            ),
            (
                JobStoreError::NotWholeHost { id: id.clone() },
                "job a does not need the whole host",
            ),
            (
                JobStoreError::NotReleasable { id: id.clone() },
                "job a has not ended and no cancel was asked for",
            ),
            (
                JobStoreError::UnknownRow {
                    key: "job/a".to_owned(),
                },
                "the row job/a was written by a later build",
            ),
            (
                JobStoreError::Corrupt {
                    key: "job/a".to_owned(),
                    why: DecodeError::Empty,
                },
                "the row job/a cannot be read: the row is empty",
            ),
            (
                JobStoreError::TooLarge { tag: 19 },
                "field 19 of a row is over 4 GiB",
            ),
            (
                JobStoreError::BookingOverflow {
                    host: HostId::new("host-a").expect("a host"),
                },
                "the bookings on host host-a overflow",
            ),
        ];
        for (error, said) in cases {
            assert_eq!(error.to_string(), said);
        }
        let from_store: JobStoreError = StoreError::Failed("down".to_owned()).into();
        assert_eq!(
            from_store,
            JobStoreError::Store(StoreError::Failed("down".to_owned()))
        );
        let from_mint: JobStoreError = MintError { code }.into();
        assert!(
            from_mint.source().is_none(),
            "transparent: the source is the mint's"
        );
        let corrupt = JobStoreError::Corrupt {
            key: "job/a".to_owned(),
            why: DecodeError::Truncated,
        };
        assert!(corrupt.source().is_none());
    }
}
