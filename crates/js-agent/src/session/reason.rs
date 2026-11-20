//! Why an exchange did not go through, and what became of a welcome's host credential.

use std::fmt;

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
