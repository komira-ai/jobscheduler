//! The job server as the harness sees it: the few calls it makes, as a trait with two
//! implementations, [`crate::http::HttpFarm`] (the live server) and [`crate::fake::FakeFarm`]
//! (a simulated one with a fake clock).

/// Why a call to the farm failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FarmError {
    /// The server could not be reached or its answer could not be read.
    #[error("cannot reach the server: {0}")]
    Transport(String),
    /// The server refused the request.
    #[error("the server answered {status} {code}")]
    Refused {
        /// The HTTP status.
        status: u16,
        /// The error code of the answer, empty when it had none.
        code: String,
    },
    /// The server answered with a body this harness cannot read.
    #[error("unreadable answer: {0}")]
    Malformed(String),
}

/// The end of what a job printed: the tails of its two streams.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogTail {
    /// The tail of the job's standard output.
    pub stdout: String,
    /// The tail of the job's standard error.
    pub stderr: String,
    /// Whether the job printed more than the two tails hold.
    pub truncated: bool,
}

/// A job asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobAsk {
    /// The container image, pinned by digest.
    pub image: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// CPU asked for, in thousandths of a core; sent unless the job takes the whole host.
    pub cpu_millis: u64,
    /// Memory asked for, in bytes; sent unless the job takes the whole host.
    pub memory_bytes: u64,
    /// Whether the job needs a whole host, in place of `cpu_millis` and `memory_bytes`.
    pub whole_host: bool,
    /// The longest the job may run, in seconds.
    pub wall_limit_seconds: u64,
}

/// How far a job has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Queued, placed or starting.
    Waiting,
    /// Running.
    Running,
    /// Over: it ended in any state.
    Over,
}

/// A job as the server shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStanding {
    /// The server's state name (`JOB_STATE_RUNNING`, ...).
    pub state: String,
    /// The host the job is placed on, once it is.
    pub host: Option<String>,
    /// How it ended, once it has (`succeeded`, `out_of_memory`, ...).
    pub outcome: Option<String>,
    /// The end of what the job printed, once it has ended.
    pub log_tail: Option<LogTail>,
}

impl JobStanding {
    /// The phase of the state name; a state this harness does not know reads as waiting.
    #[must_use]
    pub fn phase(&self) -> Phase {
        match self.state.as_str() {
            "JOB_STATE_RUNNING" => Phase::Running,
            "JOB_STATE_SUCCEEDED"
            | "JOB_STATE_FAILED"
            | "JOB_STATE_CANCELLED"
            | "JOB_STATE_LOST"
            | "JOB_STATE_FAILED_TO_START" => Phase::Over,
            _ => Phase::Waiting,
        }
    }
}

/// The size of a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostShape {
    /// Thousandths of a core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
}

/// One minute of a host in the usage store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageRow {
    /// Minutes since the epoch.
    pub minute: u64,
    /// Mean CPU of the minute, in thousandths of a core.
    pub cpu_millis: u64,
    /// Largest working set of the minute, in bytes.
    pub ws_bytes: u64,
    /// Allocations that ran on the host in the minute, as the usage row counts them.
    pub leases: u64,
}

/// The calls the harness makes. Every method is one request.
pub trait Farm {
    /// Submits a job and returns its id.
    ///
    /// # Errors
    /// [`FarmError`] when the server refuses or cannot be read.
    fn submit(&self, ask: &JobAsk) -> Result<String, FarmError>;
    /// Where a job stands; once it has ended, how it ended and what it printed.
    ///
    /// # Errors
    /// [`FarmError`] when the server cannot be read.
    fn job(&self, id: &str) -> Result<JobStanding, FarmError>;
    /// Cancels a job.
    ///
    /// # Errors
    /// [`FarmError`] when the server refuses.
    fn cancel(&self, id: &str) -> Result<(), FarmError>;
    /// The size of a host.
    ///
    /// # Errors
    /// [`FarmError`] when the host is not listed or the server cannot be read.
    fn host(&self, id: &str) -> Result<HostShape, FarmError>;
    /// The usage rows of a host for the minutes `from..=to`.
    ///
    /// # Errors
    /// [`FarmError`] when the server cannot be read.
    fn usage_rows(&self, host: &str, from: u64, to: u64) -> Result<Vec<UsageRow>, FarmError>;
}
