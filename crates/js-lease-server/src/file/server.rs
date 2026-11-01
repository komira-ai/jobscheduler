//! The `[server]` table of the configuration file: its keys and the client token file.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_lease::Settings;

use super::{Load, Section};
use crate::config::{BootMode, Limits, Placement, TlsFiles, Token, parse_token};
use crate::error::StartupError;
use crate::rate::HelloLimit;
use crate::usage_setup::DEFAULT_PROFILE_STORE_BYTES;

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
    "max_lease_wall",
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
    "placement",
    "usage_db",
    "usage_off",
    "profile_store_bytes",
    "alert_webhook_url",
    "alert_webhook_token_file",
    "alert_repeat_after",
    "max_spend_per_day_total",
    "max_spend_per_month_total",
    "max_spend_since_go_total",
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
    /// `max_lease_wall`: the ceiling on a lease's `max_wall`, when the file sets one.
    pub(crate) max_lease_wall: Option<Duration>,
    pub(crate) limits: Limits,
    pub(crate) agents: AgentKeys,
    pub(crate) usage: UsageKeys,
    /// The totals over every pool of the server.
    pub(crate) totals: js_scaler::Totals,
    pub(crate) alerts: Option<AlertKeys>,
}

/// The alert keys of `[server]`: where each alert the scaler raises is POSTed, and the file
/// that holds the bearer token the receiver wants, when it wants one.
pub(crate) struct AlertKeys {
    pub(crate) webhook_url: String,
    pub(crate) token_file: Option<PathBuf>,
    /// How often an alert that keeps holding is sent again.
    pub(crate) repeat_after: Duration,
}

/// The usage keys of `[server]`, as the file gave them: the flags `--usage-db`,
/// `--usage-off` and `--profile-store-bytes`, with their defaults.
pub(crate) struct UsageKeys {
    pub(crate) db: Option<PathBuf>,
    pub(crate) off: bool,
    pub(crate) profile_store_bytes: u64,
}

/// The keys of `[server]` that belong to agent registration, as the file gave them. They
/// mean something only with a pool that registers agents.
pub(crate) struct AgentKeys {
    burst: Option<u32>,
    interval: Option<Duration>,
    require_host_credential: Option<bool>,
    placement: Option<Placement>,
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
            ("placement", self.placement.is_some()),
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

    /// Who places a lease on a registered host; the registry unless the file says so.
    pub(crate) fn placement(&self) -> Placement {
        self.placement.unwrap_or_default()
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
        placement: read_placement(&mut s)?,
    };
    let usage = read_usage(&mut s)?;
    let totals = super::budget::read_totals(&mut s)?;
    let alerts = read_alerts(&mut s)?;
    // The same check as `--default-ttl` and `--max-ttl`.
    let settings = match Settings::new(default_ttl, max_ttl) {
        Ok(settings) => settings,
        Err(err) => return Err(s.fail("default_ttl", &err.to_string())),
    };
    let max_lease_wall = s.duration("max_lease_wall")?;
    let settings = match max_lease_wall {
        Some(wall) => with_ceiling(&s, settings, wall)?,
        None => settings,
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
        max_lease_wall,
        limits,
        agents,
        usage,
        totals,
        alerts,
    })
}

/// The alert keys. The token file and the repeat period mean something only with the URL.
fn read_alerts(s: &mut Section) -> Result<Option<AlertKeys>, StartupError> {
    let url = s.string("alert_webhook_url")?;
    let token_file = s.path("alert_webhook_token_file")?;
    let repeat_after = s.duration("alert_repeat_after")?;
    let Some(webhook_url) = url else {
        let given = [
            ("alert_webhook_token_file", token_file.is_some()),
            ("alert_repeat_after", repeat_after.is_some()),
        ];
        return match given.into_iter().find(|(_, set)| *set) {
            Some((key, _)) => Err(s.fail(key, "needs server.alert_webhook_url")),
            None => Ok(None),
        };
    };
    Ok(Some(AlertKeys {
        webhook_url,
        token_file,
        repeat_after: repeat_after.unwrap_or(Duration::from_secs(3600)),
    }))
}

/// `settings` with `wall` as the ceiling on a lease's wall time: at most 30 days, and
/// positive (the settings refuse a zero, which `duration` has already refused, and the
/// refusal is passed on if that ever changes).
fn with_ceiling(s: &Section, settings: Settings, wall: Duration) -> Result<Settings, StartupError> {
    if wall > super::lifetime::LONGEST {
        return Err(s.fail("max_lease_wall", "must not be longer than 30 days"));
    }
    settings
        .with_max_wall(wall)
        .map_err(|err| s.fail("max_lease_wall", &err.to_string()))
}

/// `placement`, the key of `--placement`: `registry` or `js-sched`.
fn read_placement(s: &mut Section) -> Result<Option<Placement>, StartupError> {
    let text = s.string("placement")?;
    let named = |text: String| match text.as_str() {
        "registry" => Ok(Placement::Registry),
        "js-sched" => Ok(Placement::JsSched),
        _ => Err(s.fail("placement", "must be registry or js-sched")),
    };
    text.map(named).transpose()
}

/// The usage keys. The rules are the flags': a budget of at least 1, and no `usage_db`
/// beside `usage_off = true`.
fn read_usage(s: &mut Section) -> Result<UsageKeys, StartupError> {
    let db = s.path("usage_db")?;
    let off = s.boolean("usage_off")?.unwrap_or(false);
    let bytes = s.bounded(
        "profile_store_bytes",
        1,
        u64::MAX,
        DEFAULT_PROFILE_STORE_BYTES,
    )?;
    if db.is_some() & off {
        return Err(s.fail("usage_db", "not allowed with server.usage_off"));
    }
    Ok(UsageKeys {
        db,
        off,
        profile_store_bytes: bytes,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a ceiling the settings refuse (a zero) is an error naming the key and
    /// not a ceiling silently dropped, and that 30 days is the longest accepted. A zero
    /// cannot come from the file, whose durations are positive, so this is the only way to
    /// reach the refusal.
    #[test]
    fn a_ceiling_is_never_dropped_silently() {
        let s = Section::open(toml::Table::new(), "server.".to_owned());
        let base = Settings::new(Duration::from_secs(60), Duration::from_secs(120)).unwrap();
        let said = with_ceiling(&s, base, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert_eq!(
            said,
            "server.max_lease_wall: the ceiling on the maximum wall time must be positive"
        );
        let longest = super::super::lifetime::LONGEST;
        assert!(with_ceiling(&s, base, longest).is_ok());
        assert!(with_ceiling(&s, base, longest + Duration::from_secs(1)).is_err());
    }
}
