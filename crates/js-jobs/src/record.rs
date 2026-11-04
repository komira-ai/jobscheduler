//! The job and allocation records, as the store's callers read and write them.

use std::collections::BTreeMap;

use js_state::StoreTime;
use js_types::{ExecMode, HostId, ImageDigestRef, Labels, Resources};

use crate::codec::Carried;
use crate::ids::{AllocationId, JobId};

/// What a job needs of the host it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Needs {
    /// CPU, memory, accelerators and disk, as a share of a host.
    Shape(Resources),
    /// The whole host (design section 5.4).
    WholeHost,
}

/// The host a job may run on, beyond its resources: the vocabulary of design section 3.8.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Constraints {
    /// The operating system (`linux`, `macos`). `None`: any.
    pub os: Option<String>,
    /// The CPU architecture (`x86_64`, `aarch64`). `None`: any.
    pub arch: Option<String>,
    /// How the host runs jobs. `None`: either.
    pub exec: Option<ExecMode>,
    /// Further capabilities the host must report, by key and value.
    pub capabilities: BTreeMap<String, String>,
}

/// What a job may reach on the network (design section 5.8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No route out: loopback only.
    #[default]
    None,
    /// The endpoints and resolvers the agent is configured with, and nothing else.
    AllowList,
    /// Any address except what the agent always denies.
    Open,
}

/// A host port forwarded to a port of the job's container (design section 5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeclaredPort {
    /// The port on the host.
    pub host: u16,
    /// The port in the container.
    pub container: u16,
}

/// A job as it was submitted: everything the idempotency check compares (design section
/// 5.3, step 2). The priority is not here, because a resubmit may change it and still be
/// the same job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// The image, pinned by digest.
    pub image: ImageDigestRef,
    /// The command.
    pub command: Vec<String>,
    /// The command's arguments.
    pub args: Vec<String>,
    /// The environment, by name.
    pub env: BTreeMap<String, String>,
    /// The resources, or the whole host.
    pub needs: Needs,
    /// What the host must be.
    pub constraints: Constraints,
    /// The longest the job may run once running, in seconds.
    pub wall_limit_secs: u64,
    /// What the job may reach.
    pub network: NetworkPolicy,
    /// The host ports the job declares.
    pub ports: Vec<DeclaredPort>,
    /// Files handed to the job, by path (for example the upload URL its client minted).
    pub files: BTreeMap<String, Vec<u8>>,
    /// The job's labels.
    pub labels: Labels,
}

/// Why a job did not get to run (design section 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailure {
    /// No capacity came within the pool's maximum queue wait.
    NoCapacity,
    /// The start did not complete within the start timeout.
    StartTimeout,
    /// A whole-host prepare failed as often as the pool allows.
    ExclusiveNotReady,
    /// The agent refused the job (a tainted host, a failed image fetch).
    AgentRefused,
}

/// Where a job is (design section 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Recorded and waiting for placement; it books nothing.
    Queued,
    /// A host was chosen and the allocation made.
    Placed,
    /// The agent has the start and is making the container.
    Starting,
    /// The container runs.
    Running,
    /// It exited 0.
    Succeeded,
    /// It exited nonzero, or was stopped at its wall limit or for memory.
    Failed,
    /// The client cancelled it.
    Cancelled,
    /// Its container is gone and no result exists.
    Lost,
    /// It did not get to run.
    FailedToStart(StartFailure),
}

impl JobState {
    /// Whether the job has ended: no state follows this one.
    #[must_use]
    pub fn is_ended(self) -> bool {
        matches!(
            self,
            JobState::Succeeded
                | JobState::Failed
                | JobState::Cancelled
                | JobState::Lost
                | JobState::FailedToStart(_)
        )
    }
}

/// Whether [`crate::JobStore::transition`] may move a job from `from` to `to`. Placement
/// (queued to placed) and the requeue after a failed whole-host prepare (placed to queued)
/// have operations of their own and are not moves here.
pub(crate) fn may_move(from: JobState, to: JobState) -> bool {
    use JobState::{Cancelled, FailedToStart, Lost, Placed, Queued, Running, Starting};
    use StartFailure::{AgentRefused, ExclusiveNotReady, NoCapacity, StartTimeout};
    match (from, to) {
        (Placed, Starting) | (Starting, Running) => true,
        (Running, JobState::Succeeded | JobState::Failed) => true,
        (Queued, FailedToStart(NoCapacity)) => true,
        (Placed | Starting, FailedToStart(StartTimeout | AgentRefused)) => true,
        (Placed, FailedToStart(ExclusiveNotReady)) => true,
        (from, Cancelled | Lost) => !from.is_ended(),
        _ => false,
    }
}

/// A job's row (design section 10.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// The job's id.
    pub id: JobId,
    /// The job as submitted.
    pub spec: JobSpec,
    /// The placement priority, higher first. Not part of the idempotency comparison.
    pub priority: i32,
    /// Where the job is.
    pub state: JobState,
    /// When it was recorded, by the store's clock.
    pub submitted_at: StoreTime,
    /// When it was last placed.
    pub placed_at: Option<StoreTime>,
    /// When it began running.
    pub started_at: Option<StoreTime>,
    /// When it ended.
    pub ended_at: Option<StoreTime>,
    /// The host it is placed on, once placed.
    pub host: Option<HostId>,
    /// Whether the client asked to cancel it.
    pub cancel_requested: bool,
    /// When the client first asked to cancel it.
    pub cancel_requested_at: Option<StoreTime>,
    /// When the job's record may be deleted. Nothing in this crate sets it.
    pub retention_ends: Option<StoreTime>,
    /// Fields a later build wrote, kept for the next write of the row.
    pub(crate) carried: Carried,
}

/// What an allocation books on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Booked {
    /// A share of the host.
    Shape {
        /// Thousandths of a CPU core.
        cpu_millis: u64,
        /// Bytes of memory.
        memory_bytes: u64,
        /// Bytes of scratch disk.
        disk_bytes: u64,
        /// Whole accelerator devices.
        accelerators: u64,
    },
    /// The whole host.
    WholeHost,
}

/// Where an allocation is (design section 6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationStep {
    /// The host holds, or is about to hold, the job.
    Booked,
    /// The job ended or a cancel was sent, and the host has not confirmed the container gone.
    Releasing,
    /// The container is gone (or was never made). The row is kept as history and books
    /// nothing.
    Freed,
}

/// Why an allocation was freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The job's process exited.
    Exited,
    /// The job was stopped at its wall limit.
    WallLimit,
    /// The job was stopped for memory.
    Memory,
    /// The job was cancelled.
    Cancelled,
    /// The provider said the host is absent.
    HostAbsent,
    /// A whole-host prepare failed before any container was made.
    FailedPrepare,
    /// The start did not complete within the start timeout.
    StartTimeout,
    /// The agent refused the start.
    Refused,
}

/// An allocation's row: the booking of one placement of one job (design sections 3.2, 6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationRecord {
    /// The job it books for; the row's key is the job's.
    pub job: JobId,
    /// This placement's id.
    pub allocation_id: AllocationId,
    /// The host.
    pub host: HostId,
    /// What it books.
    pub booked: Booked,
    /// Where it is.
    pub step: AllocationStep,
    /// When it was made, by the store's clock.
    pub created_at: StoreTime,
    /// When it was freed.
    pub freed_at: Option<StoreTime>,
    /// Why it was freed.
    pub end_reason: Option<EndReason>,
    /// Fields a later build wrote, kept for the next write of the row.
    pub(crate) carried: Carried,
}

/// What a host's live allocations (booked and releasing) book, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostBooking {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Bytes of scratch disk.
    pub disk_bytes: u64,
    /// Whole accelerator devices.
    pub accelerators: u64,
    /// Whether a live allocation books the whole host.
    pub whole_host: bool,
    /// How many live allocations there are.
    pub allocations: u64,
}

impl HostBooking {
    /// This sum with `booked` added, or `None` when a count overflows.
    pub(crate) fn plus(self, booked: &Booked) -> Option<HostBooking> {
        let allocations = self.allocations.checked_add(1)?;
        match *booked {
            Booked::WholeHost => Some(HostBooking {
                whole_host: true,
                allocations,
                ..self
            }),
            Booked::Shape {
                cpu_millis,
                memory_bytes,
                disk_bytes,
                accelerators,
            } => Some(HostBooking {
                cpu_millis: self.cpu_millis.checked_add(cpu_millis)?,
                memory_bytes: self.memory_bytes.checked_add(memory_bytes)?,
                disk_bytes: self.disk_bytes.checked_add(disk_bytes)?,
                accelerators: self.accelerators.checked_add(accelerators)?,
                allocations,
                ..self
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [JobState; 12] = [
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Lost,
        JobState::FailedToStart(StartFailure::NoCapacity),
        JobState::FailedToStart(StartFailure::StartTimeout),
        JobState::FailedToStart(StartFailure::ExclusiveNotReady),
        JobState::FailedToStart(StartFailure::AgentRefused),
    ];

    // Catches any move the design does not name (out of an ended state, back to queued or
    // placed, a start failure from the wrong state) and any move it names that is refused:
    // the whole table of pairs is compared with the list of legal moves.
    #[test]
    fn the_legal_moves_are_exactly_the_designs() {
        use JobState::*;
        use StartFailure::*;
        let live = [Queued, Placed, Starting, Running];
        let mut legal = vec![
            (Placed, Starting),
            (Starting, Running),
            (Running, Succeeded),
            (Running, Failed),
            (Queued, FailedToStart(NoCapacity)),
            (Placed, FailedToStart(StartTimeout)),
            (Starting, FailedToStart(StartTimeout)),
            (Placed, FailedToStart(AgentRefused)),
            (Starting, FailedToStart(AgentRefused)),
            (Placed, FailedToStart(ExclusiveNotReady)),
        ];
        for from in live {
            legal.push((from, Cancelled));
            legal.push((from, Lost));
        }
        for from in ALL_STATES {
            for to in ALL_STATES {
                assert_eq!(
                    may_move(from, to),
                    legal.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    // Catches an ended state read as live (it could then move again) or a live one read as
    // ended (its allocation would be released while it runs).
    #[test]
    fn only_the_end_states_are_ended() {
        let ended: Vec<JobState> = ALL_STATES.into_iter().filter(|s| s.is_ended()).collect();
        assert_eq!(ended, ALL_STATES[4..].to_vec());
    }

    // Catches a sum that drops a dimension, counts a whole host as a share, or wraps on
    // overflow instead of saying so.
    #[test]
    fn a_booking_sums_each_dimension_and_refuses_to_overflow() {
        let shape = Booked::Shape {
            cpu_millis: 1,
            memory_bytes: 2,
            disk_bytes: 3,
            accelerators: 4,
        };
        let sum = HostBooking::default()
            .plus(&shape)
            .and_then(|sum| sum.plus(&shape))
            .and_then(|sum| sum.plus(&Booked::WholeHost));
        assert_eq!(
            sum,
            Some(HostBooking {
                cpu_millis: 2,
                memory_bytes: 4,
                disk_bytes: 6,
                accelerators: 8,
                whole_host: true,
                allocations: 3,
            })
        );
        // Each dimension on its own: the largest sum is answered, one more is refused, with
        // every other dimension small.
        let one = |dimension: usize, count: u64| {
            let mut counts = [1u64; 4];
            counts[dimension] = count;
            let [cpu_millis, memory_bytes, disk_bytes, accelerators] = counts;
            Booked::Shape {
                cpu_millis,
                memory_bytes,
                disk_bytes,
                accelerators,
            }
        };
        for dimension in 0..4 {
            let start = HostBooking::default()
                .plus(&one(dimension, u64::MAX - 1))
                .expect("one booking fits");
            let largest = start
                .plus(&one(dimension, 1))
                .expect("the largest sum fits");
            let counts = [
                largest.cpu_millis,
                largest.memory_bytes,
                largest.disk_bytes,
                largest.accelerators,
            ];
            let mut want = [2u64; 4];
            want[dimension] = u64::MAX;
            assert_eq!(counts, want, "dimension {dimension}");
            assert_eq!(
                largest.plus(&one(dimension, 1)),
                None,
                "dimension {dimension}"
            );
        }
        let counted_out = HostBooking {
            allocations: u64::MAX,
            ..HostBooking::default()
        };
        assert_eq!(counted_out.plus(&Booked::WholeHost), None);
    }
}
