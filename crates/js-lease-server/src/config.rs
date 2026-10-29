//! The flags, and the checks that decide whether the server may start.

use std::fs::OpenOptions;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use js_lease::{Boot, Clock, Settings, SystemClock};
use rustix::fs::OFlags;
use rustix::process::geteuid;
use subtle::ConstantTimeEq;

use js_registry::{Credential, PoolConfig, Registry, WholeNodeConfig};
use js_types::sched::WorkKey;

use crate::adapt::SharedClock;
use crate::agent_pool;
use crate::agent_tokens::AgentTokens;
use crate::capacity::{CapacitySource, StaticPoolSource};
use crate::error::StartupError;
use crate::file::{self, ElasticPlan, Load, Loaded, LoadedHosts, Registering};
use crate::flags;
use crate::kinds::{Kinds, OwnerOnlyFiles, SecretFiles};
use crate::pool::LaunchInputs;
use crate::rate::HelloLimit;
use crate::usage::{HostLimits, KeySource, ProfileKey, UsageSink};
use crate::usage_hub::Usage;
use crate::usage_js::{self, NameMap, RegistryKeys, StoreSetup, no_names};

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
    /// A file holding the bearer token. It must be owner-only (mode 0600 or 0400).
    #[arg(long, required_unless_present = "config")]
    pub token_file: Option<PathBuf>,
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
    /// The ttl of a lease whose client names none, such as `600s`, `10m` or `1h`.
    #[arg(long, default_value = "600s", value_parser = parse_duration)]
    pub default_ttl: Duration,
    /// The longest ttl granted at one time.
    #[arg(long, default_value = "3600s", value_parser = parse_duration)]
    pub max_ttl: Duration,
    /// How often the table is ticked (expiries, retries, pending starts).
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    pub tick: Duration,
    /// The PEM certificate chain to serve over TLS.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    /// The PEM private key of the certificate.
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

/// A duration in whole seconds: `90`, `90s`, `15m` or `2h`. Zero is refused.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (count, unit) = text.split_at(digits);
    let factor = ["", "s", "m", "h"].iter().position(|known| *known == unit);
    let factor = factor.map(|at| [1u64, 1, 60, 3600][at]);
    let seconds = count
        .parse::<u64>()
        .ok()
        .zip(factor)
        .and_then(|(count, factor)| count.checked_mul(factor));
    match seconds {
        Some(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err(format!(
            "{text:?} is not a positive duration such as 600, 600s, 10m or 1h"
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

/// The bearer token. It is compared in constant time and never printed.
#[derive(Clone)]
pub struct Token(Vec<u8>);

impl Token {
    /// Whether `given` is the token. The comparison takes the same time for any two
    /// tokens of one length; a token of another length is refused at once (the length
    /// of a token is not a secret).
    pub(crate) fn matches(&self, given: &[u8]) -> bool {
        self.0.ct_eq(given).into()
    }

    /// Whether `other` is the same token, compared as [`Token::matches`] does.
    pub(crate) fn same_as(&self, other: &Token) -> bool {
        self.matches(&other.0)
    }

    /// The credential a host admitted with this token is bound to. A token is printable
    /// ASCII, so its text is exact.
    pub(crate) fn credential(&self) -> Credential {
        Credential::of_token(&String::from_utf8_lossy(&self.0))
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
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
    pub(crate) agent_listen: Option<SocketAddr>,
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
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) limits: Limits,
    pub(crate) hosts: Hosts,
    pub(crate) usage: UsageSetup,
}

/// What `--usage-db`, `--usage-off` and `--profile-store-bytes` decided, and the seams a
/// caller may fill: the sink that measures, and the source of the key of a run.
pub(crate) struct UsageSetup {
    /// The usage file; `None` when usage is off.
    pub(crate) db: Option<PathBuf>,
    pub(crate) profile_store_bytes: u64,
    /// The sink to use in place of the one that opens `db`.
    pub(crate) sink: Option<Arc<dyn UsageSink>>,
    /// The key source a caller supplied; `None` leaves the server to read the profile names
    /// clients gave the runs it queued.
    pub(crate) keys: Option<Arc<dyn KeySource>>,
    /// How admission finds the profile name of a work key; `None` leaves every key flat.
    pub(crate) names: Option<NameMap>,
}

/// The default of `--profile-store-bytes`.
pub(crate) const DEFAULT_PROFILE_STORE_BYTES: u64 = 268_435_456;

/// Milliseconds in a day.
const MILLIS_PER_DAY: u64 = 86_400_000;

impl UsageSetup {
    /// The usage side as `db` (`None` for usage off) and `profile_store_bytes` say, with
    /// no seam filled.
    fn new(db: Option<PathBuf>, profile_store_bytes: u64) -> Self {
        Self {
            db,
            profile_store_bytes,
            sink: None,
            keys: None,
            names: None,
        }
    }

    /// The usage side for hosts the pool books as `limits`, or `None` when usage is off.
    /// A usage file that will not open is a refused start naming `--usage-db`. A run's key
    /// is read from `registry` unless the caller supplied a key source.
    pub(crate) fn build(
        self,
        clock: &SharedClock,
        limits: HostLimits,
        registry: &Registry,
    ) -> Result<Option<Arc<Usage>>, StartupError> {
        let Some(db) = self.db else {
            return Ok(None);
        };
        let sink = match self.sink {
            Some(sink) => sink,
            None => {
                let setup = StoreSetup {
                    db,
                    profile_store_bytes: self.profile_store_bytes,
                    names: self.names.unwrap_or_else(no_names),
                };
                usage_js::open(&setup, clock.now().millis() / MILLIS_PER_DAY)?
            }
        };
        let keys = self
            .keys
            .unwrap_or_else(|| Arc::new(RegistryKeys(registry.clone())));
        let usage = Usage::new(sink, keys, clock.clone(), limits);
        Ok(Some(Arc::new(usage)))
    }
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

/// What `--agent-pool-config`, `--agent-token-file` (when given) and the hello limit
/// decided.
pub(crate) struct AgentSetup {
    pub(crate) tokens: AgentTokens,
    pub(crate) pool: PoolConfig,
    pub(crate) hello_limit: HelloLimit,
    /// `--require-host-credential`.
    pub(crate) require_host_credential: bool,
}

impl Config {
    /// Checks `cli`: the ttls are consistent, a listen address that is not loopback has
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
        let settings = Settings::new(cli.default_ttl, cli.max_ttl)
            .map_err(|err| StartupError::new("--default-ttl", err))?;
        let tls = tls_files(&cli)?;
        let token_file = required(cli.token_file.as_ref(), "--token-file")?;
        let token = read_token(token_file)?;
        let limits = Limits::from(&cli);
        let hosts = hosts(&cli, &token)?;
        let settings = with_pool_settings(settings, &hosts)?;
        let state_file = required(cli.state_file.clone(), "--state-file")?;
        let usage = usage_setup(
            cli.usage_off,
            cli.usage_db.clone(),
            cli.profile_store_bytes,
            &state_file,
        );
        let mut warnings = Vec::new();
        let status = status_policy("--listen", cli.listen, &mut warnings);
        Ok(Self {
            listen: cli.listen,
            listen_flag: "--listen",
            agent_listen: None,
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
            tls,
            limits,
            hosts,
            usage,
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
            hosts,
            mut warnings,
        } = file::load(path, &load)?;
        let (hosts, elastic) = match hosts {
            LoadedHosts::Static(source) => (Hosts::Static(source), None),
            LoadedHosts::Registering {
                static_pool,
                registering,
            } => {
                let Registering { plan, pool, tokens } = *registering;
                let agents = Box::new(AgentSetup {
                    tokens,
                    pool,
                    hello_limit: server.agents.hello_limit(),
                    require_host_credential: server.agents.require_host_credential(),
                });
                (
                    Hosts::Layered {
                        static_pool,
                        agents,
                    },
                    Some(plan),
                )
            }
        };
        let state_file = server.state_file;
        let keys = server.usage;
        let usage = usage_setup(keys.off, keys.db, keys.profile_store_bytes, &state_file);
        let settings = with_pool_settings(server.settings, &hosts)?;
        let status = status_policy("server.listen", server.listen, &mut warnings);
        Ok(Self {
            listen: server.listen,
            listen_flag: "server.listen",
            agent_listen: server.agent_listen,
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
            tls: server.tls,
            limits: server.limits,
            hosts,
            usage,
        })
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

    /// How often the production ticker ticks (`--tick`).
    pub fn tick_period(&self) -> Duration {
        self.tick
    }

    /// The usage file (`--usage-db`, or `usage.db` beside the lease file), or `None`
    /// when `--usage-off` turned usage off.
    pub fn usage_db(&self) -> Option<&Path> {
        self.usage.db.as_deref()
    }

    /// The most bytes the learned cost curves may take (`--profile-store-bytes`).
    pub fn profile_store_bytes(&self) -> u64 {
        self.usage.profile_store_bytes
    }

    /// The same configuration measuring usage with `sink` instead of the sink that opens
    /// the usage file. Usage that `--usage-off` turned off stays off.
    pub fn with_usage_sink(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage.sink = Some(sink);
        self
    }

    /// The same configuration naming the key of a run with `keys` instead of the profile
    /// name the client gave when it queued the run (the default).
    pub fn with_key_source(mut self, keys: Arc<dyn KeySource>) -> Self {
        self.usage.keys = Some(keys);
        self
    }

    /// The same configuration telling admission's curve reads (`usage_curves`) which
    /// profile a work key has. The server never derives a profile name from a work key:
    /// the name is the one the caller supplied for the run (see [`KeySource`]), so only
    /// the caller knows which work key has which. Without it every work key is charged
    /// flat.
    pub fn with_curve_names(
        mut self,
        names: impl Fn(&WorkKey) -> Option<ProfileKey> + Send + Sync + 'static,
    ) -> Self {
        self.usage.names = Some(Arc::new(names));
        self
    }
}

/// The usage file `--usage-db`, `--usage-off` or the file's keys name: none when usage is
/// off, else `db` or `usage.db` in the directory of the lease file.
fn usage_db(off: bool, db: Option<PathBuf>, state_file: &Path) -> Option<PathBuf> {
    let beside = || state_file.with_file_name("usage.db");
    (!off).then(|| db.unwrap_or_else(beside))
}

/// What the usage settings decide, for the lease file at `state_file`.
fn usage_setup(
    off: bool,
    db: Option<PathBuf>,
    profile_store_bytes: u64,
    state_file: &Path,
) -> UsageSetup {
    UsageSetup::new(usage_db(off, db, state_file), profile_store_bytes)
}

/// `settings` with what the registered pool's file decides about whole-node leases: the
/// time a lease waits, pending, for its host to be ready is the pool's `prepare_timeout`
/// (the lease table counts it; the registry counts the same figure per commit). A pool
/// without whole-node settings, or no registered pool, leaves `settings` as the flags
/// made them.
fn with_pool_settings(settings: Settings, hosts: &Hosts) -> Result<Settings, StartupError> {
    match hosts {
        Hosts::Layered { agents, .. } => with_whole_node(settings, agents.pool.whole_node()),
        Hosts::Static(_) => Ok(settings),
    }
}

fn with_whole_node(
    settings: Settings,
    whole: Option<&WholeNodeConfig>,
) -> Result<Settings, StartupError> {
    let Some(whole) = whole else {
        return Ok(settings);
    };
    settings
        .with_prepare_timeout(Duration::from_millis(whole.prepare_timeout_ms))
        .map_err(|err| StartupError::new("--agent-pool-config", err))
}

/// `value`, or the refusal of the flag that was left out: `clap` requires the flag unless
/// `--config` is given, so a missing one means a `Cli` built by hand.
fn required<T>(value: Option<T>, flag: &'static str) -> Result<T, StartupError> {
    value.ok_or_else(|| StartupError::new(flag, "required unless --config is given"))
}

/// The sources of hosts the flags name. At least one is required; the shared join token
/// file needs the agent pool file, which alone turns registration on.
fn hosts(cli: &Cli, client: &Token) -> Result<Hosts, StartupError> {
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
                Some(path) => AgentTokens::open(path, client)?,
                None => AgentTokens::none(),
            };
            Ok(Hosts::Layered {
                static_pool,
                agents: Box::new(AgentSetup {
                    tokens,
                    pool: agent_pool::read(pool)?,
                    hello_limit: HelloLimit {
                        burst: cli.agent_hello_burst,
                        interval: cli.agent_hello_interval,
                    },
                    require_host_credential: cli.require_host_credential,
                }),
            })
        }
        (Some(_), None) => Err(StartupError::new(
            "--agent-pool-config",
            "required with --agent-token-file",
        )),
    }
}

fn tls_files(cli: &Cli) -> Result<Option<TlsFiles>, StartupError> {
    let both = |cert: &Path, key: &Path| TlsFiles {
        cert: cert.to_path_buf(),
        key: key.to_path_buf(),
    };
    let files = match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert), Some(key)) => Some(both(cert, key)),
        (None, None) => None,
        (Some(_), None) => return Err(StartupError::new("--tls-key", "required with --tls-cert")),
        (None, Some(_)) => return Err(StartupError::new("--tls-cert", "required with --tls-key")),
    };
    let clear = files.is_none() & !cli.listen.ip().is_loopback();
    match clear {
        true => Err(StartupError::new(
            "--listen",
            format!(
                "{} is not a loopback address, so --tls-cert and --tls-key are required: a bearer token never crosses a network in clear",
                cli.listen
            ),
        )),
        false => Ok(files),
    }
}

/// Whether a secret file may be trusted: a regular file, owned by the user running the
/// server (`euid`), that nobody else can read or write (mode 0600 or 0400).
fn trusted_private_file(is_file: bool, mode: u32, owner: u32, euid: u32) -> bool {
    is_file & matches!(mode, 0o600 | 0o400) & (owner == euid)
}

/// The fewest bytes a bearer token may have after its trailing whitespace is dropped.
/// A token that anyone on the network can try to guess is only as strong as it is long;
/// 32 bytes of a random source is out of reach.
pub const MIN_TOKEN_BYTES: usize = 32;

/// The token a file's bytes hold: its content without trailing whitespace, which must
/// be one run of printable ASCII (an HTTP header value cannot carry anything else, so
/// any other token could never match a request) of at least [`MIN_TOKEN_BYTES`] bytes.
pub(crate) fn parse_token(bytes: &[u8]) -> Result<Token, String> {
    let token = bytes.trim_ascii_end();
    let printable = token.iter().all(u8::is_ascii_graphic);
    match (token.is_empty(), printable, token.len() < MIN_TOKEN_BYTES) {
        (true, _, _) => Err("is empty".to_string()),
        (false, false, _) => Err(
            "must hold one token of printable ASCII with no space or line break inside it"
                .to_string(),
        ),
        (false, true, true) => Err(format!(
            "holds a token of {} bytes; a token must have at least {MIN_TOKEN_BYTES} (use a random one)",
            token.len()
        )),
        (false, true, false) => Ok(Token(token.to_vec())),
    }
}

/// Reads the secret file at `path`, named by `flag`, after checking, on the open file,
/// that it is a regular file owned by the server's user that only that user can use.
/// See [`read_private_reason`].
pub(crate) fn read_private(path: &Path, flag: &'static str) -> Result<Vec<u8>, StartupError> {
    read_private_reason(path).map_err(StartupError::because(flag))
}

/// Reads the secret file at `path` after checking, on the open file, that it is a regular
/// file owned by the server's user that only that user can use, and says why it was not
/// read otherwise. The file is opened without blocking, so a named pipe at the path is
/// refused instead of waited on; a symbolic link is followed (secret mounts are links)
/// and the checks are on what it leads to. The reason names the path and never the
/// content.
pub(crate) fn read_private_reason(path: &Path) -> Result<Vec<u8>, String> {
    match open_private(path) {
        Ok(read) => read,
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// [`read_private_reason`] with the system's failures on the outside: a file that could
/// not be opened, inspected or read, and inside it the refusal a file earns by what it
/// is.
fn open_private(path: &Path) -> std::io::Result<Result<Vec<u8>, String>> {
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path)?;
    let meta = file.metadata()?;
    let mode = meta.permissions().mode() & 0o777;
    let euid = geteuid().as_raw();
    if !trusted_private_file(meta.is_file(), mode, meta.uid(), euid) {
        return Ok(Err(format!(
            "{} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode {mode:04o} owned by uid {}",
            path.display(),
            meta.uid()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Ok(bytes))
}

/// Reads the token from the file `--token-file` names, see [`read_private`].
fn read_token(path: &Path) -> Result<Token, StartupError> {
    let bytes = read_private(path, "--token-file")?;
    parse_token(&bytes)
        .map_err(|reason| StartupError::new("--token-file", format!("{} {reason}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a token compares equal only to itself and prints without its bytes.
    #[test]
    fn a_token_matches_itself_and_hides_itself() {
        let token = Token(b"s3cret".to_vec());
        assert!(token.matches(b"s3cret"));
        assert!(!token.matches(b"s3creT"));
        assert!(!token.matches(b"s3cret!"));
        assert!(!token.matches(b""));
        assert_eq!(format!("{token:?}"), "Token(..)");
    }

    /// Asserts that a token file is trusted only when it is a regular file, owner-only
    /// in mode, and owned by the user running the server: each condition alone refuses.
    #[test]
    fn a_token_file_must_be_the_servers_own_private_file() {
        assert!(trusted_private_file(true, 0o600, 1000, 1000));
        assert!(trusted_private_file(true, 0o400, 1000, 1000));
        assert!(!trusted_private_file(false, 0o600, 1000, 1000));
        assert!(!trusted_private_file(true, 0o640, 1000, 1000));
        assert!(!trusted_private_file(true, 0o600, 0, 1000));
        assert!(!trusted_private_file(true, 0o600, 1000, 0));
    }

    /// Asserts that a token is the content without trailing whitespace, and that empty
    /// content and any space, line break, control or non-ASCII byte inside the token
    /// is refused: no header value could ever equal such a token.
    #[test]
    fn a_token_is_one_run_of_printable_ascii() {
        let good = "abcdefghijklmnopqrstuvwxyz-0123456789";
        let trailing = format!("{good}\r\n");
        assert!(
            parse_token(trailing.as_bytes())
                .unwrap()
                .matches(good.as_bytes())
        );
        assert_eq!(parse_token(b" \r\n").err().as_deref(), Some("is empty"));
        for bad in [
            &b"two\nlines\n"[..],
            b"has space",
            b" leading",
            b"tab\tinside",
            b"nul\0byte",
            "caf\u{e9}".as_bytes(),
        ] {
            let reason = parse_token(bad).err().unwrap();
            assert!(reason.contains("printable ASCII"), "{bad:?}");
        }
    }

    /// Asserts that a token of 31 bytes is refused and one of 32 is accepted, counting
    /// the bytes without the trailing whitespace, and that the refusal says how long
    /// the token was and how long it must be.
    #[test]
    fn a_token_must_have_at_least_32_bytes() {
        let short = "x".repeat(MIN_TOKEN_BYTES - 1);
        let reason = parse_token(format!("{short}\n\n").as_bytes())
            .err()
            .unwrap();
        assert!(reason.contains("31 bytes"), "{reason}");
        assert!(reason.contains("at least 32"), "{reason}");
        let enough = "x".repeat(MIN_TOKEN_BYTES);
        assert!(parse_token(enough.as_bytes()).is_ok());
    }

    /// Asserts that the default limits are what the flags default to, so the two
    /// cannot drift apart.
    #[test]
    fn the_default_limits_are_the_flag_defaults() {
        let cli = Cli::try_parse_from([
            "js-lease-server",
            "--state-file=a",
            "--boot=resume",
            "--pool-config=b",
            "--token-file=c",
        ])
        .unwrap();
        assert_eq!(Limits::from(&cli), Limits::default());
    }

    /// Asserts how a pool's whole-node settings reach the lease table's settings: none
    /// leaves the flags' settings untouched (every figure), a pool's `prepare_timeout`
    /// replaces only the prepare timeout, and a zero one is a startup error naming the
    /// pool flag. It catches a pool key that is dropped on the way, and one that also
    /// disturbs the ttls.
    #[test]
    fn a_pools_prepare_timeout_replaces_only_the_prepare_timeout() {
        let secs = Duration::from_secs;
        let flags = Settings::new(secs(60), secs(3600)).unwrap();
        assert_eq!(with_whole_node(flags, None).unwrap(), flags);
        let mut whole = WholeNodeConfig::dedicated(2);
        whole.prepare_timeout_ms = 1_200_000;
        let set = with_whole_node(flags, Some(&whole)).unwrap();
        assert_eq!(set.prepare_timeout(), secs(1200));
        assert_eq!((set.default_ttl(), set.max_ttl()), (secs(60), secs(3600)));
        whole.prepare_timeout_ms = 0;
        let err = with_whole_node(flags, Some(&whole)).err().unwrap();
        assert!(err.to_string().contains("--agent-pool-config"), "{err}");
    }
}
