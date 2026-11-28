//! What the lifecycle reports of what it did ([`Event`]), why it did not act on a start
//! ([`Skip`]) and what it keeps for a person ([`Finding`]).

use js_agent_wire::{JobUsage, StartSizing, Taint};

/// Something the agent keeps because it needs a person or a later step: a cleanup that
/// failed, a check that found something, a container it could not account for. The text
/// is local; it is never put into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The run it is about, when it is about one.
    pub run_id: Option<String>,
    /// One of [`code`](super::code), or the code of a [`CheckFinding`](super::CheckFinding) or
    /// of a [`CheckNote`](super::CheckNote).
    pub code: &'static str,
    /// What the runtime, the file system or the check said.
    pub message: String,
}

/// Why a start was not acted on, when that is worth a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// It reached the agent after its validity window.
    Stale,
    /// The run was finished, or stopped, earlier: a run start whose run id is retired.
    Retired,
    /// The fence of this job start (its job id with its allocation id) is retired: the job
    /// ended, was stopped or cancelled, was not started for a silence of the server, or a stop
    /// that named no allocation retired every fence of the job. Nothing is made or booked.
    /// The same job under another allocation is another fence.
    FenceRetired,
    /// The agent already holds as many runs as a heartbeat can list, so it took on no
    /// more; the server may send the start again once results are acknowledged.
    Full,
    /// The agent holds a run whose run id is the job id of this job start, or a job whose
    /// job id is the run id of this run start: the two would share one scratch directory
    /// and one set of labels, so the start is not taken.
    Clash,
}

/// What a job booked and where the booking came from, carried by [`Event::Started`] and
/// [`Event::ResultReady`] and so written in the journal at a job's start and at its result,
/// with the job's usage at its result: the record that tells how a job's booking compared with
/// what it used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobBooking {
    /// The CPU booked, in thousandths of a core: the lease's `cpu.weight`, a share of the
    /// node's cores and never a ceiling.
    pub cpu_millis: u64,
    /// The memory booked, in bytes: what placement counted for the job, not a limit (the lease
    /// has no `memory.max`, and the job may use swap).
    pub memory_bytes: u64,
    /// The size class and which history gave the CPU and the memory, as the job's start said
    /// ([`JobStartSpec::sizing`](js_agent_wire::JobStartSpec::sizing)); `None` for a run start,
    /// for a server that sends none, and for a job taken over from an earlier agent.
    pub sizing: Option<StartSizing>,
}

/// When each step of a container's start was done, carried by [`Event::Started`] and so
/// logged with it: where the time from a start's arrival to its container went. Its
/// container's command is started by the container's init after the runtime's create has
/// returned (`crun run` makes the container then), so the last step here comes before it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartTimeline {
    /// When the reply that carried the start arrived, on the lifecycle's clock (milliseconds
    /// from its origin, which on a node is the agent's start).
    pub received_ms: u64,
    /// The server's Unix time in milliseconds at which it placed the job, as its start says
    /// ([`JobStartSpec::placed_at_ms`](js_agent_wire::JobStartSpec::placed_at_ms)); `None`
    /// for a run start and for a server that sends none.
    pub placed_at_ms: Option<u64>,
    /// When the reply arrived in Unix time (milliseconds), taken from the clock's Unix time
    /// at the start's admission less the time since `received_ms`; `None` when the clock has
    /// no Unix time. From `placed_at_ms` to here spans the server's clock and the node's, so
    /// the skew between them is in it; from here to the runtime's `running` step is on the
    /// node's clock alone.
    pub received_at_ms: Option<u64>,
    /// The lifecycle's steps, each with the milliseconds after `received_ms` at which it was
    /// done, in the order they were: `taken` (the worker began on the start), `admitted` (its
    /// checks passed and its slot is booked). A start without layers or mounts goes on with
    /// `ready` (all it waited for is here) and `dirs` (its scratch directory, and the
    /// runtime's beside it, made and put under its quota). A start with layers or mounts
    /// makes its directories at its admission and goes on with `dirs`, `provided` (a start
    /// with providers: every provider has answered the directory of its source), `resolved`
    /// (its images are here and its host paths found) and `ready`. Each ends with `created`
    /// (the runtime's create returned).
    pub steps: Vec<(&'static str, u64)>,
    /// The runtime's own steps inside the create, each with the wall clock time it was done
    /// at (milliseconds since the Unix epoch), in order; empty for a runtime that keeps none
    /// ([`Runtime::start_steps`](super::Runtime::start_steps)).
    pub runtime: Vec<(&'static str, u64)>,
    /// The lookup of a host path that the start waited on (the open of the path beneath its
    /// root, which on a FUSE file system is the file system's own lookup of the tree), as the
    /// runtime gives it ([`Runtime::lookup_stamps`](super::Runtime::lookup_stamps)); `None`
    /// for a start with no host path and for a runtime that keeps none.
    pub lookup: Option<LookupStamps>,
}

/// When the lookup of one host path was asked of the runtime's pool of lookups, when a
/// thread of the pool began it and when it ended, each the wall clock time in milliseconds
/// since the Unix epoch, as one host's clock read them. A start with several host paths is
/// given the lookup that ended last, the one it waited on.
///
/// It is logged with the two spans it gives beside the stamps, the time it waited in the
/// pool's queue ([`LookupStamps::queue_ms`]) and the time the open took
/// ([`LookupStamps::open_ms`]):
/// `LookupStamps { queued_at_ms: Q, started_at_ms: S, ended_at_ms: E, queue_ms: S-Q,
/// open_ms: E-S }`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LookupStamps {
    /// When the lookup was asked of the pool.
    pub queued_at_ms: u64,
    /// When a thread of the pool began it.
    pub started_at_ms: u64,
    /// When the open returned.
    pub ended_at_ms: u64,
}

impl LookupStamps {
    /// The milliseconds the lookup waited in the pool's queue for a thread: from when it was
    /// asked to when a thread began it, none for a clock that stepped back.
    #[must_use]
    pub fn queue_ms(&self) -> u64 {
        self.started_at_ms.saturating_sub(self.queued_at_ms)
    }

    /// The milliseconds the open took on its thread: from when the thread began it to when
    /// it returned, none for a clock that stepped back.
    #[must_use]
    pub fn open_ms(&self) -> u64 {
        self.ended_at_ms.saturating_sub(self.started_at_ms)
    }
}

impl std::fmt::Debug for LookupStamps {
    /// The stamps, then the two spans they give.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookupStamps")
            .field("queued_at_ms", &self.queued_at_ms)
            .field("started_at_ms", &self.started_at_ms)
            .field("ended_at_ms", &self.ended_at_ms)
            .field("queue_ms", &self.queue_ms())
            .field("open_ms", &self.open_ms())
            .finish()
    }
}

/// When each step of a finished run's cleanup was done, carried by [`Event::Cleaned`] and so
/// logged with it: where the time from a run's end to its free slot went, and how much of it
/// the lifecycle's worker spent itself. The container's removal and the removal of the run's
/// directories run off the worker when the runtime and the lifecycle can (on their pools),
/// and their time is then in the steps and not in `worker_ms`; so is the checks' wait for
/// the host to settle, which the job waits out in its cleanup while the worker goes on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupTimeline {
    /// The lifecycle's steps, each with the milliseconds, on the lifecycle's clock, after
    /// the first step that took the run into its cleanup, at which the step that did it
    /// ended, in the order they were: `removed` (the container is gone), `scratch` (the
    /// scratch directory and the directories beside it are gone) and `checks` (the post-job
    /// checks passed and the slot is free).
    pub steps: Vec<(&'static str, u64)>,
    /// The milliseconds the worker spent in the run's cleanup, on the lifecycle's clock,
    /// summed over the steps that took it on: the record of the owed checks, what of the
    /// removals runs on the worker, the quota and the checks' looks at the host.
    pub worker_ms: u64,
}

/// What [`Lifecycle`](super::Lifecycle) did, in order. The caller logs these; a test reads them as the
/// record of the order of things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A labelled container was taken over after a restart.
    Adopted {
        /// The run.
        run_id: String,
    },
    /// The container started.
    Started {
        /// The run.
        run_id: String,
        /// When each step of the start was done.
        timeline: StartTimeline,
        /// What the job booked and where the booking came from.
        booked: JobBooking,
    },
    /// The start was refused and nothing was created; a `failed_to_start` result is held.
    Refused {
        /// The run.
        run_id: String,
        /// The wire refusal code.
        code: &'static str,
        /// A local explanation.
        why: String,
    },
    /// The start was not acted on and nothing is sent.
    Skipped {
        /// The run.
        run_id: String,
        /// Why.
        why: Skip,
    },
    /// A command of the reply failed validation and was not acted on.
    Invalid {
        /// Its place in the reply.
        index: usize,
        /// The wire refusal code.
        code: &'static str,
    },
    /// The container exited and its result is held for the next heartbeat.
    ResultReady {
        /// The run.
        run_id: String,
        /// What the job booked and where the booking came from.
        booked: JobBooking,
        /// What the job's control group used, with how often its CPU was throttled
        /// ([`JobUsage::cpu_throttle`]), when it was read; `None` for a job that never ran or
        /// whose group could not be read.
        usage: Option<JobUsage>,
    },
    /// The container is gone.
    ContainerRemoved {
        /// The run.
        run_id: String,
    },
    /// The scratch directory is gone.
    ScratchRemoved {
        /// The run.
        run_id: String,
    },
    /// The post-job checks passed.
    ChecksPassed {
        /// The run.
        run_id: String,
    },
    /// The host is tainted from now on, and this is why (the first taint stays). It is
    /// pushed once the marker has been tried and before the findings that follow from the
    /// same cause; a failure to write the marker is a [`Finding`] after it.
    Tainted {
        /// The taint, as the marker holds it and the heartbeat reports it.
        taint: Taint,
        /// What changed, in one line, when a check's finding said so (its detail, with the
        /// words "the check could not run" for a check that did not run); none for a taint
        /// that no check found (a host job, a container that cannot be removed); the
        /// `cgroup-not-found` taint carries its message.
        detail: Option<String>,
    },
    /// The slot is free again.
    SlotFreed {
        /// The run.
        run_id: String,
    },
    /// The run's cleanup passed: when each of its steps was done. Pushed after
    /// [`Event::SlotFreed`].
    Cleaned {
        /// The run.
        run_id: String,
        /// When each step of the cleanup was done.
        timeline: CleanupTimeline,
    },
    /// Something to look at; see [`Finding`]. Reported once for each code of a run; a check's
    /// notes are one each at every call of the check that saw a change, and taint nothing.
    Finding(Finding),
}
