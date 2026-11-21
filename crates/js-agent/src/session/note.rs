//! The lines a session gives the operator to read ([`Note`]).

use std::fmt;
use std::time::Duration;

use js_agent_wire::RefusalReason;

use super::{CredentialState, Reason};

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
