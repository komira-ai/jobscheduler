//! Why an exchange did not go through.

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
