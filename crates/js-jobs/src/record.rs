//! The job, allocation and mark records, as the store's callers read and write them.

use std::collections::BTreeMap;

use js_state::StoreTime;
use js_types::{ExecMode, HostId, ImageDigestRef, Labels, Resources};
use sha2::{Digest as _, Sha256};

use crate::codec::Carried;
use crate::ids::{AllocationId, ClientName, JobId};
use crate::result::JobResult;

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
    /// How the job is confined on its host.
    pub isolation: Isolation,
}

/// How a job is confined on its host (design section 3.8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Isolation {
    /// In a container (or the native runtime's sandbox): the default.
    #[default]
    Contained,
    /// On the machine itself: the host is tainted when the job's placement commits.
    Host,
}

/// The reason of the taint a host-isolation job's placement puts on its host.
pub const HOST_JOB_TAINT: &str = "host-job";

/// The taint a host-isolation job's placement puts on its host (`taint/<host>`): the job that
/// placed there. Only [`JobStore::drop_taints`](crate::JobStore::drop_taints) deletes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTaint {
    /// The job whose placement tainted the host.
    pub job: JobId,
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

/// The SHA-256 of a file's contents: what an ended job keeps of each of its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileDigest([u8; 32]);

impl FileDigest {
    /// The digest of `contents`.
    #[must_use]
    pub fn of(contents: &[u8]) -> FileDigest {
        FileDigest(Sha256::digest(contents).into())
    }

    /// The digest as a row stored it: 32 bytes.
    #[must_use]
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> FileDigest {
        FileDigest(bytes)
    }

    /// The 32 bytes of the digest.
    #[must_use]
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The files handed to a job, by name. Their contents are secret (a read token, an upload
/// URL): a job keeps them while it is live, so its host can be sent them, and when it ends
/// they are replaced by their digests in the same write that ends it, so an ended job's
/// record holds the names and no contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFiles {
    /// The contents, as submitted: a job that has not ended.
    Contents(BTreeMap<String, Vec<u8>>),
    /// The SHA-256 of each file's contents: a job that has ended.
    Digests(BTreeMap<String, FileDigest>),
}

impl Default for JobFiles {
    /// No files.
    fn default() -> JobFiles {
        JobFiles::Contents(BTreeMap::new())
    }
}

impl JobFiles {
    /// The files' names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        match self {
            JobFiles::Contents(files) => files.keys().map(String::as_str).collect(),
            JobFiles::Digests(files) => files.keys().map(String::as_str).collect(),
        }
    }

    /// The contents by name; `None` once they were replaced by their digests.
    #[must_use]
    pub fn contents(&self) -> Option<&BTreeMap<String, Vec<u8>>> {
        match self {
            JobFiles::Contents(files) => Some(files),
            JobFiles::Digests(_) => None,
        }
    }

    /// The digest of each file by name, computed from the contents or as stored.
    fn digests(&self) -> BTreeMap<&str, FileDigest> {
        match self {
            JobFiles::Contents(files) => files
                .iter()
                .map(|(name, contents)| (name.as_str(), FileDigest::of(contents)))
                .collect(),
            JobFiles::Digests(files) => files
                .iter()
                .map(|(name, digest)| (name.as_str(), *digest))
                .collect(),
        }
    }

    /// These files with each content replaced by its digest; files already digested are
    /// returned as they are.
    pub(crate) fn digested(self) -> JobFiles {
        match self {
            JobFiles::Contents(files) => JobFiles::Digests(
                files
                    .into_iter()
                    .map(|(name, contents)| (name, FileDigest::of(&contents)))
                    .collect(),
            ),
            digests @ JobFiles::Digests(_) => digests,
        }
    }

    /// Whether these are the same files as `other`: the same names, and for each the same
    /// contents, compared byte for byte when both sides hold contents and by digest when
    /// either holds digests.
    fn same_as(&self, other: &JobFiles) -> bool {
        match (self, other) {
            (JobFiles::Contents(mine), JobFiles::Contents(theirs)) => mine == theirs,
            _ => self.digests() == other.digests(),
        }
    }
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
    /// Files handed to the job, by path (for example the upload URL its client minted): their
    /// contents while the job is live, their digests once it has ended.
    pub files: JobFiles,
    /// The job's labels.
    pub labels: Labels,
}

impl JobSpec {
    /// Whether `self`, a stored job, and `submitted` are the same job (design section 5.3,
    /// step 2): every field equal, the files compared as [`JobFiles`] compares them, so a
    /// resubmit of an ended job, whose files are digests, matches by the digest of each
    /// submitted file.
    pub(crate) fn same_job_as(&self, submitted: &JobSpec) -> bool {
        // Every field named, so a field added later cannot be left out of the comparison.
        let JobSpec {
            image,
            command,
            args,
            env,
            needs,
            constraints,
            wall_limit_secs,
            network,
            ports,
            files,
            labels,
        } = self;
        (*image == submitted.image)
            & (*command == submitted.command)
            & (*args == submitted.args)
            & (*env == submitted.env)
            & (*needs == submitted.needs)
            & (*constraints == submitted.constraints)
            & (*wall_limit_secs == submitted.wall_limit_secs)
            & (*network == submitted.network)
            & (*ports == submitted.ports)
            & files.same_as(&submitted.files)
            & (*labels == submitted.labels)
    }

    /// This spec with its files' contents replaced by their digests: what an ended job keeps.
    pub(crate) fn digested(self) -> JobSpec {
        JobSpec {
            files: self.files.digested(),
            ..self
        }
    }
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

/// Whether a job may move from `from` to `to`: [`crate::JobStore::transition`] makes the
/// moves to a state that has not ended, [`crate::JobStore::end_job`] the moves to an end.
/// Placement (queued to placed) and the requeue after a failed whole-host prepare (placed to
/// queued) have operations of their own and are not moves here.
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
    /// Whether the client asked to cancel it. Kept after the job ends, however it ends; the job
    /// service's `Job.cancel_requested` shows it only until the end, and on a job that ended
    /// lost.
    pub cancel_requested: bool,
    /// When the client first asked to cancel it.
    pub cancel_requested_at: Option<StoreTime>,
    /// What the job left for its client to read, set when it ends: `None` until then, and
    /// for a job that was lost or failed to start.
    pub result: Option<JobResult>,
    /// When the job's record may be deleted: set when it ends, to the end plus the store's
    /// retention window, and raised at boot ([`crate::JobStore::raise_deadlines_at_boot`]);
    /// `None` until it ends.
    pub retention_ends: Option<StoreTime>,
    /// The earliest instant a time limit may end the job while it waits to be placed or to
    /// start: written at boot to at least the boot plus [`crate::BOOT_GRACE`]
    /// ([`crate::JobStore::raise_at_boot`]), so an outage fails no job that waited through it;
    /// `None` on a row no boot raised (and on every row an older build wrote).
    pub deadline_floor: Option<StoreTime>,
    /// The client that submitted the job, as the server named it from the token it presented.
    /// Not part of the idempotency comparison: a retry of the same job by another client
    /// answers the stored record, which keeps the first. `None` on a row written before
    /// clients were named, and for a job recorded with no client.
    pub client: Option<ClientName>,
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
    /// The host ports the job declared, sorted ascending, each once. They are booked with the
    /// allocation and held until it is freed; a row written before this field holds none.
    pub host_ports: Vec<u16>,
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

/// What keeps a whole-host job off a host ([`MarkRecord::exclusions`]), and until when: the
/// kinds of js-sched's `Bar`, with its ends on the store's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// The back-off after a hold the job gave up at its deadline, until the instant given
    /// (exclusive).
    Backoff(StoreTime),
    /// A hard exclusion until the instant given (exclusive).
    Until(StoreTime),
    /// A hard exclusion that never ends.
    ForLife,
}

/// The mark of a queued whole-host job (design section 5.4): the host it holds while that host
/// drains, when the mark was made, its deadline, the end of the host's tail when it was made,
/// and the hosts the job is kept off, each with its end. It is the row `mark/<id>`, one per
/// job, written with [`crate::JobStore::put_mark`]. The failed-prepare count of a job is not
/// here: it belongs to the job's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkRecord {
    /// The job that holds the mark; the row's key is the job's.
    pub job: JobId,
    /// The held host.
    pub host: HostId,
    /// When the mark was made.
    pub made: StoreTime,
    /// When the hold is given up if the host is still busy; raised at boot to at least the
    /// boot plus [`crate::BOOT_GRACE`] ([`crate::JobStore::raise_at_boot`]).
    pub deadline: StoreTime,
    /// The end of the host's tail when the mark was made: when the job is expected to start
    /// there. `None`: not known.
    pub tail_end: Option<StoreTime>,
    /// The hosts the job may not mark, each with what keeps it off and until when.
    pub exclusions: BTreeMap<HostId, Exclusion>,
    /// Fields a later build wrote, kept for the next write of the row.
    pub(crate) carried: Carried,
}

impl MarkRecord {
    /// The mark of `job` on `host`, made at `made`, given up at `deadline`, with no tail end
    /// known and no exclusions.
    #[must_use]
    pub fn new(job: JobId, host: HostId, made: StoreTime, deadline: StoreTime) -> MarkRecord {
        MarkRecord {
            job,
            host,
            made,
            deadline,
            tail_end: None,
            exclusions: BTreeMap::new(),
            carried: Carried::default(),
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

    // Catches files digested twice (a digest replaced by the digest of its own bytes) and a
    // digest that is not the SHA-256 of the contents: digesting is done once and stays done.
    #[test]
    fn files_are_digested_once() {
        let contents = JobFiles::Contents(BTreeMap::from([("a".to_owned(), b"abc".to_vec())]));
        let digested = contents.clone().digested();
        // SHA-256 of "abc", from FIPS 180-2, appendix B.1.
        let abc = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(
            digested,
            JobFiles::Digests(BTreeMap::from([(
                "a".to_owned(),
                FileDigest::from_bytes(abc)
            )]))
        );
        assert_eq!(digested.clone().digested(), digested);
        assert_eq!(contents.contents().map(BTreeMap::len), Some(1));
        assert_eq!(digested.contents(), None);
        assert_eq!(digested.names(), ["a"]);
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
