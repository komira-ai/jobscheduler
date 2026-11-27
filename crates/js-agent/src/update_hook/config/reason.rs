//! The reasons the config hook gives in the last line of a step: a closed set, so a token cannot
//! be misspelt or too long. The line itself is js-update-hook's
//! [`Report`](js_update_hook::Report).

use js_update_hook::ReasonToken;

/// The machine-readable reasons this hook gives. A test checks every one against
/// [`is_reason_token`](js_update_hook::is_reason_token).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The command line was refused.
    BadArgs,
    /// The `--params` file was unreadable or wrong.
    BadParams,
    /// Another step holds the lock of the state directory.
    Locked,
    /// The state directory or the record is not as the hook requires.
    StateInvalid,
    /// The flags file now in place cannot be used by the agent: it does not parse as the agent's
    /// flags, or names no usable host id.
    BadConfig,
    /// The unit could not be stopped, or did not stop in time.
    StopFailed,
    /// The unit could not be started, did not become active in time, or ran and died.
    StartFailed,
    /// `verify` waited out its bound and the agent did not come back with a new session.
    NoHello,
    /// The lease server could not be asked, or refused to answer.
    LeaseUnreachable,
    /// `verify` of an update whose files did not change, so that `cutover` restarted nothing:
    /// the agent's unit is not running.
    AgentDown,
    /// `verify` of an update whose files did not change: the agent's unit runs, and the lease
    /// server did not show the host `live` within the bound.
    AgentNotLive,
    /// `cleanup` before a `verify` succeeded.
    NotVerified,
    /// The copy of the files the package replaces could not be made, or a copy could not be
    /// removed.
    KeepFailed,
    /// `rollback` could not put the kept files back.
    RestoreFailed,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 14] = [
        Reason::BadArgs,
        Reason::BadParams,
        Reason::Locked,
        Reason::StateInvalid,
        Reason::BadConfig,
        Reason::StopFailed,
        Reason::StartFailed,
        Reason::NoHello,
        Reason::LeaseUnreachable,
        Reason::AgentDown,
        Reason::AgentNotLive,
        Reason::NotVerified,
        Reason::KeepFailed,
        Reason::RestoreFailed,
    ];

    /// The token in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::BadArgs => "bad_args",
            Reason::BadParams => "bad_params",
            Reason::Locked => "locked",
            Reason::StateInvalid => "state_invalid",
            Reason::BadConfig => "bad_config",
            Reason::StopFailed => "stop_failed",
            Reason::StartFailed => "start_failed",
            Reason::NoHello => "no_hello",
            Reason::LeaseUnreachable => "lease_unreachable",
            Reason::AgentDown => "agent_down",
            Reason::AgentNotLive => "agent_not_live",
            Reason::NotVerified => "not_verified",
            Reason::KeepFailed => "keep_failed",
            Reason::RestoreFailed => "restore_failed",
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
