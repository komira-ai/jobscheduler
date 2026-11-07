//! The JSON object a step writes as the last line of its standard output: js-update-hook's
//! [`Report`](js_update_hook::Report), with the reasons this hook gives ([`Reason`]).

use js_update_hook::ReasonToken;
pub use js_update_hook::{Class, MAX_REASON_LEN, is_reason_token};

/// The machine-readable reasons this hook gives. A closed set, so a token cannot be
/// misspelt or too long; a test checks every one against [`is_reason_token`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The step exists on the command line and has no body yet.
    NotImplemented,
    /// The command line was refused.
    BadArgs,
    /// The `--params` file was unreadable or wrong.
    BadParams,
    /// Another step holds the lock of the state directory.
    Locked,
    /// The state directory, the record or the settings are not as the hook requires.
    StateInvalid,
    /// The agent is tainted: no version can repair it (exit 76).
    Tainted,
    /// A job's post-job checks are owed: no version can repair it (exit 76).
    OwedCheck,
    /// The staged release is not an agent this hook can run.
    BadArtifact,
    /// The staged agent is not the version, or lacks the runtime, the order names.
    VersionMismatch,
    /// The drain bound passed with a lease still running (exit 75).
    DrainBound,
    /// The lease server could not be reached, or refused the request.
    LeaseUnreachable,
    /// `cleanup` before a `verify` succeeded.
    NotVerified,
    /// The cordon's cap leaves nothing to ask for.
    CordonCapReached,
    /// The unit could not be stopped, or did not stop in time.
    StopFailed,
    /// The release link is not where the step needs it (the updater moves it), or cannot be
    /// read.
    LinkFailed,
    /// The unit could not be started, or did not become active in time.
    StartFailed,
    /// A cordon is in force but the record's mark of it was never confirmed: it may lapse
    /// during the stop, and nothing renews it.
    CordonUnconfirmed,
    /// `verify` waited out its bound and no new session of the target said hello.
    NoHello,
    /// `rollback` of a first install that has begun its cutover: there is no previous release
    /// to restore.
    NoPreviousRelease,
    /// A first install, and the join token file is not there as the agent needs it and the
    /// state root holds no host credential (exit 75).
    JoinTokenMissing,
    /// The unit changes, and its cgroup still holds a process after the stop.
    UnitNotEmpty,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 21] = [
        Reason::NotImplemented,
        Reason::BadArgs,
        Reason::BadParams,
        Reason::Locked,
        Reason::StateInvalid,
        Reason::Tainted,
        Reason::OwedCheck,
        Reason::BadArtifact,
        Reason::VersionMismatch,
        Reason::DrainBound,
        Reason::LeaseUnreachable,
        Reason::NotVerified,
        Reason::CordonCapReached,
        Reason::StopFailed,
        Reason::LinkFailed,
        Reason::StartFailed,
        Reason::CordonUnconfirmed,
        Reason::NoHello,
        Reason::NoPreviousRelease,
        Reason::JoinTokenMissing,
        Reason::UnitNotEmpty,
    ];

    /// The token in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::NotImplemented => "not_implemented",
            Reason::BadArgs => "bad_args",
            Reason::BadParams => "bad_params",
            Reason::Locked => "locked",
            Reason::StateInvalid => "state_invalid",
            Reason::Tainted => "tainted",
            Reason::OwedCheck => "owed_check",
            Reason::BadArtifact => "bad_artifact",
            Reason::VersionMismatch => "version_mismatch",
            Reason::DrainBound => "drain_bound",
            Reason::LeaseUnreachable => "lease_unreachable",
            Reason::NotVerified => "not_verified",
            Reason::CordonCapReached => "cordon_cap_reached",
            Reason::StopFailed => "stop_failed",
            Reason::LinkFailed => "link_failed",
            Reason::StartFailed => "start_failed",
            Reason::CordonUnconfirmed => "cordon_unconfirmed",
            Reason::NoHello => "no_hello",
            Reason::NoPreviousRelease => "no_previous_release",
            Reason::JoinTokenMissing => "join_token_missing",
            Reason::UnitNotEmpty => "unit_not_empty",
        }
    }
}

impl ReasonToken for Reason {
    fn as_str(self) -> &'static str {
        Reason::as_str(self)
    }
}

/// What a step tells the updater besides its exit code, with this hook's [`Reason`]s.
pub type Report = js_update_hook::Report<Reason>;
