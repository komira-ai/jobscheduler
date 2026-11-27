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
//! own bounds (the provider wait of a start, the release retries, and the wait after a
//! failed call before it is sent again) on its own clock.

use js_agent_wire::MountMode;

/// One call of the provider contract, as the lifecycle names it. The client fills in what
/// every call carries besides (the agent's id, and `js.` before a run id).
///
/// A source is named by its run, the run's attempt and its index. The attempt tells two
/// starts of one run on this agent apart (the same job given to it again): each has its
/// own, larger than every earlier one, kept in every call of that start, so a provider
/// that reads it never lets a late `Release` of an earlier start touch a later one's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCall {
    /// Begin making the tree of a source of a run, with the ref the job named.
    Prepare {
        /// The run (the job's id).
        run_id: String,
        /// The run's attempt on this agent: 1 or more, the same in every call of one start;
        /// 0 for a run whose record an agent wrote before it counted attempts, which a
        /// provider takes for the source as it was named then.
        attempt: u64,
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
        /// The run's attempt, as in [`ProviderCall::Prepare`].
        attempt: u64,
        /// The source's place, as in [`ProviderCall::Prepare`].
        index: u32,
    },
    /// Give up a source.
    Release {
        /// The run.
        run_id: String,
        /// The run's attempt, as in [`ProviderCall::Prepare`].
        attempt: u64,
        /// The source's place, as in [`ProviderCall::Prepare`].
        index: u32,
    },
    /// Release every source of this agent except those of the runs in `keep`.
    Sweep {
        /// The runs (job ids) whose sources are kept, each with the earliest attempt kept:
        /// a source of an earlier attempt of the run is released. 0 keeps every attempt of
        /// the run (one whose attempt is not known).
        keep: Vec<(String, u64)>,
    },
}

impl ProviderCall {
    /// What tells two calls apart for a provider: the kind, the run, the attempt and the
    /// index. A call asked while one with the same key is out is not sent twice.
    #[must_use]
    pub fn key(&self) -> (u8, &str, u64, u32) {
        match self {
            Self::Prepare {
                run_id,
                attempt,
                index,
                ..
            } => (0, run_id.as_str(), *attempt, *index),
            Self::Mount {
                run_id,
                attempt,
                index,
            } => (1, run_id.as_str(), *attempt, *index),
            Self::Release {
                run_id,
                attempt,
                index,
            } => (2, run_id.as_str(), *attempt, *index),
            Self::Sweep { .. } => (3, "", 0, 0),
        }
    }
}

/// What a provider answered a call, as far as the lifecycle knows now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAnswer {
    /// The call is out and not answered yet.
    Pending,
    /// The call succeeded; a [`ProviderCall::Mount`] carries the directory the provider
    /// answered (its text, untrusted), every other call `None`.
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
    ///
    /// When a source's [`ProviderCall::Prepare`] succeeds, an implementation may send that
    /// source's [`ProviderCall::Mount`] itself, after the `Prepare`'s answer is in; asking for
    /// that `Mount` then finds it out or answered, as if it had been asked. A
    /// [`ProviderCall::Release`] of the source ends that: a `Prepare` still out is then not
    /// followed.
    fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer;

    /// Gives the providers the [`Wake`](super::Wake) to ring when a call's answer is in, so
    /// the start that waits on it goes on at once. A call that succeeded rings at once; a
    /// call refused or failed rings after a short pause, and is sent again only when the
    /// lifecycle asks for it again, after its own wait. The default keeps none: answers are
    /// then asked for at the next step.
    fn set_wake(&mut self, _wake: super::Wake) {}
}
