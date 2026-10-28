//! The flags, and the checks that decide whether the server may start.

use std::fs::OpenOptions;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use js_lease::{Boot, Clock, Settings};
use rustix::fs::OFlags;
use rustix::process::geteuid;
use subtle::ConstantTimeEq;

use js_registry::{Credential, PoolConfig, Registry, WholeNodeConfig};

use crate::adapt::SharedClock;
use crate::agent_pool;
use crate::agent_tokens::AgentTokens;
use crate::capacity::{CapacitySource, StaticPoolSource};
use crate::error::StartupError;
use crate::rate::HelloLimit;
use crate::usage::{HostLimits, KeySource, UsageSink};
use crate::usage_hub::Usage;
use crate::usage_js::{self, RegistryKeys, StoreSetup};

/// The command line. Configuration is flags only; secrets come from files.
#[derive(Debug, Parser)]
#[command(
    name = "js-lease-server",
    version,
    about = "Serves a lease table over an authenticated HTTP JSON API"
)]
pub struct Cli {
    /// The address to listen on. Anything but a loopback address needs --tls-cert and
    /// --tls-key: a bearer token never crosses a network in clear.
    #[arg(long, default_value = "127.0.0.1:7300")]
    pub listen: SocketAddr,
    /// The lease file.
    #[arg(long)]
    pub state_file: PathBuf,
    /// `initialise` creates the lease file when it is missing (the first boot);
    /// `resume` requires it. There is no default, so a lost state file is never
    /// silently started empty.
    #[arg(long, value_enum)]
    pub boot: BootMode,
    /// The TOML file listing the hosts to lease. Optional when agents register hosts
    /// (`--agent-token-file`); one of the two sources is required.
    #[arg(long)]
    pub pool_config: Option<PathBuf>,
    /// A file holding the bearer token. It must be owner-only (mode 0600 or 0400).
    #[arg(long)]
    pub token_file: PathBuf,
    /// Turns agent registration on: a file holding the join tokens agents present, one
    /// per line (several while one is being replaced), each at least 32 bytes and the
    /// file owner-only like `--token-file`. Read again at every tick. Needs
    /// `--agent-pool-config`.
    #[arg(long)]
    pub agent_token_file: Option<PathBuf>,
    /// The TOML file describing the pool that registering hosts join: its name, class,
    /// machine limit, shape, capabilities and timing. Needs `--agent-token-file`.
    #[arg(long)]
    pub agent_pool_config: Option<PathBuf>,
    /// The hellos one join token may say in a burst before it is held to one per
    /// `--agent-hello-interval`.
    #[arg(
        long,
        default_value = "60",
        value_parser = clap::value_parser!(u32).range(1..),
        requires = "agent_token_file"
    )]
    pub agent_hello_burst: u32,
    /// How often a join token that spent its burst is allowed one more hello.
    #[arg(
        long,
        default_value = "1s",
        value_parser = parse_duration,
        requires = "agent_token_file"
    )]
    pub agent_hello_interval: Duration,
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
    /// Turns the recording of usage off. For an emergency: usage
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
    /// The key source a caller supplied; `None` leaves the server to read the work keys
    /// of the runs it queued.
    pub(crate) keys: Option<Arc<dyn KeySource>>,
}

/// Milliseconds in a day.
const MILLIS_PER_DAY: u64 = 86_400_000;

impl UsageSetup {
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

/// What `--agent-token-file`, `--agent-pool-config` and the hello limit decided.
pub(crate) struct AgentSetup {
    pub(crate) tokens: AgentTokens,
    pub(crate) pool: PoolConfig,
    pub(crate) hello_limit: HelloLimit,
}

impl Config {
    /// Checks `cli`: the ttls are consistent, a listen address that is not loopback has
    /// TLS flags, the token files are owner-only and hold tokens, the pool files are
    /// valid, and at least one source of hosts is configured.
    pub fn from_cli(cli: Cli) -> Result<Self, StartupError> {
        let settings = Settings::new(cli.default_ttl, cli.max_ttl)
            .map_err(|err| StartupError::new("--default-ttl", err))?;
        let tls = tls_files(&cli)?;
        let token = read_token(&cli.token_file)?;
        let limits = Limits::from(&cli);
        let hosts = hosts(&cli, &token)?;
        let settings = with_pool_settings(settings, &hosts)?;
        let usage = UsageSetup {
            db: usage_db(&cli),
            profile_store_bytes: cli.profile_store_bytes,
            sink: None,
            keys: None,
        };
        Ok(Self {
            listen: cli.listen,
            state_file: cli.state_file,
            boot: cli.boot.into(),
            settings,
            tick: cli.tick,
            token,
            tls,
            limits,
            hosts,
            usage,
        })
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

    /// The same configuration naming the key of a run with `keys` instead of the work key
    /// the client gave when it queued the run (the default).
    pub fn with_key_source(mut self, keys: Arc<dyn KeySource>) -> Self {
        self.usage.keys = Some(keys);
        self
    }
}

/// The usage file the flags name: none for `--usage-off`, else `--usage-db` or `usage.db`
/// in the directory of the lease file.
fn usage_db(cli: &Cli) -> Option<PathBuf> {
    let beside = || cli.state_file.with_file_name("usage.db");
    (!cli.usage_off).then(|| cli.usage_db.clone().unwrap_or_else(beside))
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

/// The sources of hosts the flags name. At least one is required; the two agent flags
/// come together.
fn hosts(cli: &Cli, client: &Token) -> Result<Hosts, StartupError> {
    let static_pool = cli
        .pool_config
        .as_ref()
        .map(|path| Box::new(StaticPoolSource::new(path)) as Box<dyn CapacitySource>);
    match (&cli.agent_token_file, &cli.agent_pool_config) {
        (None, None) => static_pool.map(Hosts::Static).ok_or_else(|| {
            StartupError::new(
                "--pool-config",
                "required unless agents register hosts (--agent-token-file and --agent-pool-config)",
            )
        }),
        (Some(tokens), Some(pool)) => Ok(Hosts::Layered {
            static_pool,
            agents: Box::new(AgentSetup {
                tokens: AgentTokens::open(tokens, client)?,
                pool: agent_pool::read(pool)?,
                hello_limit: HelloLimit {
                    burst: cli.agent_hello_burst,
                    interval: cli.agent_hello_interval,
                },
            }),
        }),
        (Some(_), None) => Err(StartupError::new(
            "--agent-pool-config",
            "required with --agent-token-file",
        )),
        (None, Some(_)) => Err(StartupError::new(
            "--agent-token-file",
            "required with --agent-pool-config",
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
/// The file is opened without blocking, so a named pipe at the path is refused instead
/// of waited on; a symbolic link is followed (secret mounts are links) and the checks
/// are on what it leads to.
pub(crate) fn read_private(path: &Path, flag: &'static str) -> Result<Vec<u8>, StartupError> {
    let refuse = StartupError::because(flag);
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path);
    let mut file = opened.map_err(|err| refuse(format!("{}: {err}", path.display())))?;
    let meta = file.metadata().map_err(StartupError::because(flag))?;
    let mode = meta.permissions().mode() & 0o777;
    let euid = geteuid().as_raw();
    if !trusted_private_file(meta.is_file(), mode, meta.uid(), euid) {
        return Err(StartupError::new(
            flag,
            format!(
                "{} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode {mode:04o} owned by uid {}",
                path.display(),
                meta.uid()
            ),
        ));
    }
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    read.map_err(StartupError::because(flag))?;
    Ok(bytes)
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
