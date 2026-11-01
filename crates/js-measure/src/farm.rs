//! The lease server as the harness sees it: the few calls it makes, as a trait with two
//! implementations, [`crate::http::HttpFarm`] (the live server) and [`crate::fake::FakeFarm`]
//! (a simulated one with a fake clock).

use js_client::LogTail;

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

/// A lease asked for: the whole host is `cpu_millis` and `memory_bytes` of the class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAsk {
    /// The lease class.
    pub class: String,
    /// CPU booked, in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory booked, in bytes.
    pub memory_bytes: u64,
    /// The longest the lease may live, in seconds.
    pub max_wall_seconds: u64,
}

/// The answer to a lease request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// The lease is held now.
    Held {
        /// The lease id.
        id: String,
        /// The host it holds.
        host: String,
    },
    /// The server has not placed it yet.
    Pending {
        /// The lease id.
        id: String,
    },
}

/// Where a lease stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Held on this host.
    Held {
        /// The host.
        host: String,
    },
    /// Not placed yet.
    Pending,
    /// Not listed, or ended.
    Gone,
}

/// A run asked for on a held lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAsk {
    /// The container image.
    pub image: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// CPU limit, in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory limit, in bytes.
    pub memory_bytes: u64,
    /// Wall limit, in milliseconds.
    pub wall_ms: u64,
    /// The profile the server learns the run's cost under, when the workload names one.
    pub profile: Option<String>,
}

/// How far a run has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Queued or starting.
    Waiting,
    /// Running (or being stopped).
    Running,
    /// Over: exited, ended or never started.
    Over,
}

/// A run as the server shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStanding {
    /// The server's state name.
    pub state: String,
    /// How it ended, once it has (`succeeded`, `out_of_memory`, ...).
    pub outcome: Option<String>,
    /// The end of what the job printed, when the server holds it (a contained run that
    /// exited); `None` is not an empty output.
    pub log_tail: Option<LogTail>,
}

impl RunStanding {
    /// The phase of the state name.
    #[must_use]
    pub fn phase(&self) -> Phase {
        match self.state.as_str() {
            "running" | "stopping" => Phase::Running,
            "exited" | "ended" | "never-started" => Phase::Over,
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
    /// Leases that ran on the host in the minute.
    pub leases: u64,
}

/// The cost curve the server has learned for a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curve {
    /// Runs it was fitted from.
    pub runs: u64,
    /// Working set of each minute of a run's life, in bytes.
    pub ws_bytes: Vec<u64>,
    /// CPU of each minute, in thousandths of a core; empty when the fit learned no cores.
    pub cores_milli: Vec<u64>,
}

/// The calls the harness makes. Every method is one request.
pub trait Farm {
    /// Asks for a lease.
    ///
    /// # Errors
    /// [`FarmError`] when the server refuses or cannot be read.
    fn acquire(&self, ask: &LeaseAsk) -> Result<Acquired, FarmError>;
    /// Where a lease stands.
    ///
    /// # Errors
    /// [`FarmError`] when the server cannot be read.
    fn lease(&self, id: &str) -> Result<Standing, FarmError>;
    /// Queues a run on a held lease and returns its id.
    ///
    /// # Errors
    /// [`FarmError`] when the server refuses.
    fn start_run(&self, lease: &str, ask: &RunAsk) -> Result<String, FarmError>;
    /// Where a run stands.
    ///
    /// # Errors
    /// [`FarmError`] when the server cannot be read.
    fn run(&self, lease: &str, run: &str) -> Result<RunStanding, FarmError>;
    /// Releases a lease.
    ///
    /// # Errors
    /// [`FarmError`] when the server refuses.
    fn release(&self, lease: &str) -> Result<(), FarmError>;
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
    /// The curve for a profile, `None` when the server has learned none.
    ///
    /// # Errors
    /// [`FarmError`] when the server cannot be read.
    fn curve(&self, profile: &str) -> Result<Option<Curve>, FarmError>;
}
