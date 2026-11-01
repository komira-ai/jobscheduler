//! Reading one run of a lease: `GET /v1/leases/{id}/runs/{run_id}`.

use std::collections::BTreeMap;

pub use js_agent_wire::{LogTail, Outcome};
use serde::Deserialize;

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
    /// job's own text, untrusted, and `None` is not the same as an empty output.
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
    /// A 200 whose body is not a run entry.
    #[error("the run entry cannot be read: {0}")]
    Malformed(String),
}

impl Client {
    /// Reads one run of a lease. `lease_id` and `run_id` are put in the path as given, so
    /// they must be ids the server minted.
    ///
    /// # Errors
    ///
    /// [`RunReadError`]: `Client` when the server cannot be reached, `UnknownLease`,
    /// `UnknownRun` and `Forgotten` for the three documented refusals, `Refused` for any
    /// other status, `Malformed` for a 200 that is not a run entry.
    pub fn run(&self, lease_id: &str, run_id: &str) -> Result<RunRead, RunReadError> {
        let _ = (self, lease_id, run_id);
        Err(RunReadError::Malformed("not written yet".to_owned()))
    }
}
