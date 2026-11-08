//! The flags, and the checks that decide whether the server may start.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use js_alert::AlertSink;
use js_jobs::Retention;
use js_lease::{Boot, Monotonic, Settings, SystemClock, SystemMonotonic};

use js_registry::{OsSessions, SessionSource};

use crate::adapt::SharedClock;
use crate::agent_pool;
use crate::agent_tokens::AgentTokens;
use crate::alerts::{DEFAULT_ALERT_REPEAT, alert_sink};
use crate::capacity::{CapacitySource, StaticPoolSource};
use crate::error::StartupError;
use crate::file::{self, ElasticPlan, Load, Loaded, LoadedHosts, Registering, UsageKeys};
use crate::flags;
use crate::job_expiry::JobLimits;
use crate::job_store::JobSources;
use crate::kinds::{Kinds, OwnerOnlyFiles, SecretFiles};
use crate::pool::LaunchInputs;
use crate::rate::HelloLimit;
use crate::store_health::DEFAULT_UNREACHABLE_AFTER;
use crate::usage::{KeySource, UsageSink};
use crate::usage_setup::{UsageSetup, usage_setup};

mod agent_setup;
mod cpu_slack;
mod feed_retention;
mod job_wall;
mod listen;
mod placement;
mod private;
mod resolve;
mod retention;
mod store;
mod tokens;
pub(crate) use agent_setup::AgentSetup;
use agent_setup::with_pool_settings;
pub use cpu_slack::{CpuSlack, CpuSlackError};
pub use feed_retention::FeedRetention;
pub(crate) use feed_retention::{
    DEFAULT_JOB_FEED_RETENTION, JOB_FEED_RETENTION_MUST, parse_job_feed_retention,
};
pub(crate) use job_wall::{MAX_JOB_WALL_MUST, parse_max_job_wall};
pub(crate) use listen::{JobListen, MetricsListen, clear_text, not_private};
pub use placement::Placement;
use placement::check_pool;
pub(crate) use placement::{checked, placer, split};
pub(crate) use private::{read_private, read_private_reason};
pub(crate) use retention::{
    DEFAULT_JOB_ALLOCATION_RETENTION, DEFAULT_JOB_RETENTION, JOB_RETENTION_MUST,
    parse_job_retention,
};
pub(crate) use store::{StoreNames, StoreSetting, hook_store_target};
pub use tokens::Token;
pub(crate) use tokens::{Clients, Entry, LiveClients, Role, Taken, parse_token, token_at};

/// The command line. Configuration is flags, or one file (`--config`); secrets come from
/// files.
#[derive(Debug, Parser)]
#[command(
    name = "js-lease-server",
    version,
    about = "Serves a lease table over an authenticated HTTP JSON API"
)]
pub struct Cli {
    /// The configuration file: a `[server]` table and `[[pool]]` tables (see the crate
    /// docs). It replaces every flag below: with `--config`, any other flag is refused
    /// naming the first one given and the key of the file that takes its place.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// The flags the command line gave besides `--config`, in command-line order, with
    /// their dashes. [`Cli::try_parse_from`] fills it; a flag left at its default is not
    /// in it. `None` for a `Cli` made any other way (`Parser::parse`, `parse_from`), which
    /// [`Config::from_cli`] refuses: it cannot tell which flags were given.
    #[arg(skip)]
    pub(crate) given: Option<Vec<String>>,
    /// The address to listen on. Anything but a loopback address needs --tls-cert and
    /// --tls-key: a bearer token never crosses a network in clear.
    #[arg(long, default_value = "127.0.0.1:7300")]
    pub listen: SocketAddr,
    /// Serves the job service (gRPC and its HTTP/JSON gateway, one port, the bearer token)
    /// here, and opens the job store, which per-job heartbeats then update. Off unless given.
    /// Off the loopback it needs --tls-cert and --tls-key. Needs --agent-pool-config.
    #[arg(long)]
    pub job_listen: Option<SocketAddr>,
    /// Serves the operator's metrics (`GET /metrics`, the Prometheus text format, no token)
    /// here, to private peers only. Off unless given; an address that is not a private one is
    /// refused.
    #[arg(long)]
    pub metrics_listen: Option<SocketAddr>,
    /// The lease file.
    #[arg(long, required_unless_present = "config")]
    pub state_file: Option<PathBuf>,
    /// `initialise` creates the lease file when it is missing (the first boot);
    /// `resume` requires it. There is no default, so a lost state file is never
    /// silently started empty.
    #[arg(long, value_enum, required_unless_present = "config")]
    pub boot: Option<BootMode>,
    /// The TOML file listing the hosts to lease. Optional when agents register hosts
    /// (`--agent-pool-config`); one of the two sources is required.
    #[arg(long)]
    pub pool_config: Option<PathBuf>,
    /// A file holding the bearer token. It must be owner-only (mode 0600 or 0400). On the
    /// job service it is the token of the client named `default`.
    #[arg(long, required_unless_present = "config")]
    pub token_file: Option<PathBuf>,
    /// One more client of the job service, as `NAME=PATH`: its name (1 to 40 letters,
    /// digits, `.`, `_` and `-`, not `default`) and a file holding its own bearer token,
    /// checked as `--token-file` is. Repeatable. Needs `--job-listen`.
    #[arg(long, value_name = "NAME=PATH")]
    pub client_token: Vec<String>,
    /// The pool's shared join tokens: a file holding the tokens agents present, one per
    /// line (several while one is being replaced), each at least 32 bytes and the file
    /// owner-only like `--token-file`. Read again at every tick. Needs
    /// `--agent-pool-config`. Without it the pool admits only hosts whose one-time join
    /// token verifies (`POST /v1/agent/join-tokens`).
    #[arg(long)]
    pub agent_token_file: Option<PathBuf>,
    /// Turns agent registration on: the TOML file describing the pool that registering
    /// hosts join (its name, class, machine limit, shape, capabilities and timing).
    #[arg(long)]
    pub agent_pool_config: Option<PathBuf>,
    /// The hellos one join token may say in a burst before it is held to one per
    /// `--agent-hello-interval`. Needs `--agent-pool-config`. The scaler's wait to hear from
    /// the hosts after a restart assumes the default burst and interval; a smaller burst or
    /// a longer interval is bounded by its ten minute quiet period only.
    #[arg(
        long,
        default_value_t = js_agent_wire::DEFAULT_HELLO_BURST,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub agent_hello_burst: u32,
    /// How often a join token that spent its burst is allowed one more hello. Needs
    /// `--agent-pool-config`. See `--agent-hello-burst` for what the scaler assumes of it.
    #[arg(
        long,
        default_value = "1s",
        value_parser = parse_duration
    )]
    pub agent_hello_interval: Duration,
    /// Refuses the first hello of a one-time join token that does not say the agent keeps
    /// a host credential, so every launched host's token is dead after its first hello.
    /// Turn it on once every agent is new: an agent that predates the host credential
    /// cannot say so and is refused with `join-token-credential-required`. Off by default;
    /// pool join tokens are not affected. Needs `--agent-pool-config`.
    #[arg(long)]
    pub require_host_credential: bool,
    /// Deprecated, and does nothing: `js-sched` places every lease on a host that
    /// registered itself, choosing among the hosts with room. `js-sched` is accepted with a
    /// warning; `registry`, the removed first-free placement, is refused at start. Needs
    /// `--agent-pool-config`.
    #[arg(long, value_enum)]
    pub placement: Option<Placement>,
    /// How far above a host's cores the placement engine may charge it, as a multiplier
    /// of the cores from 1.0 to 2.0 with at most three decimals, such as `1.1`. Without
    /// the flag the engine's own default is used, which is 1.1.
    #[arg(long, value_parser = CpuSlack::parse)]
    pub cpu_slack: Option<CpuSlack>,
    /// Feeds each lease's estimated end to the drain cost of a whole-node request: the
    /// lease's start plus the largest P90 wall time, among the profiles the usage file has
    /// learned, of the runs queued on it with a profile name. A lease with no such run is
    /// costed at its max-wall bound, as every lease is without the flag. A lease put back
    /// after a restart keeps the start and the bound the lease file records. Off by
    /// default. Needs `--agent-pool-config`; not allowed with `--usage-off`, which leaves
    /// nothing to estimate from.
    #[arg(long, conflicts_with = "usage_off")]
    pub estimate_feed: bool,
    /// How often the table is ticked (expiries, retries, pending starts).
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    pub tick: Duration,
    /// The PEM certificate chain to serve over TLS. SIGHUP reads it and the key again.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    /// The PEM private key of the certificate: owner-only (0600 or 0400); a symbolic link is
    /// followed and the file it leads to is checked. SIGHUP reads it again.
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
    /// The most connections held open at once, handshaking, idle or busy. At the cap
    /// the server stops accepting until one ends.
    #[arg(long, default_value = "256", value_parser = clap::value_parser!(u16).range(1..))]
    pub max_connections: u16,
    /// How long a TLS client has to finish its handshake.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub handshake_timeout: Duration,
    /// How long a client has to send the head of a request, from the connection's
    /// start and again after each response on it. This is also how long a keep-alive
    /// connection may sit idle between requests.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub header_timeout: Duration,
    /// How long a read from a client may make no progress before the connection is
    /// closed: a client that sends a request head and stops cannot hold a connection
    /// through the body. Keep it above `--header-timeout`, which is what closes an idle
    /// connection.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub read_timeout: Duration,
    /// How long a write to a client may make no progress before the connection is
    /// closed: a client that does not read its answers cannot hold a connection.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub write_timeout: Duration,
    /// The most requests served on one connection. The last answer says
    /// `Connection: close`; a client opens another connection for the next request.
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_requests: u32,
    /// How long a stop waits for the requests in flight before it cuts what is left.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub drain_timeout: Duration,
    /// The file the per-minute usage of the nodes and the learned cost curves are kept
    /// in. Beside `--state-file`, as `usage.db`, unless given. Only agent registration
    /// reports usage.
    #[arg(long, conflicts_with = "usage_off")]
    pub usage_db: Option<PathBuf>,
    /// Turns the recording of usage, and its read routes, off. For an emergency: usage
    /// is on by default and the per-minute series is meant to be always collected.
    #[arg(long)]
    pub usage_off: bool,
    /// The most bytes the learned cost curves may take in the usage file; the keys used
    /// longest ago are dropped to stay under it. At least 1.
    #[arg(
        long,
        default_value = "268435456",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub profile_store_bytes: u64,
    /// How many days the record of each ended allocation (a lease's booking today) is kept
    /// in the usage file; older records are removed by the daily prune. At least 1.
    #[arg(
        long,
        default_value = "90",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub history_retention_days: u64,
    /// How long an ended job's record and result are kept after the job ends: a whole
    /// count of days (`7d`) or a duration such as `36h` or `90m`, at least one minute. Read
    /// once at start; it is the window the job store applies when a job ends, so a changed
    /// window affects only the jobs that end after the restart.
    #[arg(long, default_value = DEFAULT_JOB_RETENTION, value_parser = parse_job_retention)]
    pub job_retention: Retention,
    /// How many of the newest changes the job store's change feed keeps: a whole number,
    /// at least 1; 100000 unless given. The tick trims the feed to them (counted in feed
    /// changes, never by time), so a reader that has fallen further behind is told to start
    /// again from the tables. Read once at start.
    #[arg(long, default_value = DEFAULT_JOB_FEED_RETENTION, value_parser = parse_job_feed_retention)]
    pub job_feed_retention: FeedRetention,
    /// How long a freed allocation's row is kept after its free: a whole count of days
    /// (`14d`) or a duration such as `36h` or `90m`, at least one minute; fourteen days unless
    /// given. The tick deletes a freed row once the window has passed, a bounded batch a tick;
    /// the usage history of the allocation is kept. Read once at start.
    #[arg(long, default_value = DEFAULT_JOB_ALLOCATION_RETENTION, value_parser = parse_job_retention)]
    pub job_allocation_retention: Retention,
    /// The most a submitted job's wall limit may be: a whole count of days (`30d`) or a
    /// duration such as `12h` or `90m`, at least one second; 30 days unless given. A job
    /// asking for more is refused `wall-time-above-ceiling`. Read once at start.
    #[arg(long, value_parser = parse_max_job_wall)]
    pub max_job_wall: Option<Duration>,
    /// A file holding the store's connection, one line: `postgres://...`,
    /// `postgresql://...`, or `sqlite:<path>` (needs --allow-sqlite-store). Owner-only, as
    /// --token-file is. The server takes the store's writer lock before anything else.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Accepts a `sqlite:` store: SQLite is for tests and single-node development. With a
    /// Postgres store it has no effect. Needs --store.
    #[arg(long)]
    pub allow_sqlite_store: bool,
    /// How long the store paths of the tick (the lease file's write, the job pass) may fail
    /// tick after tick before `/healthz` answers 503 `store unreachable` and the alert
    /// `store:unreachable` holds. The first tick that succeeds clears both.
    #[arg(long, default_value = DEFAULT_UNREACHABLE_AFTER, value_parser = parse_duration)]
    pub store_unreachable_after: Duration,
}

/// What a client may hold open (see the module doc of `listener`). The flags set
/// these; the defaults are what the flags default to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most connections open at once.
    pub max_connections: u16,
    /// How long a TLS handshake may take.
    pub handshake_timeout: Duration,
    /// How long the head of a request may take to arrive; also the idle time of a
    /// keep-alive connection between two requests.
    pub header_timeout: Duration,
    /// How long a read from a client may make no progress.
    pub read_timeout: Duration,
    /// How long a write to a client may make no progress.
    pub write_timeout: Duration,
    /// The most requests one connection serves.
    pub max_requests: u32,
    /// How long a stop waits for requests in flight.
    pub drain_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            handshake_timeout: Duration::from_secs(10),
            header_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(10),
            max_requests: 100,
            drain_timeout: Duration::from_secs(10),
        }
    }
}

impl From<&Cli> for Limits {
    fn from(cli: &Cli) -> Self {
        Self {
            max_connections: cli.max_connections,
            handshake_timeout: cli.handshake_timeout,
            header_timeout: cli.header_timeout,
            read_timeout: cli.read_timeout,
            write_timeout: cli.write_timeout,
            max_requests: cli.max_requests,
            drain_timeout: cli.drain_timeout,
        }
    }
}

/// What a missing lease file means (`--boot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BootMode {
    /// A missing file starts an empty table.
    Initialise,
    /// A missing file is an error.
    Resume,
}

impl From<BootMode> for Boot {
    fn from(mode: BootMode) -> Self {
        match mode {
            BootMode::Initialise => Boot::Initialise,
            BootMode::Resume => Boot::Resume,
        }
    }
}

/// A duration: a whole count with an optional unit, `90`, `90s`, `250ms`, `15m` or `2h`.
/// A bare count is seconds. Zero is refused.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (count, unit) = text.split_at(digits);
    let millis_per_unit = ["", "ms", "s", "m", "h"]
        .iter()
        .position(|known| *known == unit)
        .map(|at| [1000u64, 1, 1000, 60_000, 3_600_000][at]);
    let millis = count
        .parse::<u64>()
        .ok()
        .zip(millis_per_unit)
        .and_then(|(count, factor)| count.checked_mul(factor));
    match millis {
        Some(millis) if millis > 0 => Ok(Duration::from_millis(millis)),
        _ => Err(format!(
            "{text:?} is not a positive duration such as 600, 600s, 250ms, 10m or 1h"
        )),
    }
}

/// Whether `ip` is an address only a private network reaches: the loopback, the private
/// ranges (10/8, 172.16/12, 192.168/16, and fc00::/7), link-local addresses, the shared
/// range a tailnet uses (100.64/10; its IPv6 range is in fc00::/7), and, because a
/// wildcard bind is normal in a VPC or a container and the server cannot tell what is
/// behind it, the wildcard. Anything else is a public address.
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let shared = v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64;
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || shared
        }
        IpAddr::V6(v6) => {
            let local = (v6.segments()[0] & 0xfe00) == 0xfc00;
            let link = (v6.segments()[0] & 0xffc0) == 0xfe80;
            let mapped = v6
                .to_ipv4_mapped()
                .is_some_and(|v4| is_private_address(v4.into()));
            v6.is_loopback() || v6.is_unspecified() || local || link || mapped
        }
    }
}

/// Whether the status routes are served on `listen`; when not, says so in `warnings`.
fn status_policy(flag: &str, listen: SocketAddr, warnings: &mut Vec<String>) -> bool {
    let private = is_private_address(listen.ip());
    if !private {
        warnings.push(format!(
            "server: warning: /v1/pools and /status are not served: {flag} {listen} is not a private address"
        ));
    }
    private
}

/// The certificate and key files of `--tls-cert` and `--tls-key`.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    /// The PEM certificate chain.
    pub cert: PathBuf,
    /// The PEM private key.
    pub key: PathBuf,
}

/// A command line that passed every check that needs no running server: the flags make
/// sense together, and the token was read from an owner-only file.
pub struct Config {
    pub(crate) listen: SocketAddr,
    /// What names the listen address in a refusal: `--listen` or `server.listen`.
    pub(crate) listen_flag: &'static str,
    /// A listener the caller bound, served in place of binding `listen`.
    pub(crate) bound: Option<std::net::TcpListener>,
    pub(crate) agent_listen: Option<SocketAddr>,
    /// Where the job service is served (`--job-listen`, `server.job_listen`); off when `None`.
    pub(crate) job_listen: Option<JobListen>,
    /// Where the metrics are served (`--metrics-listen`, `server.metrics_listen`); off when `None`.
    pub(crate) metrics_listen: Option<MetricsListen>,
    pub(crate) advertise_url: Option<String>,
    pub(crate) warnings: Vec<String>,
    /// Whether `/v1/pools` and `/status` are served: the listen address is a private one.
    pub(crate) status: bool,
    pub(crate) launch_inputs: LaunchInputs,
    pub(crate) elastic: Option<ElasticPlan>,
    pub(crate) state_file: PathBuf,
    pub(crate) boot: Boot,
    pub(crate) settings: Settings,
    pub(crate) tick: Duration,
    pub(crate) token: Token,
    /// The clients of the job service, the token of `--token-file` among them, behind the
    /// handle the bearer layer reads and the tick reloads.
    pub(crate) clients: LiveClients,
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) limits: Limits,
    pub(crate) hosts: Hosts,
    pub(crate) usage: UsageSetup,
    pub(crate) monotonic: Arc<dyn Monotonic>,
    /// Where the scaler draws the random part of each acquire id, and so of each machine's
    /// name: the operating system's ([`OsSessions`]) unless [`Config::with_entropy`] gives
    /// another.
    pub(crate) entropy: Arc<dyn SessionSource>,
    /// Where the scaler's alerts and the store's `store:unreachable` go besides the log; from
    /// `server.alert_webhook_url`, or [`Config::with_alert_sink`].
    pub(crate) alerts: Option<Arc<dyn AlertSink>>,
    /// How often an alert that keeps holding is sent to the sink again.
    pub(crate) alert_repeat: Duration,
    /// How long an ended job is kept (`--job-retention`, `server.job_retention`).
    pub(crate) job_retention: Retention,
    /// How many of the newest feed changes the job store keeps (`--job-feed-retention`,
    /// `server.job_feed_retention`).
    pub(crate) job_feed_retention: FeedRetention,
    /// How long a freed allocation's row is kept (`--job-allocation-retention`,
    /// `server.job_allocation_retention`).
    pub(crate) job_allocation_retention: Retention,
    /// The most a submitted job's wall limit may be (`--max-job-wall`, `server.max_job_wall`).
    pub(crate) max_job_wall: Duration,
    pub(crate) logs: file::LogsConfig,
    /// The store the server's state is kept in (`--store`, `server.store`); none unless set.
    pub(crate) store: Option<StoreSetting>,
    /// `--store-unreachable-after`, `server.store_unreachable_after`.
    pub(crate) store_unreachable_after: Duration,
}

/// Where the hosts come from: the operator's pool file alone, or that file (when given)
/// beside the hosts agents register.
pub(crate) enum Hosts {
    /// `--pool-config` alone: the server builds exactly what it always has.
    Static(Box<dyn CapacitySource>),
    /// Agent registration is on.
    Layered {
        /// The static pool, when `--pool-config` was given; it is tried first.
        static_pool: Option<Box<dyn CapacitySource>>,
        /// The registering side.
        agents: Box<AgentSetup>,
    },
}

impl Config {
    /// Checks `cli`: a listen address that is not loopback has
    /// TLS flags, the token files are owner-only and hold tokens, the pool files are
    /// valid, and at least one source of hosts is configured.
    pub fn from_cli(cli: Cli) -> Result<Self, StartupError> {
        let given = flags::recorded(&cli)?;
        if let Some(path) = &cli.config {
            if let Some(flag) = given.first() {
                return Err(flags::beside_config(flag));
            }
            let clock = SharedClock::new(SystemClock);
            return Self::from_config_file(path, &Kinds::standard(), &OwnerOnlyFiles, clock);
        }
        flags::need_agent_pool(&cli)?;
        // The deprecated `--placement` first: a removed `registry` is refused before the
        // pool is read, naming the flag to drop.
        let mut warnings = Vec::from_iter(Placement::given(cli.placement, "--placement")?);
        let settings = Settings::new();
        let tls = listen::tls_files(&cli)?;
        let token_file = required(cli.token_file.as_ref(), "--token-file")?;
        let token = read_token(token_file)?;
        let clients = LiveClients::new(tokens::from_flags(&token, &cli)?);
        let limits = Limits::from(&cli);
        let hosts = hosts(&cli, &clients)?;
        let settings = with_pool_settings(settings, &hosts)?;
        let state_file = required(cli.state_file.clone(), "--state-file")?;
        let keys = UsageKeys {
            db: cli.usage_db.clone(),
            off: cli.usage_off,
            profile_store_bytes: cli.profile_store_bytes,
            history_retention_days: cli.history_retention_days,
        };
        let usage = usage_setup(keys, &state_file);
        let store = store::from_flags(&cli)?;
        let status = status_policy("--listen", cli.listen, &mut warnings);
        Ok(Self {
            listen: cli.listen,
            listen_flag: "--listen",
            bound: None,
            agent_listen: None,
            job_listen: listen::job_listen(&cli),
            metrics_listen: listen::metrics_listen(&cli)?,
            advertise_url: None,
            warnings,
            status,
            launch_inputs: LaunchInputs::default(),
            elastic: None,
            state_file,
            boot: required(cli.boot, "--boot")?.into(),
            settings,
            tick: cli.tick,
            token,
            clients,
            tls,
            limits,
            hosts,
            usage,
            monotonic: Arc::new(SystemMonotonic::default()),
            entropy: Arc::new(OsSessions),
            alerts: None,
            alert_repeat: DEFAULT_ALERT_REPEAT,
            job_retention: cli.job_retention,
            job_feed_retention: cli.job_feed_retention,
            job_allocation_retention: cli.job_allocation_retention,
            max_job_wall: job_wall::or_default(cli.max_job_wall),
            logs: file::LogsConfig::None,
            store,
            store_unreachable_after: cli.store_unreachable_after,
        })
    }

    /// Checks the configuration file at `path` (see the crate docs for its format) and
    /// builds the pools it describes with `kinds`, reading secret files through `files`.
    /// Nothing here makes a network call: what only the network can say is for the
    /// checks of a running server.
    ///
    /// # Errors
    ///
    /// [`StartupError`] naming the pool and the field at fault.
    pub fn from_config_file(
        path: &Path,
        kinds: &Kinds,
        files: &dyn SecretFiles,
        clock: SharedClock,
    ) -> Result<Self, StartupError> {
        let load = Load {
            kinds,
            files,
            clock,
        };
        let Loaded {
            server,
            client_token,
            clients,
            hosts,
            mut warnings,
            unrunnable,
            logs,
            ..
        } = file::load(path, &load)?;
        if let Some(refusal) = unrunnable {
            return Err(refusal);
        }
        let (hosts, elastic) = match hosts {
            LoadedHosts::Static(source) => (Hosts::Static(source), None),
            LoadedHosts::Registering {
                static_pool,
                registering,
            } => {
                let Registering {
                    plan,
                    pool,
                    tokens,
                    mint_provider,
                    job_limits,
                } = *registering;
                let placed = Placement::given(server.agents.placement(), "server.placement")?;
                warnings.extend(placed);
                let cpu_slack = server.agents.cpu_slack().unwrap_or_default();
                let agents = Box::new(AgentSetup {
                    tokens,
                    pool,
                    mint_provider,
                    hello_limit: server.agents.hello_limit(),
                    require_host_credential: server.agents.require_host_credential(),
                    estimate_feed: server.agents.estimate_feed(),
                    cpu_slack,
                    max_lease_wall: server.max_lease_wall,
                    job_limits,
                    jobs: server.agents.job_store(server.job_store_windows()),
                    job_sources: JobSources::default(),
                });
                (
                    Hosts::Layered {
                        static_pool,
                        agents,
                    },
                    plan,
                )
            }
        };
        let alerts = alert_sink(server.alerts.as_ref(), files)?;
        let store = store::from_file(server.store.as_deref(), server.allow_sqlite_store, files)?;
        let alert_repeat = server
            .alerts
            .as_ref()
            .map_or(DEFAULT_ALERT_REPEAT, |keys| keys.repeat_after);
        let state_file = server.state_file;
        let usage = usage_setup(server.usage, &state_file);
        let settings = with_pool_settings(server.settings, &hosts)?;
        let status = status_policy("server.listen", server.listen, &mut warnings);
        Ok(Self {
            listen: server.listen,
            listen_flag: "server.listen",
            bound: None,
            agent_listen: server.agent_listen,
            job_listen: server.agents.job_listen(),
            metrics_listen: server.metrics_listen,
            advertise_url: server.advertise_url,
            warnings,
            status,
            launch_inputs: LaunchInputs::default(),
            elastic,
            state_file,
            boot: server.boot.into(),
            settings,
            tick: server.tick,
            token: client_token,
            clients,
            tls: server.tls,
            limits: server.limits,
            hosts,
            usage,
            monotonic: Arc::new(SystemMonotonic::default()),
            entropy: Arc::new(OsSessions),
            alerts,
            alert_repeat,
            job_retention: server.job_retention,
            job_feed_retention: server.job_feed_retention,
            job_allocation_retention: server.job_allocation_retention,
            max_job_wall: job_wall::or_default(server.max_job_wall),
            logs,
            store,
            store_unreachable_after: server.store_unreachable_after,
        })
    }

    /// The same configuration with every alert of the scaler, and the store's
    /// `store:unreachable`, also sent to `sink`, in place of the one the file names.
    pub fn with_alert_sink(mut self, sink: Arc<dyn AlertSink>) -> Self {
        self.alerts = Some(sink);
        self
    }

    /// The limits on what a client may hold open.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The address the client, operator and status routes are served on.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// The address the agent routes alone are served on, when the file gives one.
    pub fn agent_listen(&self) -> Option<SocketAddr> {
        self.agent_listen
    }

    /// The address the job service is served on, when the flag or the file gives one.
    pub fn job_listen(&self) -> Option<SocketAddr> {
        self.job_listen.map(|job| job.addr)
    }

    /// The address the metrics are served on, when the flag or the file gives one.
    pub fn metrics_listen(&self) -> Option<SocketAddr> {
        self.metrics_listen.map(|metrics| metrics.addr)
    }

    /// The URL agents are told to dial, when the file gives one.
    pub fn advertise_url(&self) -> Option<&str> {
        self.advertise_url.as_deref()
    }

    /// What the server shows and logs as warnings once it runs, such as a listener bound
    /// to every interface.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Whether the server serves `GET /v1/pools` and `GET /status`. They take no token, so
    /// they are served only when the listen address is a private one
    /// ([`is_private_address`]); on any other the routes do not exist, and the server says
    /// so in [`warnings`](Self::warnings). Where they are served, a request is answered
    /// only when its peer is a private address too; any other peer gets the reply of an
    /// unknown route.
    pub fn serves_status(&self) -> bool {
        self.status
    }

    /// The same configuration with what a launch plan needs that the file does not carry:
    /// the agent binary's digest, the Tailscale build, or a stand-in network mode.
    pub fn with_launch_inputs(mut self, inputs: LaunchInputs) -> Self {
        self.launch_inputs = inputs;
        self
    }

    /// The same configuration serving on `listener`, which the caller has already bound,
    /// in place of binding `--listen`. The caller owns the socket: the server makes its
    /// copy non-blocking and closes only that copy when it stops, so a clone of the
    /// listener (`try_clone`) the caller kept still holds the port afterwards. The
    /// server sets the socket non-blocking (a flag of the open file description, which
    /// clones share), so a clone the caller kept is non-blocking too until it calls
    /// `set_nonblocking(false)` on it, which it should do only once the server has
    /// stopped. The caller must not accept on a clone while the server runs, since the
    /// copies share one queue of connections. `Running::addr` is the address of `listener`.
    pub fn with_listener(mut self, listener: std::net::TcpListener) -> Self {
        self.bound = Some(listener);
        self
    }

    /// The elastic pool of the file, when it has one.
    pub fn elastic_pool(&self) -> Option<&ElasticPlan> {
        self.elastic.as_ref()
    }

    /// The same configuration taking its static hosts from `capacity` instead of the
    /// file `--pool-config` names.
    pub fn with_capacity_source(mut self, capacity: Box<dyn CapacitySource>) -> Self {
        self.hosts = match self.hosts {
            Hosts::Static(_) => Hosts::Static(capacity),
            Hosts::Layered { agents, .. } => Hosts::Layered {
                static_pool: Some(capacity),
                agents,
            },
        };
        self
    }

    /// The same configuration with other connection limits than the flags gave.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The same configuration timing the lease table's steps on `clock`.
    pub fn with_monotonic(mut self, clock: Arc<dyn Monotonic>) -> Self {
        self.monotonic = clock;
        self
    }

    /// The same configuration drawing the random part of each acquire id the scaler makes
    /// from `entropy` instead of the operating system ([`OsSessions`], the default). A
    /// machine's name is derived from its acquire id, so two servers given sources that
    /// answer the same bytes in the same order name the same machines. The source is shared:
    /// a server started again with the same `Arc` goes on with the next bytes rather than
    /// repeating the first ones. Only the scaler reads it; the registry's session secrets
    /// and the join tokens still come from the operating system.
    pub fn with_entropy(mut self, entropy: Arc<dyn SessionSource>) -> Self {
        self.entropy = entropy;
        self
    }

    /// How often the production ticker ticks (`--tick`).
    pub fn tick_period(&self) -> Duration {
        self.tick
    }

    /// The usage file (`--usage-db`, or `usage.db` beside the lease file), or `None`
    /// when `--usage-off` turned usage off.
    pub fn usage_db(&self) -> Option<&Path> {
        self.usage.db.as_deref()
    }

    /// Whether each lease's estimated end is fed to the drain cost (`--estimate-feed`, or
    /// `server.estimate_feed`): off for a server with no agent registration.
    pub fn estimate_feed(&self) -> bool {
        match &self.hosts {
            Hosts::Layered { agents, .. } => agents.estimate_feed,
            Hosts::Static(_) => false,
        }
    }

    /// The CPU slack of the placement engine (`--cpu-slack`, or `server.cpu_slack`): the
    /// value given, or the engine's default, 1.1x, which is also the answer for a server
    /// with no agent registration.
    pub fn cpu_slack(&self) -> CpuSlack {
        match &self.hosts {
            Hosts::Layered { agents, .. } => agents.cpu_slack,
            Hosts::Static(_) => CpuSlack::default(),
        }
    }

    /// How long an ended job's record and result are kept after the job ends
    /// (`--job-retention`, or `server.job_retention`): seven days unless set. It is read
    /// once here and applied when a job ends.
    pub fn job_retention(&self) -> Retention {
        self.job_retention
    }

    /// How many of the newest changes the job store's change feed keeps
    /// (`--job-feed-retention`, or `server.job_feed_retention`): 100000 unless set.
    pub fn job_feed_retention(&self) -> FeedRetention {
        self.job_feed_retention
    }

    /// How long a freed allocation's row is kept after its free
    /// (`--job-allocation-retention`, or `server.job_allocation_retention`): fourteen days
    /// unless set.
    pub fn job_allocation_retention(&self) -> Retention {
        self.job_allocation_retention
    }

    /// The most bytes the learned cost curves may take (`--profile-store-bytes`).
    pub fn profile_store_bytes(&self) -> u64 {
        self.usage.profile_store_bytes
    }

    /// How many days the record of an ended allocation is kept
    /// (`--history-retention-days`).
    pub fn history_retention_days(&self) -> u64 {
        self.usage.history_retention_days
    }

    /// The same configuration measuring usage with `sink` instead of the sink that opens
    /// the usage file. Usage that `--usage-off` turned off stays off.
    pub fn with_usage_sink(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage.sink = Some(sink);
        self
    }

    /// The same configuration naming the key of a run with `keys`; without it no run has a
    /// key, and none is learned from.
    pub fn with_key_source(mut self, keys: Arc<dyn KeySource>) -> Self {
        self.usage.keys = Some(keys);
        self
    }
}

/// `value`, or the refusal of the flag that was left out: `clap` requires the flag unless
/// `--config` is given, so a missing one means a `Cli` built by hand.
fn required<T>(value: Option<T>, flag: &'static str) -> Result<T, StartupError> {
    value.ok_or_else(|| StartupError::new(flag, "required unless --config is given"))
}

/// The sources of hosts the flags name. At least one is required; the shared join token
/// file needs the agent pool file, which alone turns registration on.
fn hosts(cli: &Cli, clients: &LiveClients) -> Result<Hosts, StartupError> {
    let static_pool = cli
        .pool_config
        .as_ref()
        .map(|path| Box::new(StaticPoolSource::new(path)) as Box<dyn CapacitySource>);
    match (&cli.agent_token_file, &cli.agent_pool_config) {
        (None, None) => static_pool.map(Hosts::Static).ok_or_else(|| {
            StartupError::new(
                "--pool-config",
                "required unless agents register hosts (--agent-pool-config)",
            )
        }),
        (tokens, Some(pool)) => {
            let tokens = match tokens {
                Some(path) => AgentTokens::open(path, clients)?,
                None => AgentTokens::none(clients),
            };
            let pool = agent_pool::read(pool)?;
            check_pool(&pool, "--agent-pool-config")?;
            let cpu_slack = cli.cpu_slack.unwrap_or_default();
            Ok(Hosts::Layered {
                static_pool,
                agents: Box::new(AgentSetup {
                    tokens,
                    pool,
                    mint_provider: None,
                    hello_limit: HelloLimit {
                        burst: cli.agent_hello_burst,
                        interval: cli.agent_hello_interval,
                    },
                    require_host_credential: cli.require_host_credential,
                    estimate_feed: cli.estimate_feed,
                    cpu_slack,
                    max_lease_wall: None,
                    job_limits: JobLimits::default(),
                    jobs: listen::job_store(cli),
                    job_sources: JobSources::default(),
                }),
            })
        }
        (Some(_), None) => Err(StartupError::new(
            "--agent-pool-config",
            "required with --agent-token-file",
        )),
    }
}

/// The fewest bytes a bearer token may have after its trailing whitespace is dropped.
/// A token that anyone on the network can try to guess is only as strong as it is long;
/// 32 bytes of a random source is out of reach.
pub const MIN_TOKEN_BYTES: usize = 32;

/// Reads the token from the file `--token-file` names, see [`read_private`].
fn read_token(path: &Path) -> Result<Token, StartupError> {
    let bytes = read_private(path, "--token-file")?;
    parse_token(&bytes)
        .map_err(|reason| StartupError::new("--token-file", format!("{} {reason}", path.display())))
}

#[cfg(test)]
pub(crate) mod clients_rig;
#[cfg(test)]
mod tests;
