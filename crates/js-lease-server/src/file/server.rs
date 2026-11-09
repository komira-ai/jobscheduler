//! The `[server]` table of the configuration file: its keys and the client token file.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_jobs::Retention;
use js_lease::Settings;

use super::{Load, Section};
use crate::config::{
    BootMode, CpuSlack, FeedRetention, JOB_FEED_RETENTION_MUST, JOB_RETENTION_MUST, JobListen,
    Limits, MAX_JOB_WALL_MUST, MetricsListen, Placement, TlsFiles, Token, not_private,
    parse_job_feed_retention, parse_job_retention, parse_max_job_wall, parse_token,
};
use crate::error::StartupError;
use crate::job_store::JobStoreSetup;
use crate::rate::HelloLimit;
use crate::store_health::UNREACHABLE_AFTER;
use crate::usage_setup::{DEFAULT_HISTORY_RETENTION_DAYS, DEFAULT_PROFILE_STORE_BYTES};

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
    "estimate_feed",
    "cpu_slack",
    "usage_db",
    "usage_off",
    "profile_store_bytes",
    "history_retention_days",
    "alert_webhook_url",
    "alert_webhook_token_file",
    "alert_repeat_after",
    "max_spend_per_day_total",
    "max_spend_per_month_total",
    "max_spend_since_go_total",
    "job_retention",
    "job_feed_retention",
    "job_allocation_retention",
    "job_listen",
    "max_job_wall",
    "store",
    "allow_sqlite_store",
    "store_unreachable_after",
    "metrics_listen",
];

/// What the `[server]` table says.
pub(crate) struct Server {
    pub(crate) listen: SocketAddr,
    pub(crate) agent_listen: Option<SocketAddr>,
    /// `metrics_listen`: where the metrics are served, when the file turns them on.
    pub(crate) metrics_listen: Option<MetricsListen>,
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
    /// `job_retention`: how long an ended job is kept; seven days unless the file sets it.
    pub(crate) job_retention: Retention,
    /// `job_feed_retention`: how many of the newest changes of the job store's feed are
    /// kept; 100000 unless the file sets it.
    pub(crate) job_feed_retention: FeedRetention,
    /// `job_allocation_retention`: how long a freed allocation's row is kept; fourteen days
    /// unless the file sets it.
    pub(crate) job_allocation_retention: Retention,
    /// `max_job_wall`: the most a submitted job's wall limit may be, when the file sets it.
    pub(crate) max_job_wall: Option<Duration>,
    /// `store`: the file that holds the store's connection, when the file sets one.
    pub(crate) store: Option<PathBuf>,
    /// `allow_sqlite_store`: whether a `sqlite:` store is accepted; false unless set.
    pub(crate) allow_sqlite_store: bool,
    /// `store_unreachable_after`: how long the tick's store paths may fail before the store
    /// is unreachable; 60 s unless the file sets it.
    pub(crate) store_unreachable_after: Duration,
}

/// The alert keys of `[server]`: where each alert the scaler raises is POSTed, and the file
/// that holds the bearer token the receiver wants, when it wants one.
pub(crate) struct AlertKeys {
    pub(crate) webhook_url: String,
    pub(crate) token_file: Option<PathBuf>,
    /// How often an alert that keeps holding is sent again.
    pub(crate) repeat_after: Duration,
}

/// The usage keys of `[server]`, as the file gave them, or the flags `--usage-db`,
/// `--usage-off`, `--profile-store-bytes` and `--history-retention-days`, with their
/// defaults.
pub(crate) struct UsageKeys {
    pub(crate) db: Option<PathBuf>,
    pub(crate) off: bool,
    pub(crate) profile_store_bytes: u64,
    pub(crate) history_retention_days: u64,
}

/// The keys of `[server]` that belong to agent registration, as the file gave them. They
/// mean something only with a pool that registers agents. The job service is one of them:
/// its store is held by the agent side, and its jobs run on hosts that register.
pub(crate) struct AgentKeys {
    burst: Option<u32>,
    interval: Option<Duration>,
    require_host_credential: Option<bool>,
    placement: Option<Placement>,
    estimate_feed: Option<bool>,
    cpu_slack: Option<CpuSlack>,
    job_listen: Option<SocketAddr>,
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
            ("estimate_feed", self.estimate_feed.is_some()),
            ("cpu_slack", self.cpu_slack.is_some()),
            ("job_listen", self.job_listen.is_some()),
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

    /// The deprecated `placement` key, as the file gave it.
    pub(crate) fn placement(&self) -> Option<Placement> {
        self.placement
    }

    /// Whether each lease's estimated end is fed to the drain cost; off unless the file
    /// says so.
    pub(crate) fn estimate_feed(&self) -> bool {
        self.estimate_feed.unwrap_or(false)
    }

    /// The CPU slack the file gives, if it gives one; absent, the engine's default.
    pub(crate) fn cpu_slack(&self) -> Option<CpuSlack> {
        self.cpu_slack
    }

    /// Where the job service is served, when the file turns it on.
    pub(crate) fn job_listen(&self) -> Option<JobListen> {
        let flag = "server.job_listen";
        self.job_listen.map(|addr| JobListen { addr, flag })
    }

    /// The job store's setup when the file turns the job service on: named by its key, and
    /// keeping an ended job, the newest changes of its feed and a freed allocation as
    /// `windows` says.
    pub(crate) fn job_store(&self, windows: JobStoreWindows) -> Option<JobStoreSetup> {
        let flag = "server.job_listen";
        self.job_listen.map(|_| JobStoreSetup {
            flag,
            retention: windows.retention,
            feed_retention: windows.feed_retention,
            allocation_retention: windows.allocation_retention,
        })
    }
}

pub(super) fn read_server(table: toml::Table) -> Result<Server, StartupError> {
    let mut s = Section::open(table, "server.".to_owned());
    s.only(SERVER_KEYS)?;
    let listen = s.address("listen")?;
    let listen = listen.unwrap_or(SocketAddr::from(([127, 0, 0, 1], 7300)));
    let agent_listen = s.address("agent_listen")?;
    let metrics_listen = read_metrics_listen(&mut s)?;
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
        estimate_feed: s.boolean("estimate_feed")?,
        cpu_slack: read_cpu_slack(&mut s)?,
        job_listen: s.address("job_listen")?,
    };
    let usage = read_usage(&mut s)?;
    // The rule of `--estimate-feed`: with usage off there is nothing to estimate from.
    if agents.estimate_feed() & usage.off {
        return Err(s.fail("estimate_feed", "not allowed with server.usage_off"));
    }
    let totals = super::budget::read_totals(&mut s)?;
    let alerts = read_alerts(&mut s)?;
    let job_retention = read_job_retention(&mut s)?;
    let job_feed_retention = read_job_feed_retention(&mut s)?;
    let job_allocation_retention = read_job_allocation_retention(&mut s)?;
    let max_job_wall = read_max_job_wall(&mut s)?;
    let (store, allow_sqlite_store) = read_store(&mut s)?;
    let store_unreachable_after = s.duration_or("store_unreachable_after", UNREACHABLE_AFTER)?;
    let settings = Settings::new();
    let max_lease_wall = s.duration("max_lease_wall")?;
    let settings = match max_lease_wall {
        Some(wall) => with_ceiling(&s, settings, wall)?,
        None => settings,
    };
    Ok(Server {
        listen,
        agent_listen,
        metrics_listen,
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
        job_retention,
        job_feed_retention,
        job_allocation_retention,
        max_job_wall,
        store,
        allow_sqlite_store,
        store_unreachable_after,
    })
}

/// The `store` key and the SQLite gate, `allow_sqlite_store`, which is refused as `true`
/// without the store it would apply to, as `--allow-sqlite-store` is without `--store`.
fn read_store(s: &mut Section) -> Result<(Option<PathBuf>, bool), StartupError> {
    let store = s.path("store")?;
    let allow = s.boolean("allow_sqlite_store")?.unwrap_or(false);
    match (&store, allow) {
        (None, true) => Err(s.fail("allow_sqlite_store", "needs server.store")),
        _ => Ok((store, allow)),
    }
}

/// The `metrics_listen` key, refused when it is not a private address as `--metrics-listen`
/// is; `None` when the file does not give it.
fn read_metrics_listen(s: &mut Section) -> Result<Option<MetricsListen>, StartupError> {
    let Some(addr) = s.address("metrics_listen")? else {
        return Ok(None);
    };
    if let Some(reason) = not_private(addr) {
        return Err(s.fail("metrics_listen", &reason));
    }
    let flag = "server.metrics_listen";
    Ok(Some(MetricsListen { addr, flag }))
}

/// The `max_job_wall` key, read as `--max-job-wall` is; `None` when the file does not give
/// it. A refusal says what the value must be and does not repeat it.
fn read_max_job_wall(s: &mut Section) -> Result<Option<Duration>, StartupError> {
    let Some(text) = s.string("max_job_wall")? else {
        return Ok(None);
    };
    let wall = parse_max_job_wall(&text).map_err(|_| s.fail("max_job_wall", MAX_JOB_WALL_MUST))?;
    Ok(Some(wall))
}

/// The `job_retention` key, read as `--job-retention` is; [`Retention::DEFAULT`] when the
/// file does not give it. A refusal says what the value must be and does not repeat it.
fn read_job_retention(s: &mut Section) -> Result<Retention, StartupError> {
    let Some(text) = s.string("job_retention")? else {
        return Ok(Retention::DEFAULT);
    };
    parse_job_retention(&text).map_err(|_| s.fail("job_retention", JOB_RETENTION_MUST))
}

/// The `job_feed_retention` key, read as `--job-feed-retention` is; [`FeedRetention::DEFAULT`]
/// when the file does not give it. A refusal says what the value must be and does not
/// repeat it.
fn read_job_feed_retention(s: &mut Section) -> Result<FeedRetention, StartupError> {
    let Some(text) = s.string("job_feed_retention")? else {
        return Ok(FeedRetention::DEFAULT);
    };
    parse_job_feed_retention(&text)
        .map_err(|_| s.fail("job_feed_retention", JOB_FEED_RETENTION_MUST))
}

/// The `job_allocation_retention` key, read as `--job-allocation-retention` is;
/// [`Retention::ALLOCATION_DEFAULT`] when the file does not give it. A refusal says what the
/// value must be and does not repeat it.
fn read_job_allocation_retention(s: &mut Section) -> Result<Retention, StartupError> {
    let Some(text) = s.string("job_allocation_retention")? else {
        return Ok(Retention::ALLOCATION_DEFAULT);
    };
    parse_job_retention(&text).map_err(|_| s.fail("job_allocation_retention", JOB_RETENTION_MUST))
}

/// The windows of the job store the `[server]` table gives, for [`AgentKeys::job_store`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobStoreWindows {
    /// `job_retention`.
    pub(crate) retention: Retention,
    /// `job_feed_retention`.
    pub(crate) feed_retention: FeedRetention,
    /// `job_allocation_retention`.
    pub(crate) allocation_retention: Retention,
}

impl Server {
    /// The job store's windows this table gives.
    pub(crate) fn job_store_windows(&self) -> JobStoreWindows {
        JobStoreWindows {
            retention: self.job_retention,
            feed_retention: self.job_feed_retention,
            allocation_retention: self.job_allocation_retention,
        }
    }
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

/// `placement`, the deprecated key of `--placement`: `js-sched`, or the removed `registry`,
/// refused here, as `[server]` is read and before any pool is, as the flag is.
fn read_placement(s: &mut Section) -> Result<Option<Placement>, StartupError> {
    let text = s.string("placement")?;
    let place = format!("{}placement", s.head);
    let named = |text: String| match text.as_str() {
        "registry" => Placement::given(Some(Placement::Registry), &place).map(|_| None),
        "js-sched" => Ok(Some(Placement::JsSched)),
        _ => Err(s.fail("placement", "must be js-sched")),
    };
    Ok(text.map(named).transpose()?.flatten())
}

/// `cpu_slack`, the key of `--cpu-slack`: a number written as the multiplier, such as
/// `1.1`, read as the flag reads its text. A string, even one that spells a number, is
/// refused, as every other number of the file is.
fn read_cpu_slack(s: &mut Section) -> Result<Option<CpuSlack>, StartupError> {
    let text = match s.map.remove("cpu_slack") {
        None => return Ok(None),
        Some(toml::Value::Float(number)) => number.to_string(),
        Some(toml::Value::Integer(number)) => number.to_string(),
        Some(_) => return Err(s.fail("cpu_slack", "must be a number such as 1.1")),
    };
    let slack = CpuSlack::parse(&text).map_err(|err| s.fail("cpu_slack", &err.to_string()))?;
    Ok(Some(slack))
}

/// The usage keys. The rules are the flags': a budget and a retention of at least 1, and no
/// `usage_db` beside `usage_off = true`.
fn read_usage(s: &mut Section) -> Result<UsageKeys, StartupError> {
    let db = s.path("usage_db")?;
    let off = s.boolean("usage_off")?.unwrap_or(false);
    let bytes = s.bounded(
        "profile_store_bytes",
        1,
        u64::MAX,
        DEFAULT_PROFILE_STORE_BYTES,
    )?;
    let days = s.bounded(
        "history_retention_days",
        1,
        u64::MAX,
        DEFAULT_HISTORY_RETENTION_DAYS,
    )?;
    if db.is_some() & off {
        return Err(s.fail("usage_db", "not allowed with server.usage_off"));
    }
    Ok(UsageKeys {
        db,
        off,
        profile_store_bytes: bytes,
        history_retention_days: days,
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
        let base = Settings::new();
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

    /// The text of the refusal of the `[server]` table `text`.
    fn refusal(text: &str) -> String {
        let table: toml::Table = text.parse().unwrap();
        read_server(table).err().unwrap().to_string()
    }

    /// Asserts that `server.default_ttl` and `server.max_ttl` are told as unknown keys, as
    /// any key the section does not read is, and that the list of known keys names neither.
    #[test]
    fn a_ttl_key_is_an_unknown_key() {
        let head = "known: ";
        for key in ["default_ttl", "max_ttl"] {
            let said = refusal(&format!("{key} = \"60s\""));
            assert!(
                said.starts_with(&format!("server.{key}: unknown field; {head}")),
                "{said}"
            );
            assert!(!said[said.find(head).unwrap()..].contains("ttl"), "{said}");
        }
    }
}
