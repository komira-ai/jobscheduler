//! What the lifecycle asks of the node-local mount providers it was given
//! (`--mount-provider NAME=SOCKET`, `docs/design/mount-providers.md`): the seam between the
//! lifecycle, which decides when to ask and what an answer means, and the client that
//! carries a call to a provider's socket and back (`crate::provider::ProviderClient`).
//!
//! The lifecycle runs on its worker thread and never waits on a provider, so [`Providers`]
//! is polled, as an image pull is: [`Providers::ask`] sends a call the first time it is
//! asked and answers [`ProviderAnswer::Pending`] until the provider has answered, then
//! hands the answer over once. Asked again after that, it sends the call again. Every call
//! of the contract is idempotent, so asking again is always safe; the lifecycle counts its
//! own bounds (the provider wait of a start, the release retries) on its own clock.

use js_agent_wire::MountMode;

/// One call of the provider contract, as the lifecycle names it. The client fills in what
/// every call carries besides (the agent's id, and `js.` before a run id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCall {
    /// Begin making the tree of a source of a run, with the ref the job named.
    Prepare {
        /// The run (the job's id).
        run_id: String,
        /// The source's place among the run's layers, then its mounts, from 0.
        index: u32,
        /// What the job named, untouched.
        reference: String,
        /// How the job will use the tree: read-only for a layer, the mount's mode for a
        /// mount.
        mode: MountMode,
    },
    /// The directory of a prepared source.
    Mount {
        /// The run.
        run_id: String,
        /// The source's place, as in [`ProviderCall::Prepare`].
        index: u32,
    },
    /// Give up a source.
    Release {
        /// The run.
        run_id: String,
        /// The source's place, as in [`ProviderCall::Prepare`].
        index: u32,
    },
    /// Ask whether a source was lost while its run used it: the provider answers the cause
    /// (`tree-lost` when the tree cannot be read whole), or nothing when it was not lost.
    Check {
        /// The run.
        run_id: String,
        /// The source's place, as in [`ProviderCall::Prepare`].
        index: u32,
    },
    /// Release every source of this agent except those of the runs in `keep`.
    Sweep {
        /// The runs (job ids) whose sources are kept.
        keep: Vec<String>,
    },
}

impl ProviderCall {
    /// What tells two calls apart for a provider: the kind, the run and the index. A call
    /// asked while one with the same key is out is not sent twice.
    #[must_use]
    pub fn key(&self) -> (u8, &str, u32) {
        match self {
            Self::Prepare { run_id, index, .. } => (0, run_id.as_str(), *index),
            Self::Mount { run_id, index } => (1, run_id.as_str(), *index),
            Self::Release { run_id, index } => (2, run_id.as_str(), *index),
            Self::Sweep { .. } => (3, "", 0),
            Self::Check { run_id, index } => (4, run_id.as_str(), *index),
        }
    }
}

/// What a provider answered a call, as far as the lifecycle knows now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAnswer {
    /// The call is out and not answered yet.
    Pending,
    /// The call succeeded; a [`ProviderCall::Mount`] carries the directory the provider
    /// answered and a [`ProviderCall::Check`] the cause it lost the source for, when it did
    /// (both its text, untrusted), every other call `None`.
    Done(Option<String>),
    /// The provider refused the ref for good (`INVALID_ARGUMENT` or
    /// `FAILED_PRECONDITION`), with its reason.
    Refused(String),
    /// The provider could not be reached, did not answer within the call's bound, or
    /// answered that it cannot yet (`UNAVAILABLE` and every other failure), with why. The
    /// call may be asked again.
    Unavailable(String),
}

/// The node-local mount providers of this agent.
pub trait Providers {
    /// Whether this agent was given the provider `name`.
    fn has(&self, name: &str) -> bool;

    /// The names of the providers this agent was given, in order.
    fn names(&self) -> Vec<String>;

    /// Sends `call` to the provider `provider` unless a call with the same
    /// [`ProviderCall::key`] is already out to it, and answers what is known now. A call
    /// whose answer is handed over is forgotten: asking it again sends it again. A provider
    /// this agent was not given answers [`ProviderAnswer::Unavailable`].
    fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer;

    /// Gives the providers the [`Wake`](super::Wake) to ring when a call succeeds, so the
    /// start that waits on it goes on at once. A call refused or failed rings nothing: it is
    /// asked again at a later step (a release and a start's `Prepare` and `Mount` at every
    /// step), and a ring for it would ask a provider that fails at once in a loop with no
    /// pause. The default keeps none: answers are then asked for at the next step.
    fn set_wake(&mut self, _wake: super::Wake) {}
}
