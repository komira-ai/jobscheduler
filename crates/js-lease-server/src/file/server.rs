//! The `[server]` table of the configuration file: its keys and the client token file.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_lease::Settings;

use super::{Load, Section};
use crate::config::{BootMode, Limits, TlsFiles, Token, parse_token};
use crate::error::StartupError;
use crate::rate::HelloLimit;

const SERVER_KEYS: &[&str] = &[
    "listen",
    "agent_listen",
    "advertise_url",
    "scheduler_id",
    "state_file",
    "boot",
    "client_token_file",
    "tls_cert",
    "tls_key",
    "tick",
    "default_ttl",
    "max_ttl",
    "max_connections",
    "handshake_timeout",
    "header_timeout",
    "read_timeout",
    "write_timeout",
    "max_requests",
    "drain_timeout",
    "agent_hello_burst",
    "agent_hello_interval",
    "require_host_credential",
];

/// What the `[server]` table says.
pub(crate) struct Server {
    pub(crate) listen: SocketAddr,
    pub(crate) agent_listen: Option<SocketAddr>,
    pub(crate) advertise_url: Option<String>,
    pub(crate) scheduler: SchedulerId,
    pub(crate) state_file: PathBuf,
    pub(crate) boot: BootMode,
    pub(crate) client_token_file: PathBuf,
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) tick: Duration,
    pub(crate) settings: Settings,
    pub(crate) limits: Limits,
    pub(crate) agents: AgentKeys,
}

/// The keys of `[server]` that belong to agent registration, as the file gave them. They
/// mean something only with a pool that registers agents.
pub(crate) struct AgentKeys {
    burst: Option<u32>,
    interval: Option<Duration>,
    require_host_credential: Option<bool>,
}

impl AgentKeys {
    /// The first key given, in the order of the reference.
    pub(crate) fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("agent_hello_burst", self.burst.is_some()),
            ("agent_hello_interval", self.interval.is_some()),
            (
                "require_host_credential",
                self.require_host_credential.is_some(),
            ),
        ];
        given.into_iter().find(|(_, set)| *set).map(|(key, _)| key)
    }

    /// The hello limit: the keys' values, or the flags' defaults.
    pub(crate) fn hello_limit(&self) -> HelloLimit {
        let default = HelloLimit::default();
        HelloLimit {
            burst: self.burst.unwrap_or(default.burst),
            interval: self.interval.unwrap_or(default.interval),
        }
    }

    /// Whether a one-time join token's first hello must say the agent keeps a host
    /// credential; off unless the file says so.
    pub(crate) fn require_host_credential(&self) -> bool {
        self.require_host_credential.unwrap_or(false)
    }
}

pub(super) fn read_server(table: toml::Table) -> Result<Server, StartupError> {
    let mut s = Section::open(table, "server.".to_owned());
    s.only(SERVER_KEYS)?;
    let listen = s.address("listen")?;
    let listen = listen.unwrap_or(SocketAddr::from(([127, 0, 0, 1], 7300)));
    let agent_listen = s.address("agent_listen")?;
    let advertise_url = s.string("advertise_url")?;
    let scheduler = s.string("scheduler_id")?.unwrap_or_else(|| "js".to_owned());
    let scheduler = match SchedulerId::new(&scheduler) {
        Ok(id) => id,
        Err(err) => return Err(s.fail("scheduler_id", &err.to_string())),
    };
    let state_file = s.required_path("state_file")?;
    let boot = s.required_string("boot")?;
    let boot = match boot.as_str() {
        "initialise" => BootMode::Initialise,
        "resume" => BootMode::Resume,
        _ => return Err(s.fail("boot", "must be initialise or resume")),
    };
    let client_token_file = s.required_path("client_token_file")?;
    let tls = match (s.path("tls_cert")?, s.path("tls_key")?) {
        (Some(cert), Some(key)) => Some(TlsFiles { cert, key }),
        (None, None) => None,
        (Some(_), None) => return Err(s.fail("tls_key", "required with server.tls_cert")),
        (None, Some(_)) => return Err(s.fail("tls_cert", "required with server.tls_key")),
    };
    let secs = Duration::from_secs;
    let tick = s.duration_or("tick", secs(5))?;
    let default_ttl = s.duration_or("default_ttl", secs(600))?;
    let max_ttl = s.duration_or("max_ttl", secs(3600))?;
    let defaults = Limits::default();
    let connections = s.bounded("max_connections", 1, u64::from(u16::MAX), 256)?;
    let requests = s.bounded("max_requests", 1, u64::from(u32::MAX), 100)?;
    let limits = Limits {
        max_connections: u16::try_from(connections).unwrap_or(defaults.max_connections),
        handshake_timeout: s.duration_or("handshake_timeout", defaults.handshake_timeout)?,
        header_timeout: s.duration_or("header_timeout", defaults.header_timeout)?,
        read_timeout: s.duration_or("read_timeout", defaults.read_timeout)?,
        write_timeout: s.duration_or("write_timeout", defaults.write_timeout)?,
        max_requests: u32::try_from(requests).unwrap_or(defaults.max_requests),
        drain_timeout: s.duration_or("drain_timeout", defaults.drain_timeout)?,
    };
    let burst = s.bounded_given("agent_hello_burst", 1, u64::from(u32::MAX))?;
    let agents = AgentKeys {
        burst: burst.map(|burst| u32::try_from(burst).unwrap_or(u32::MAX)),
        interval: s.duration("agent_hello_interval")?,
        require_host_credential: s.boolean("require_host_credential")?,
    };
    // The same check as `--default-ttl` and `--max-ttl`.
    let settings = match Settings::new(default_ttl, max_ttl) {
        Ok(settings) => settings,
        Err(err) => return Err(s.fail("default_ttl", &err.to_string())),
    };
    Ok(Server {
        listen,
        agent_listen,
        advertise_url,
        scheduler,
        state_file,
        boot,
        client_token_file,
        tls,
        tick,
        settings,
        limits,
        agents,
    })
}

pub(super) fn read_client_token(server: &Server, ld: &Load<'_>) -> Result<Token, StartupError> {
    let place = "server.client_token_file";
    let path = &server.client_token_file;
    let bytes = match ld.files.read(path) {
        Ok(bytes) => bytes,
        Err(reason) => return Err(StartupError::at(place, reason)),
    };
    match parse_token(&bytes) {
        Ok(token) => Ok(token),
        Err(reason) => {
            let reason = format!("{} {reason}", path.display());
            Err(StartupError::at(place, reason))
        }
    }
}
