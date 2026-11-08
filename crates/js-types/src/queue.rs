//! Why a queued job waits, in the words the engine decides in and the job store records
//! (`docs/design/queue-status.md`): a [`QueueCode`], the [`JobEventKind`] of each event a job's
//! history keeps, and the [`QueueNote`] one outcome of a pass makes for a job.
//!
//! The engine decides a note (`js_sched::queue_status`) and the job store applies it to the
//! job's record with its own clock (`js_jobs::QueueTrail`). Neither enum is open: a reader that
//! meets a value it does not know reads it as the `Unspecified` value.

use crate::HostId;

/// Why a job is where it is in the queue. The values and their numbers are those of the job
/// service's `QueueStatusCode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueueCode {
    /// Nothing recorded yet.
    #[default]
    Unspecified,
    /// No open host has room for the job now.
    WaitingForCapacity,
    /// The job books a whole host and none is free.
    WaitingForWholeHost,
    /// An acquire for the job was refused for quota; its instance type is passed over in
    /// that region for a while.
    QuotaBackoff,
    /// An acquire for the job was refused for capacity; the next zone, offer or provider is
    /// asked.
    FailingOver,
    /// The job was placed on a host.
    Placed,
    /// A machine was launched for the job, and the job waits for it to join.
    WaitingForMachine,
}

/// What one event of a job's history says happened. The values and their numbers are those of
/// the job service's `JobEventKind`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobEventKind {
    /// Not a kind: what a value this version does not know reads as.
    #[default]
    Unspecified,
    /// The job began to wait for room.
    WaitingForCapacity,
    /// The job began to wait for a whole host.
    WaitingForWholeHost,
    /// An acquire for the job was refused for quota.
    QuotaBackoff,
    /// An acquire for the job was refused for capacity.
    FailingOver,
    /// The job was placed on a host.
    Placed,
    /// A failed whole-host prepare sent the placed job back to the queue.
    Requeued,
    /// A machine was launched for the job.
    WaitingForMachine,
}

/// What one outcome of a pass records on a job: the code it now shows and the detail for a
/// person, the event to append to its history if any, the host that event names, and whether
/// the outcome was a placement attempt. The store stamps it with its own clock when it applies
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueNote {
    /// The code the job shows from now on.
    pub code: QueueCode,
    /// Text for a person saying more. A program acts on the code only.
    pub detail: String,
    /// The event to append, or `None` for a change that appends none (a detail alone).
    pub event: Option<JobEventKind>,
    /// The host the event names: the one a job was placed on, or sent back from.
    pub host: Option<HostId>,
    /// Whether the outcome was a placement attempt, counted in the job's attempts.
    pub attempt: bool,
}
