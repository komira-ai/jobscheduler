//! The reasons the server's hook gives in the last line of a step: a closed set, so a token
//! cannot be misspelt or too long. The line itself is js-update-hook's
//! [`Report`](js_update_hook::Report).

use js_update_hook::ReasonToken;

/// The machine-readable reasons this hook gives. A test checks every one against
/// [`is_reason_token`](js_update_hook::is_reason_token).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The command line was refused.
    BadArgs,
    /// The `--params` file, or a file a flag names, was unreadable or wrong.
    BadParams,
    /// Another step holds the lock of the state directory.
    Locked,
    /// The state directory, the record, the declaration or the release link is not as the hook
    /// requires.
    StateInvalid,
    /// The store could not be reached, or did not answer the probe of its writer lock or the
    /// read of its schema.
    StoreUnreachable,
    /// The store holds no schema, or one older than this release runs on.
    SchemaUnsupported,
    /// The unit could not be stopped, or did not stop in time.
    StopFailed,
    /// The unit stopped and the store's writer lock is still held.
    LockNotReleased,
    /// The release link could not be moved.
    LinkFailed,
    /// The unit could not be started, did not become active in time, or ran and died.
    StartFailed,
    /// `verify` waited out its bound and no new process took the store's writer lock.
    NoLock,
    /// `verify` waited out its bound: a new process holds the writer lock and `/healthz` does
    /// not answer 200.
    NotServing,
    /// `rollback` of a first install that has moved the release link: there is no previous
    /// release to restore.
    NoPreviousRelease,
    /// `rollback` waited out its bound and the previous release did not take the writer lock.
    RollbackNotServing,
    /// `cleanup` before a `verify` succeeded.
    NotVerified,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 15] = [
        Reason::BadArgs,
        Reason::BadParams,
        Reason::Locked,
        Reason::StateInvalid,
        Reason::StoreUnreachable,
        Reason::SchemaUnsupported,
        Reason::StopFailed,
        Reason::LockNotReleased,
        Reason::LinkFailed,
        Reason::StartFailed,
        Reason::NoLock,
        Reason::NotServing,
        Reason::NoPreviousRelease,
        Reason::RollbackNotServing,
        Reason::NotVerified,
    ];

    /// The token in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::BadArgs => "bad_args",
            Reason::BadParams => "bad_params",
            Reason::Locked => "locked",
            Reason::StateInvalid => "state_invalid",
            Reason::StoreUnreachable => "store_unreachable",
            Reason::SchemaUnsupported => "schema_unsupported",
            Reason::StopFailed => "stop_failed",
            Reason::LockNotReleased => "lock_not_released",
            Reason::LinkFailed => "link_failed",
            Reason::StartFailed => "start_failed",
            Reason::NoLock => "no_lock",
            Reason::NotServing => "not_serving",
            Reason::NoPreviousRelease => "no_previous_release",
            Reason::RollbackNotServing => "rollback_not_serving",
            Reason::NotVerified => "not_verified",
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
