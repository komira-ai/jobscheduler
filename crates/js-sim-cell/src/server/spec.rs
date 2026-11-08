//! What the cell's server is: the pool file, the token files, the store file and the flags
//! of a server whose hosts register themselves and which serves the job service, read by
//! the server's own [`Cli`] and [`Config`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, DEFAULT_HEARTBEAT_INTERVAL_MS};
use js_lease::{Clock, ManualClock, Monotonic};
use js_lease_server::{Cli, Config, SharedClock, StartupError};
use js_registry::{DEFAULT_LOST_RETENTION_MS, SessionSource};
use js_types::Resources;

use super::store::{STORE_LINE_FILE, Store, StoreAt};
use crate::elastic::{self, Cloud, ElasticPool};

/// The client token of every cell's server: the bearer of [`super::ServerNode::client_call`].
pub const CLIENT_TOKEN: &str = "sim-cell-client-token-0123456789-abcdef";

/// The join token every agent of a cell says hello with.
pub const JOIN_TOKEN: &str = "sim-cell-join-token-0123456789-abcdefgh";

/// The class of the cell's one pool.
pub const POOL_CLASS: &str = "gpu-host";

/// The class of a pool whose hosts are given whole ([`ServerSpec::whole_node_pool`]).
pub const WHOLE_CLASS: &str = "whole-node";

/// The capability key a host's architecture is given under.
pub const ARCH_KEY: &str = "arch";

/// The architecture a server's shape is given when it names none.
pub const ARCH: &str = "x86_64";

/// The `--placement` value of every cell's server: the placement that places queued jobs.
const PLACEMENT: &str = "js-sched";

/// The address `--job-listen` names: on the loopback, as the server demands of an address
/// served without TLS, and never bound, since the cell's server runs in process.
pub const JOB_LISTEN: &str = "127.0.0.1:0";

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

    /// The host of [`Shape::gpu_host`] with no GPU: the shape a whole-host job can be
    /// started on, since a start names each GPU of the host it books by its index and the
    /// server sends none for a job that books GPUs.
    #[must_use]
    pub fn cpu_host() -> Self {
        Self {
            gpus: 0,
            gpu_class: None,
            ..Self::gpu_host()
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

/// A server with one pool of hosts that register themselves and the job service, placing as
/// `js-sched`, as the cell runs it.
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
/// and commits on a thread of its own, so a run would depend on both. The pool's class is
/// [`POOL_CLASS`] ([`WHOLE_CLASS`] for [`ServerSpec::whole_node_pool`]); placement
/// (`--placement`) is `js-sched`, the one placement that places queued jobs, and the
/// job service is on (`--job-listen`, an address that is never bound: the cell reaches
/// the service in process). The shape is given the capability [`ARCH_KEY`] = [`ARCH`]
/// when it names no architecture: `js-sched` refuses an agent or joined pool without
/// one, and places an elastic pool's machines by the one their agents report. The agents
/// announce, and the client's jobs ask for, the shape's capabilities, this one too. The
/// cell's agents speak the per-job messages (they report
/// [`js_agent_wire::PER_JOB_CAPABILITY`] in their hello). The server keeps its state in a
/// SQLite store in its directory unless [`ServerSpec::store`] says otherwise (module
/// `store`).
#[derive(Clone, Debug)]
pub struct ServerSpec {
    pool: String,
    class: &'static str,
    shape: Shape,
    hosts: usize,
    hello_burst: usize,
    interval: Duration,
    grace: Duration,
    lost_retention: Duration,
    /// Whether the server boots over the lease file a server before it left
    /// (`--boot resume`), rather than a first boot (`--boot initialise`).
    resume: bool,
    /// The elastic pool, for a server of [`ServerSpec::elastic_pool`].
    elastic: Option<ElasticPool>,
    /// Which store the server keeps its state in (`--store`).
    store: Store,
}

impl ServerSpec {
    /// A server whose one pool, `pool_name`, books `shape` for each of `hosts` hosts that
    /// register with [`JOIN_TOKEN`].
    #[must_use]
    pub fn registered_pool(pool_name: &str, mut shape: Shape, hosts: usize) -> Self {
        shape
            .capabilities
            .entry(ARCH_KEY.to_owned())
            .or_insert_with(|| ARCH.to_owned());
        Self {
            pool: pool_name.to_owned(),
            class: POOL_CLASS,
            shape,
            hosts,
            hello_burst: hosts,
            interval: Duration::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS),
            grace: Duration::from_millis(DEFAULT_GRACE_MS),
            lost_retention: Duration::from_millis(DEFAULT_LOST_RETENTION_MS),
            resume: false,
            elastic: None,
            store: Store::default(),
        }
    }

    /// A server whose one pool is the elastic pool `pool`, of class [`WHOLE_CLASS`]: its
    /// machines are launched by the server's scaler on the pool's cloud, which the server
    /// node makes and owns ([`crate::Cloud`]), and each is a host of `shape`. The node
    /// reads the server's configuration from a file ([`crate::elastic`]), not from the
    /// command line; the file also turns the job service on and places as `js-sched`. A host of an elastic pool beats at the registry's defaults (5 s, with a
    /// grace of 15 s), which an elastic pool does not set: [`ServerSpec::heartbeat`] and
    /// the pool file's keys do not apply to it. The client asks for `shape`'s capabilities,
    /// and the pool's floor has none, so a scenario gives a shape without any. The pool's
    /// floor names no architecture either: each machine is placed by the `arch` its agent
    /// reports at hello, which the shape is given.
    #[must_use]
    pub fn elastic_pool(pool: ElasticPool, shape: Shape) -> Self {
        let name = pool.name().to_owned();
        Self {
            elastic: Some(pool),
            ..Self::whole_node_pool(&name, shape, 0)
        }
    }

    /// The elastic pool of a server of [`ServerSpec::elastic_pool`].
    #[must_use]
    pub fn elastic(&self) -> Option<&ElasticPool> {
        self.elastic.as_ref()
    }

    /// A server whose one pool, `pool_name`, of class [`WHOLE_CLASS`], books `shape` for
    /// each of `hosts` hosts that register with [`JOIN_TOKEN`]: a job that asks for a
    /// whole host is given one of its hosts whole.
    #[must_use]
    pub fn whole_node_pool(pool_name: &str, shape: Shape, hosts: usize) -> Self {
        Self {
            class: WHOLE_CLASS,
            ..Self::registered_pool(pool_name, shape, hosts)
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

    /// This server, keeping its state in a store of the kind `store` (`--store`): a SQLite
    /// file in its directory ([`Store::Sqlite`], the default), or a Postgres database of its
    /// own for each server node ([`Store::Postgres`], feature `postgres`).
    #[must_use]
    pub fn store(mut self, store: Store) -> Self {
        self.store = store;
        self
    }

    /// Which store the server keeps its state in.
    #[must_use]
    pub fn kept_in(&self) -> Store {
        self.store
    }

    /// This server, allowing the join token `burst` hellos before it holds the token to
    /// one a second.
    #[must_use]
    pub fn hello_burst(mut self, burst: usize) -> Self {
        self.hello_burst = burst;
        self
    }

    /// This server, booting over the lease file a server before it left in the same
    /// directory (`--boot resume`): how [`super::ServerNode::restart`] starts it again.
    #[must_use]
    pub(crate) fn resuming(mut self) -> Self {
        self.resume = true;
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
            quoted(self.class),
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

    /// Writes the pool file, the two token files and the store file into `dir` and returns
    /// the server's command line: the lease file in `dir`, a first boot (or a resumed one,
    /// for a server started again), registration into the pool with [`JOIN_TOKEN`], the
    /// hello burst, the placement, the job service's address, the store
    /// (`--store`, a SQLite file in `dir` with `--allow-sqlite-store`), and usage off.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, naming the flag of the file; and for a spec of
    /// [`Store::Postgres`], whose database the server node makes.
    pub fn args(&self, dir: &Path) -> Result<Vec<OsString>, StartupError> {
        self.args_over(dir, &self.store_in(dir)?)
    }

    /// [`ServerSpec::args`] over the store `at`.
    pub(crate) fn args_over(
        &self,
        dir: &Path,
        at: &StoreAt,
    ) -> Result<Vec<OsString>, StartupError> {
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
        let store = write_store(dir, at, "--store")?;
        let flags: Vec<(&str, OsString)> = vec![
            ("--state-file", dir.join("leases.toml").into_os_string()),
            ("--boot", boot(self.resume).into()),
            ("--token-file", token),
            ("--agent-token-file", joins),
            ("--agent-pool-config", pool),
            ("--agent-hello-burst", self.hello_burst.to_string().into()),
            ("--placement", PLACEMENT.into()),
            ("--store", store),
            ("--job-listen", JOB_LISTEN.into()),
        ];
        let pairs = flags
            .into_iter()
            .flat_map(|(flag, value)| [flag.into(), value]);
        let head = std::iter::once(OsString::from("js-lease-server"));
        let gate = at
            .is_sqlite()
            .then(|| OsString::from("--allow-sqlite-store"));
        let tail = std::iter::once(OsString::from("--usage-off"));
        Ok(head.chain(pairs).chain(gate).chain(tail).collect())
    }

    /// The store a server of this spec whose files are in `dir` keeps its state in, when no
    /// server node made one: the SQLite file in `dir`.
    ///
    /// # Errors
    ///
    /// For [`Store::Postgres`]: its database is the server node's.
    fn store_in(&self, dir: &Path) -> Result<StoreAt, StartupError> {
        match self.store {
            Store::Sqlite => Ok(StoreAt::make(Store::Sqlite, dir)),
            #[cfg(feature = "postgres")]
            Store::Postgres => Err(StartupError::new(
                "--store",
                "a Postgres store's database is made by the server node that starts the server",
            )),
        }
    }

    /// The server's configuration, read from [`ServerSpec::args`] by its own parser, with
    /// the lease table's save timings read from `clock`, so nothing reads the system's
    /// monotonic clock.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, or the server refuses its command line; for a spec of
    /// [`ServerSpec::elastic_pool`], whose server the server node configures over the cloud
    /// it owns; and as [`ServerSpec::args`].
    pub fn config(&self, dir: &Path, clock: &ManualClock) -> Result<Config, StartupError> {
        if self.elastic.is_some() {
            let reason =
                "an elastic pool's server is configured by the server node, over its cloud";
            return Err(StartupError::new("elastic", reason));
        }
        self.config_over(dir, clock, &self.store_in(dir)?)
    }

    /// [`ServerSpec::config`] over the store `at`.
    pub(crate) fn config_over(
        &self,
        dir: &Path,
        clock: &ManualClock,
        at: &StoreAt,
    ) -> Result<Config, StartupError> {
        let args = self.args_over(dir, at)?;
        let cli = Cli::try_parse_from(args).map_err(StartupError::because("command line"))?;
        let config = Config::from_cli(cli)?;
        Ok(config.with_monotonic(Arc::new(Virtual(clock.clone()))))
    }
}

impl ServerSpec {
    /// The configuration of the server of `cloud`'s elastic pool, placing as
    /// `js-sched` and serving the job service, over the store `at`: the client token file, the store file and the
    /// configuration file written into `dir` ([`elastic::config::text`]), read by the
    /// server with the cell's `sim-cloud` kind over `cloud`, on `clock`, the lease table
    /// timing its saves on `clock`, and the scaler drawing its acquire ids from `entropy`.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, or the server refuses the file.
    pub(crate) fn elastic_config(
        &self,
        dir: &Path,
        clock: &ManualClock,
        cloud: &Cloud,
        entropy: Arc<dyn SessionSource>,
        at: &StoreAt,
    ) -> Result<Config, StartupError> {
        let token = write(dir, "token", CLIENT_TOKEN, "client_token_file")?;
        let store = write_store(dir, at, "server.store")?;
        let state = dir.join("leases.toml");
        let files = elastic::config::Files {
            state: &state,
            token: Path::new(&token),
            store: Path::new(&store),
            allow_sqlite: at.is_sqlite(),
        };
        let text = elastic::config::text(cloud, &files, PLACEMENT);
        let file = write(dir, "js.toml", &text, "--config")?;
        let shared = SharedClock::new(clock.clone());
        let config = elastic::config::read(Path::new(&file), cloud, shared)?;
        let config = config.with_monotonic(Arc::new(Virtual(clock.clone())));
        Ok(config.with_entropy(entropy))
    }
}

/// The `--boot` value: `resume` over the lease file a server before left, else
/// `initialise`.
fn boot(resume: bool) -> &'static str {
    if resume { "resume" } else { "initialise" }
}

/// `text` as a TOML basic string, which the JSON string grammar is a part of.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

/// Writes the store file of `at` (its one line) into `dir` as `flag` names it, and returns
/// its path.
fn write_store(dir: &Path, at: &StoreAt, flag: &'static str) -> Result<OsString, StartupError> {
    write(dir, STORE_LINE_FILE, &format!("{}\n", at.line()), flag)
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

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::Timestamp;

    /// Asserts that the lease table's monotonic clock reads the cell's clock in
    /// milliseconds, and moves when the cell's clock does.
    #[test]
    fn the_monotonic_clock_reads_the_cells_clock() {
        let clock = ManualClock::new(Timestamp::from_millis(5_250));
        let monotonic = Virtual(clock.clone());
        assert_eq!(monotonic.reading(), Duration::from_millis(5_250));
        clock.advance(Duration::from_secs(2));
        assert_eq!(monotonic.reading(), Duration::from_millis(7_250));
    }
}
