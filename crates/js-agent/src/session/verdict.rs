//! How the status and code of an answer read: which answers end the agent, which say the
//! session is lost, and which are retried (see the parent module's documentation, which
//! justifies each fatal code).

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
/// the parent module's documentation). Every other 403 is retried.
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
