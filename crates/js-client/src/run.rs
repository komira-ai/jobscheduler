//! Reading one run of a lease: `GET /v1/leases/{id}/runs/{run_id}`.

use std::collections::BTreeMap;

pub use js_agent_wire::{LogTail, Outcome};
use serde::Deserialize;
use serde_json::Value;

use crate::client::Client;
use crate::error::ClientError;

/// One run, as the server's entry for it reads. Fields the server leaves out read as
/// `None` (or empty); fields this client does not know are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunRead {
    /// The run's id.
    pub run_id: String,
    /// Where the run is: `queued`, `starting`, `running`, `exited`, `stopping`, `ended`
    /// or `never-started`. Kept as the server's text so a state added later still reads.
    pub state: String,
    /// How the run ended, once it has.
    pub outcome: Option<Outcome>,
    /// The job's exit status, when it exited.
    pub exit_code: Option<i32>,
    /// What the server says about how the run ended, when it says anything.
    pub detail: Option<String>,
    /// The labels the run was queued with.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The end of the job's output. Only a contained run that exited has one; it is the
    /// job's own text, untrusted, and `None` is not the same as an empty output. It is
    /// the job's own untrusted bytes (ANSI escapes, U+202E, NUL): whoever prints it must
    /// strip terminal control sequences first.
    pub log_tail: Option<LogTail>,
}

/// Why one run could not be read.
#[derive(Debug, thiserror::Error)]
pub enum RunReadError {
    /// The server could not be reached, or went away before its answer was read.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// 404 `unknown-lease`: the server has no lease of that id.
    #[error("the server has no lease {0}")]
    UnknownLease(String),
    /// 404 `unknown-run`: the lease has no run of that id.
    #[error("the lease has no run {0}")]
    UnknownRun(String),
    /// 410 `run-forgotten`: the server restarted after it minted the id and the run's work
    /// is lost.
    #[error("run {0} was forgotten: the server restarted and its work is lost")]
    Forgotten(String),
    /// Any other status, with the `error` code of the body (empty when it has none).
    #[error("the server answered {status} {code}")]
    Refused {
        /// The HTTP status.
        status: u16,
        /// The body's `error` code, or empty.
        code: String,
    },
    /// An id that is not of the form the server mints, refused before any request is made.
    /// Names the argument (`lease_id` or `run_id`), never the text.
    #[error("{0} is not an id the server mints")]
    BadId(&'static str),
    /// A 200 whose body is not a run entry. The text names the category and the field
    /// path, never a value from the body, which may hold the job's output.
    #[error("the run entry cannot be read: {0}")]
    Malformed(String),
}

impl Client {
    /// Reads one run of a lease. `lease_id` and `run_id` are put in the path as given, so
    /// each is checked first against the form the server mints (a lease id is digits and
    /// `-`; a run id is `a-z`, digits and `-`, not empty) and anything else is refused
    /// before any request is made. Refusing, not percent-encoding: a caller that passes
    /// typed text gets an error it can show, instead of a request for some other id, and no
    /// character that means something to a URL or to a path reaches the server at all.
    ///
    /// The `log_tail` of the result is the job's own untrusted bytes: whoever prints it
    /// must strip terminal control sequences.
    ///
    /// # Errors
    ///
    /// [`RunReadError`]: `BadId` for an id of another form, `Client` when the server cannot be reached, `UnknownLease`,
    /// `UnknownRun` and `Forgotten` for the three documented refusals, `Refused` for any
    /// other status, `Malformed` for a 200 that is not a run entry.
    pub fn run(&self, lease_id: &str, run_id: &str) -> Result<RunRead, RunReadError> {
        if !lease_id.bytes().all(|b| b.is_ascii_digit() || b == b'-') || lease_id.is_empty() {
            return Err(RunReadError::BadId("lease_id"));
        }
        let minted = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !run_id.bytes().all(minted) || run_id.is_empty() {
            return Err(RunReadError::BadId("run_id"));
        }
        let reply = self.get(&format!("/v1/leases/{lease_id}/runs/{run_id}"))?;
        let code = reply
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match (reply.status, code.as_str()) {
            (200, _) => serde_path_to_error::deserialize(reply.body)
                .map_err(|why| RunReadError::Malformed(describe(&why))),
            (404, "unknown-lease") => Err(RunReadError::UnknownLease(lease_id.to_owned())),
            (404, "unknown-run") => Err(RunReadError::UnknownRun(run_id.to_owned())),
            (410, "run-forgotten") => Err(RunReadError::Forgotten(run_id.to_owned())),
            (status, code) => Err(RunReadError::Refused {
                status,
                code: code.to_owned(),
            }),
        }
    }
}

/// What is wrong with an entry, without a value from it: serde's own text quotes the value
/// it found (`invalid type: string "..."`), and the body may hold the job's output. A
/// missing field is named, since that text holds only the field's name from the schema.
fn describe(why: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let inner = why.inner().to_string();
    match inner.strip_prefix("missing field `") {
        Some(rest) => format!("missing field `{}`", rest.trim_end_matches('`')),
        None => format!("wrong type or value at `{}`", why.path()),
    }
}
