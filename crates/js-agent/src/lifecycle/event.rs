//! What the lifecycle reports of what it did ([`Event`]), why it did not act on a start
//! ([`Skip`]) and what it keeps for a person ([`Finding`]).

use js_agent_wire::Taint;

/// Something the agent keeps because it needs a person or a later step: a cleanup that
/// failed, a check that found something, a container it could not account for. The text
/// is local; it is never put into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The run it is about, when it is about one.
    pub run_id: Option<String>,
    /// One of [`code`](super::code), or the code of a [`CheckFinding`](super::CheckFinding).
    pub code: &'static str,
    /// What the runtime, the file system or the check said.
    pub message: String,
}

/// Why a start was not acted on, when that is worth a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// It reached the agent after its validity window.
    Stale,
    /// The run was finished, or stopped, earlier.
    Retired,
    /// The agent already holds as many runs as a heartbeat can list, so it took on no
    /// more; the server may send the start again once results are acknowledged.
    Full,
    /// The agent holds a run whose run id is the job id of this job start, or a job whose
    /// job id is the run id of this run start: the two would share one scratch directory
    /// and one set of labels, so the start is not taken.
    Clash,
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
    Tainted(Taint),
    /// The slot is free again.
    SlotFreed {
        /// The run.
        run_id: String,
    },
    /// Something to look at; see [`Finding`]. Reported once for each code of a run.
    Finding(Finding),
}
