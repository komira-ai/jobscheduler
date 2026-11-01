//! The JSON object a step writes as the last line of its standard output.
//!
//! ```json
//! {"class":"release","reason":"...","retry_after_secs":300,"reversible":true,
//!  "promotion_pending":false,"detail":"..."}
//! ```
//!
//! The updater reads the last line leniently: a missing or malformed line is `{}`, a `release`
//! failure of a step that can still be undone with nothing pending. This module writes the
//! line in the shape the contract's own crate reads, so the updater never has to guess.

use std::io::{self, Write};

use serde_json::{Map, Value};

/// The longest `reason` token.
pub const MAX_REASON_LEN: usize = 64;

/// Whether `reason` is a token the updater accepts: lowercase letters, digits and `_`, one to
/// [`MAX_REASON_LEN`] bytes.
#[must_use]
pub fn is_reason_token(reason: &str) -> bool {
    (1..=MAX_REASON_LEN).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whom a failure is blamed on (it decides whether the fleet rolls back).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Class {
    /// The new version is wrong. The default.
    #[default]
    Release,
    /// Something remote or shared failed (an unreachable server); the release may be fine.
    Environment,
}

impl Class {
    /// The word in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Release => "release",
            Class::Environment => "environment",
        }
    }
}

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
    /// The state directory, the record or the declaration is not as the hook requires.
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
    /// The release link could not be moved.
    LinkFailed,
    /// The unit could not be started, or did not become active in time.
    StartFailed,
    /// A cordon is in force but the record's mark of it was never confirmed: it may lapse
    /// during the stop, and nothing renews it.
    CordonUnconfirmed,
    /// `verify` waited out its bound and no new session of the target said hello.
    NoHello,
    /// `rollback` of a first install that has begun: there is no previous release to restore.
    NoPreviousRelease,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 19] = [
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
        }
    }
}

/// What a step tells the updater besides its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Whom a failure is blamed on.
    pub class: Class,
    /// A short token; prose goes in `detail`.
    pub reason: Option<Reason>,
    /// With exit 75: when the updater may order the host again.
    pub retry_after_secs: Option<u64>,
    /// Whether the hook can still undo what it has done. A step that says `false` is the
    /// commit.
    pub reversible: bool,
    /// Set by `verify`: serving, but not yet fully its own.
    pub promotion_pending: bool,
    /// A short text for the log and the alert.
    pub detail: Option<String>,
}

impl Default for Report {
    /// The report of a step that says nothing: `{}` to the updater.
    fn default() -> Report {
        Report {
            class: Class::Release,
            reason: None,
            retry_after_secs: None,
            reversible: true,
            promotion_pending: false,
            detail: None,
        }
    }
}

impl Report {
    /// A `release` failure with a reason and a text.
    #[must_use]
    pub fn failure(reason: Reason, detail: impl Into<String>) -> Report {
        Report {
            reason: Some(reason),
            detail: Some(detail.into()),
            ..Report::default()
        }
    }

    /// The line to print: one JSON object, no newline. `class`, `reversible` and
    /// `promotion_pending` are always there; the others when set.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut object = Map::new();
        object.insert("class".to_owned(), Value::from(self.class.as_str()));
        object.insert("reversible".to_owned(), Value::from(self.reversible));
        object.insert(
            "promotion_pending".to_owned(),
            Value::from(self.promotion_pending),
        );
        let optional = [
            ("reason", self.reason.map(|r| Value::from(r.as_str()))),
            ("retry_after_secs", self.retry_after_secs.map(Value::from)),
            ("detail", self.detail.clone().map(Value::from)),
        ];
        for (key, value) in optional {
            object.extend(value.map(|value| (key.to_owned(), value)));
        }
        Value::Object(object).to_string()
    }

    /// Writes the line and a newline: what a step does last, to its standard output.
    ///
    /// # Errors
    /// The write failed.
    pub fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", self.to_line())
    }
}
