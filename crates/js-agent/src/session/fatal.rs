//! The answer of the server that ends the agent.

use std::fmt;

use js_agent_wire::ProtocolRange;

/// The server's answer ends the agent: it will not have this host, and the answer will
/// not change by asking again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal {
    /// The message refused: `hello` or `heartbeat`.
    pub what: &'static str,
    /// The HTTP status.
    pub status: u16,
    /// The server's code, such as `host-bound`, or `unreadable`.
    pub code: String,
    /// The majors the server speaks, when it said so (`unsupported-protocol`).
    pub supported: Option<ProtocolRange>,
    /// The capability at fault, when the server named one (`host-contract`,
    /// `execution-mismatch`).
    pub field: Option<String>,
    /// Which condition failed, when the server named one (`shape-not-accepted`).
    pub reason: Option<String>,
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let majors = self
            .supported
            .map(|range| format!("; it speaks majors {} to {}", range.min, range.max))
            .unwrap_or_default();
        let field = self
            .field
            .as_ref()
            .map(|field| format!("; the capability at fault is {field}"))
            .unwrap_or_default();
        let reason = self
            .reason
            .as_ref()
            .map(|reason| format!(", reason {reason}"))
            .unwrap_or_default();
        write!(
            f,
            "the server refused the {} with status {} ({}{reason}{majors}{field}); not retrying",
            self.what, self.status, self.code
        )
    }
}
