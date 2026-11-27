//! How the status and code of an answer read: which answers end the agent, which say the
//! session is lost, and which are retried (see the parent module's documentation).
//!
//! The answers that end the agent, each a refusal of a hello or a heartbeat that only the
//! operator can resolve:
//!
//! - 401, with any code: the server holds no such bearer (a revoked or mistyped token, a
//!   credential it never issued). A server that could not read its store of bearers
//!   answers 503, which is retried, so a 401 is the server's word that the token is wrong.
//! - 403 `unknown-pool` (the server serves no such pool), `host-bound` (the host id was
//!   admitted with another credential), `host-listed` (the operator lists the id in a
//!   static pool), `shape-not-accepted` (the pool does not take the host's shape; the
//!   refusal names the condition under `reason`, and the agent prints it),
//!   `host-contract` and `execution-mismatch` (the host lacks what the pool's execution
//!   mode requires; the refusal names the capability at fault): the pool's settings or the
//!   host's flags must change.
//! - 403 `join-token-expired`, `join-token-used`, `join-token-instance`,
//!   `join-token-not-admitted` and `join-token-credential-required`: the one-time token
//!   cannot admit this host (past its time, spent on another host, bound to another
//!   instance, not the token that registered the host, or the server requires a kept
//!   credential and the agent has no state root to keep one in); a new token or a state
//!   root is the operator's to give.
//! - 400 `unsupported-protocol`: the server speaks no protocol major the agent does; one
//!   of the two must be updated.
//!
//! Every other answer that is not a success or the loss of the session is retried.

use js_agent_wire::refusal;

/// How an answer's status and code read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The message went through.
    Done,
    /// The server and the agent disagree about the session.
    Conflict,
    /// Try again later.
    Retry,
    /// Do not try again.
    Fatal,
}

/// The 403 codes that end the agent: each names something only the operator can change (see
/// the module documentation). Every other 403 is retried.
const OPERATOR_403: [&str; 11] = [
    refusal::UNKNOWN_POOL,
    refusal::HOST_BOUND,
    refusal::HOST_LISTED,
    refusal::SHAPE_NOT_ACCEPTED,
    refusal::HOST_CONTRACT,
    refusal::EXECUTION_MISMATCH,
    "join-token-expired",
    "join-token-used",
    "join-token-instance",
    "join-token-not-admitted",
    "join-token-credential-required",
];

/// The verdict on an answer of `status` whose body carries `code`.
pub(super) fn verdict(status: u16, code: &str) -> Verdict {
    match (status, code) {
        (200, _) => Verdict::Done,
        (403, refusal::SESSION_MISMATCH) | (409, _) => Verdict::Conflict,
        (401, _) | (400, refusal::UNSUPPORTED_PROTOCOL) => Verdict::Fatal,
        (403, code) if OPERATOR_403.contains(&code) => Verdict::Fatal,
        _ => Verdict::Retry,
    }
}
