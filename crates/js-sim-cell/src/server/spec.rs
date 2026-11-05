//! What the cell's server is: the pool file, the token files and the flags of a lease
//! server whose hosts register themselves, read by the server's own [`Cli`] and
//! [`Config`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS};
use js_lease::{Clock, ManualClock, Monotonic};
use js_lease_server::{Cli, Config, StartupError};
use js_registry::DEFAULT_LOST_RETENTION_MS;
use js_types::Resources;

/// The client token of every cell's server: the bearer of [`super::ServerNode::client_call`].
pub const CLIENT_TOKEN: &str = "sim-cell-client-token-0123456789-abcdef";

/// The join token every agent of a cell says hello with.
pub const JOIN_TOKEN: &str = "sim-cell-join-token-0123456789-abcdefgh";

/// The class of the cell's one pool: what a lease asks for by name.
pub const POOL_CLASS: &str = "gpu-host";

/// What the pool books for every host, and what each of the cell's agents offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Thousandths of a core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Whole GPUs.
    pub gpus: u64,
    /// The class of the GPUs, when there are any.
    pub gpu_class: Option<String>,
    /// What every host offers besides its resources.
    pub capabilities: BTreeMap<String, String>,
}

impl Shape {
    /// The host the server's own tests register: 8 cores, 16 GiB, one `nvidia-a100` GPU,
    /// and `isa = x86-64-v3`.
    #[must_use]
    pub fn gpu_host() -> Self {
        Self {
            cpu_millis: 8_000,
            memory_bytes: 16 << 30,
            gpus: 1,
            gpu_class: Some("nvidia-a100".to_owned()),
            capabilities: BTreeMap::from([("isa".to_owned(), "x86-64-v3".to_owned())]),
        }
    }

    /// The shape as the scheduler's resources.
    ///
    /// # Errors
    ///
    /// When the GPU class does not match the GPU count, as [`Resources::new`].
    pub fn resources(&self) -> Result<Resources, js_types::TypeError> {
        let class = self.gpu_class.clone();
        Resources::new(self.cpu_millis, self.memory_bytes, self.gpus, class)
    }
}

/// A lease server with one pool of hosts that register themselves, as the cell runs it.
///
/// The pool admits as many machines as the scenario registers (`max_machines`), and one
/// join token is allowed as many hellos in a burst (`--agent-hello-burst`): every agent of
/// the cell shares the token, and the server's default bucket (60, then one a second)
/// would refuse a cell of more hosts at its start. The limiter is covered by the server's
/// own HTTP tests and is not what a scheduling scenario is about;
/// [`ServerSpec::hello_burst`] sets the burst back for a scenario that is about it.
///
/// The heartbeat interval and grace are the server's defaults (5 s and 15 s) unless
/// [`ServerSpec::heartbeat`] says otherwise, and hosts are forgotten after the
/// registry's default `lost_retention` unless [`ServerSpec::lost_retention`] says
/// otherwise. Usage recording is off (`--usage-off`): its store reads the system's clock
/// and commits on a thread of its own, so a run would depend on both. Placement is the
/// default, `registry`.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    pool: String,
    shape: Shape,
    hosts: usize,
    hello_burst: usize,
    interval: Duration,
    grace: Duration,
    lost_retention: Duration,
}

impl ServerSpec {
    /// A server whose one pool, `pool_name`, books `shape` for each of `hosts` hosts that
    /// register with [`JOIN_TOKEN`].
    #[must_use]
    pub fn registered_pool(pool_name: &str, shape: Shape, hosts: usize) -> Self {
        Self {
            pool: pool_name.to_owned(),
            shape,
            hosts,
            hello_burst: hosts,
            interval: Duration::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS),
            grace: Duration::from_millis(DEFAULT_GRACE_MS),
            lost_retention: Duration::from_millis(DEFAULT_LOST_RETENTION_MS),
        }
    }

    /// This server, telling its hosts to beat every `interval` and holding a host lost
    /// after `grace` of silence. The server refuses a grace that is not more than two
    /// intervals.
    #[must_use]
    pub fn heartbeat(mut self, interval: Duration, grace: Duration) -> Self {
        self.interval = interval;
        self.grace = grace;
        self
    }

    /// This server, forgetting a lost host that holds nothing `retention` after its grace
    /// ran out (the pool key `lost_retention`; at least a minute).
    #[must_use]
    pub fn lost_retention(mut self, retention: Duration) -> Self {
        self.lost_retention = retention;
        self
    }

    /// This server, allowing the join token `burst` hellos before it holds the token to
    /// one a second.
    #[must_use]
    pub fn hello_burst(mut self, burst: usize) -> Self {
        self.hello_burst = burst;
        self
    }

    /// The pool's name.
    #[must_use]
    pub fn pool(&self) -> &str {
        &self.pool
    }

    /// What the pool books for every host.
    #[must_use]
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// How often the hosts beat.
    #[must_use]
    pub fn heartbeat_interval(&self) -> Duration {
        self.interval
    }

    /// How long a silent host stays live.
    #[must_use]
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// The pool file (`--agent-pool-config`). The one place that spells the registering
    /// pool.
    #[must_use]
    pub fn pool_file(&self) -> String {
        let shape = &self.shape;
        let class = shape
            .gpu_class
            .iter()
            .map(|class| format!("gpu_class = {}\n", quoted(class)));
        let caps = shape
            .capabilities
            .iter()
            .map(|(k, v)| format!("{} = {}\n", quoted(k), quoted(v)));
        format!(
            "name = {}\nclass = {}\nmax_machines = {}\nheartbeat_interval = \"{}ms\"\n\
             grace = \"{}ms\"\nlost_retention = \"{}ms\"\n\n[shape]\ncpu_millis = {}\n\
             memory_bytes = {}\ngpus = {}\n{}\n[capabilities]\n{}",
            quoted(&self.pool),
            quoted(POOL_CLASS),
            self.hosts,
            self.interval.as_millis(),
            self.grace.as_millis(),
            self.lost_retention.as_millis(),
            shape.cpu_millis,
            shape.memory_bytes,
            shape.gpus,
            class.collect::<String>(),
            caps.collect::<String>(),
        )
    }

    /// Writes the pool file and the two token files into `dir` and returns the server's
    /// command line: the lease file in `dir`, a first boot, registration into the pool
    /// with [`JOIN_TOKEN`], the hello burst, and usage off.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, naming the flag of the file.
    pub fn args(&self, dir: &Path) -> Result<Vec<OsString>, StartupError> {
        let pool = write(
            dir,
            "agent-pool.toml",
            &self.pool_file(),
            "--agent-pool-config",
        )?;
        let token = write(dir, "token", CLIENT_TOKEN, "--token-file")?;
        let joins = write(
            dir,
            "agent-tokens",
            &format!("{JOIN_TOKEN}\n"),
            "--agent-token-file",
        )?;
        let flags: [(&str, OsString); 6] = [
            ("--state-file", dir.join("leases.toml").into_os_string()),
            ("--boot", "initialise".into()),
            ("--token-file", token),
            ("--agent-token-file", joins),
            ("--agent-pool-config", pool),
            ("--agent-hello-burst", self.hello_burst.to_string().into()),
        ];
        let pairs = flags
            .into_iter()
            .flat_map(|(flag, value)| [flag.into(), value]);
        let head = std::iter::once(OsString::from("js-lease-server"));
        let tail = std::iter::once(OsString::from("--usage-off"));
        Ok(head.chain(pairs).chain(tail).collect())
    }

    /// The server's configuration, read from [`ServerSpec::args`] by its own parser, with
    /// the lease table's save timings read from `clock`, so nothing reads the system's
    /// monotonic clock.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, or the server refuses its command line.
    pub fn config(&self, dir: &Path, clock: &ManualClock) -> Result<Config, StartupError> {
        let cli =
            Cli::try_parse_from(self.args(dir)?).map_err(StartupError::because("command line"))?;
        let config = Config::from_cli(cli)?;
        Ok(config.with_monotonic(Arc::new(Virtual(clock.clone()))))
    }
}

/// `text` as a TOML basic string, which the JSON string grammar is a part of.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

/// Writes `text` to the owner-only file `name` in `dir`, and returns its path.
fn write(dir: &Path, name: &str, text: &str, flag: &'static str) -> Result<OsString, StartupError> {
    let path = dir.join(name);
    fs::write(&path, text).map_err(StartupError::because(flag))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(StartupError::because(flag))?;
    Ok(path.into_os_string())
}

/// The lease table's monotonic clock: the virtual time, read from the cell's clock.
#[derive(Clone, Debug)]
struct Virtual(ManualClock);

impl Monotonic for Virtual {
    fn reading(&self) -> Duration {
        Duration::from_millis(self.0.now().millis())
    }
}
