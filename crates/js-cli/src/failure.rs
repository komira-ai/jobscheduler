//! What ends a command without success, and the exit status each kind has.

use js_client::ClientError;

use crate::events::ForwardedSignal;

/// The exit statuses of `js`.
pub mod exit {
    /// Success: the lease is held, renewed, freed or listed, or the command of `lease run`
    /// ended with status 0 (any other status of the command is passed through).
    pub const OK: u8 = 0;
    /// The server or the network failed, or answered something this client does not know.
    pub const SERVER: u8 = 1;
    /// A flag or a file it names is wrong. Statuses clap reports for a command line it
    /// refuses are 2 as well.
    pub const USAGE: u8 = 2;
    /// The server refused: no capacity, no fit, an unsupported class, an ended or unknown
    /// lease, or a lease that is not held.
    pub const REFUSED: u8 = 3;
    /// The server refused the request itself: a rule of the start contract, a label, a
    /// class, or a body it cannot read.
    pub const CONTRACT: u8 = 4;
    /// A pending start was not confirmed within `--wait-pending`.
    pub const UNRESOLVED: u8 = 5;
    /// A release was accepted but the stop is not confirmed yet; the server retries it.
    pub const RELEASING: u8 = 6;
    /// `lease run` lost its lease: a renewal found it gone, or none succeeded before the
    /// deadline short of the server's expiry, and the command was stopped.
    pub const LEASE_LOST: u8 = 7;
    /// `lease run` ended, but the release of the lease could not be confirmed after every
    /// attempt: the lease may stay held until its ttl runs out. It replaces the command's
    /// own status, which is said on stderr.
    pub const RELEASE_UNCONFIRMED: u8 = 8;
    /// `lease run` could not start its command.
    pub const NOT_STARTED: u8 = 127;
}

/// Why a command failed. The text is what `js` prints after `js: `; it never holds the
/// token.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    /// A flag or a file it names is unusable.
    #[error("{0}")]
    Usage(String),
    /// The server or the network failed.
    #[error("{0}")]
    Server(String),
    /// The server refused with this code.
    #[error("refused: {0}")]
    Refused(String),
    /// The server refused the request itself, with this code and reason.
    #[error("rejected: {0}")]
    Contract(String),
    /// A pending start was not confirmed in time; the lease was released.
    #[error("lease {id} is still pending after {waited}s; released it")]
    Unresolved {
        /// The pending lease.
        id: String,
        /// How long the client waited, in whole seconds.
        waited: u64,
    },
    /// The command of `lease run` could not be started.
    #[error("cannot start {program}: {why}")]
    NotStarted {
        /// The command, as given.
        program: String,
        /// The system's reason.
        why: String,
    },
    /// The answer to a start was lost or unusable (the connection failed, the server
    /// answered 5xx or a status this client does not know, or the body lacks a field), so
    /// the start may have created a lease. It is not repeated.
    #[error("{0}; the request may have created a lease: run \"js lease list\" before trying again")]
    Unconfirmed(String),
    /// The lease is gone from the server.
    #[error("lease {0} is gone from the server; stopped the command")]
    LeaseLost(String),
    /// No renewal succeeded before the deadline short of the lease's expiry, so the
    /// server is about to expire it.
    #[error("lease {0} could not be renewed before its deadline; stopped the command")]
    LeaseUnrenewed(String),
    /// A signal ended `lease run` while it waited for the lease; nothing was started.
    #[error("interrupted by {} before the lease was held; asked the server to release it", .0.name())]
    Interrupted(ForwardedSignal),
}

impl From<ClientError> for Failure {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::Flag { flag, why } => Self::flag(flag, why),
            ClientError::Server(why) => Self::Server(why),
        }
    }
}

impl Failure {
    /// A usage failure naming `flag`.
    pub(crate) fn flag(flag: &str, why: impl std::fmt::Display) -> Self {
        Self::Usage(format!("{flag}: {why}"))
    }

    /// The exit status and the machine-readable kind of this failure.
    pub(crate) fn parts(&self) -> (u8, &'static str) {
        match self {
            Self::Usage(_) => (exit::USAGE, "usage"),
            Self::Server(_) => (exit::SERVER, "server"),
            Self::Unconfirmed(_) => (exit::SERVER, "acquire-unconfirmed"),
            Self::Refused(_) => (exit::REFUSED, "refused"),
            Self::Contract(_) => (exit::CONTRACT, "rejected"),
            Self::Unresolved { .. } => (exit::UNRESOLVED, "unresolved"),
            Self::NotStarted { .. } => (exit::NOT_STARTED, "not-started"),
            Self::LeaseLost(_) | Self::LeaseUnrenewed(_) => (exit::LEASE_LOST, "lease-lost"),
            Self::Interrupted(signal) => (128 + signal.number(), "interrupted"),
        }
    }
}
