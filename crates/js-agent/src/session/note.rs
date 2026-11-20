//! What a session tells its caller about an exchange: why one failed ([`Reason`]), what became
//! of a host credential ([`CredentialState`]) and the notes worth a log line ([`Note`]).

use std::fmt;
use std::time::Duration;

use js_agent_wire::RefusalReason;

/// Why an exchange did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The server could not be reached, or went away before it answered.
    Unreachable(String),
    /// The server answered with this status and code.
    Status {
        /// The HTTP status.
        status: u16,
        /// The server's code, or `unreadable`.
        code: String,
    },
    /// The answer, or the message to send, failed its checks; this is the wire code.
    Unusable(&'static str),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "{why}"),
            Self::Status { status, code } => write!(f, "status {status} ({code})"),
            Self::Unusable(code) => write!(f, "unusable message ({code})"),
        }
    }
}

/// What became of the host credential a welcome carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// The welcome carried none, or the session has nowhere to keep one: the host goes on
    /// with its token.
    None,
    /// It is on disk and authenticates every later call.
    Saved,
    /// It could not be written. It authenticates this process's calls, but a restart cannot
    /// register the host again: the token it was issued for is dead.
    Unsaved,
    /// The transport would not use it. The token is dead, so the next call is refused.
    Refused,
}

/// The text a [`Note::Registered`] adds for the credential: nothing, or a clause that starts
/// with a semicolon.
impl fmt::Display for CredentialState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "",
            Self::Saved => "; the host credential is saved",
            Self::Unsaved => {
                "; the host credential could not be saved: a restart cannot register this host again"
            }
            Self::Refused => {
                "; the transport refused the host credential: this host can no longer authenticate"
            }
        })
    }
}

/// Something the operator may want to read in the log. None of them holds a token or a
/// session secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The server welcomed the host.
    Registered {
        /// The session's epoch.
        epoch: u64,
        /// How often the host heartbeats.
        interval: Duration,
        /// What became of the host credential the welcome carried, if it carried one.
        credential: CredentialState,
    },
    /// The server no longer holds the session; the agent says hello again.
    Lost {
        /// The server's code.
        code: String,
    },
    /// An exchange failed and will be tried again.
    Retrying {
        /// `hello` or `heartbeat`.
        what: &'static str,
        /// Why it failed.
        reason: Reason,
    },
    /// A heartbeat answer carried commands, which this agent does not act on: it runs no
    /// jobs.
    Ignored {
        /// How many.
        commands: usize,
    },
    /// The server granted a whole-node lease; the agent prepares the host for it.
    Preparing {
        /// The lease.
        lease: String,
    },
    /// The agent refused the lease it was preparing for.
    Refused {
        /// The code sent.
        code: RefusalReason,
    },
    /// The lease ended; the agent restores the host.
    Restoring,
    /// The job worker had no room for a heartbeat answer, which was dropped; the server
    /// sends what it said again.
    Busy,
    /// The host announced that it is draining.
    Draining {
        /// Whether the server acknowledged it.
        told: bool,
    },
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registered {
                epoch,
                interval,
                credential,
            } => write!(
                f,
                "registered, epoch {epoch}, heartbeat every {}ms{credential}",
                interval.as_millis()
            ),
            Self::Lost { code } => write!(f, "the server lost the session ({code})"),
            Self::Retrying { what, reason } => write!(f, "{what} failed: {reason}"),
            Self::Ignored { commands } => {
                write!(f, "ignoring {commands} command(s): this agent runs no jobs")
            }
            Self::Preparing { lease } => write!(f, "preparing the host for lease {lease}"),
            Self::Refused { code } => {
                write!(f, "refusing the whole-node lease: {}", String::from(*code))
            }
            Self::Restoring => write!(f, "the whole-node lease ended: restoring the host"),
            Self::Busy => write!(
                f,
                "the job worker is busy: an answer was dropped and will be sent again"
            ),
            Self::Draining { told: true } => write!(f, "draining: the server was told"),
            Self::Draining { told: false } => {
                write!(f, "draining: the server could not be told")
            }
        }
    }
}
