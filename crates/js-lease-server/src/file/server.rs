//! The `[server]` table of the configuration file: its keys and the client token file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use js_elastic::SchedulerId;
use js_jobs::Retention;
use js_sched_placer::sizes::{ColdCpuOn, ColdMemoryOn, ColdSize, Curves};

use super::{Load, Section};
use crate::config::{
    BootMode, CpuSlack, DEFAULT_LEASE_SECS, DEFAULT_RECHECK_MS, DispatchTimes, FeedRetention,
    JOB_FEED_RETENTION_MUST, JOB_RETENTION_MUST, JobListen, Limits, MAX_JOB_WALL_MUST,
    MOST_LEASE_SECS, MOST_RECHECK_MS, MetricsListen, Placement, ReadyListen, TlsFiles, Token,
    cold_cpu_entry, cold_entry, not_loopback, not_private, parse_curves, parse_job_feed_retention,
    parse_job_retention, parse_max_job_wall, parse_token,
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
    "agent_client_ca",
    "tick",
    "max_lease_wall",
    "max_connections",
    "handshake_timeout",
    "header_timeout",
    "read_timeout",
    "write_timeout",
    "max_requests",
    "drain_timeout",
    "drain_hold",
    "agent_hello_burst",
    "agent_hello_interval",
    "require_host_credential",
    "placement",
    "estimate_feed",
    "cpu_slack",
    "cold_cpu_millis",
    "cold_memory_bytes",
    "cold_memory_bytes_on",
    "cold_cpu_millis_on",
    "placement_curves",
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
    "dispatch_lease_secs",
    "dispatch_recheck_ms",
    "metrics_listen",
    "ready_listen",
];

/// What the `[server]` table says.
pub(crate) struct Server {
    pub(crate) listen: SocketAddr,
    pub(crate) agent_listen: Option<SocketAddr>,
    /// `metrics_listen`: where the metrics are served, when the file turns them on.
    pub(crate) metrics_listen: Option<MetricsListen>,
    /// `ready_listen`: where the readiness route is served, when the file turns it on.
    pub(crate) ready_listen: Option<ReadyListen>,
    pub(crate) advertise_url: Option<String>,
    pub(crate) scheduler: SchedulerId,
    pub(crate) state_file: PathBuf,
    pub(crate) boot: BootMode,
    pub(crate) client_token_file: PathBuf,
    pub(crate) tls: Option<TlsFiles>,
    /// `agent_client_ca`: the CA file of the hosts' certificates; only with `tls`.
    pub(crate) agent_client_ca: Option<PathBuf>,
    pub(crate) tick: Duration,
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
    /// `dispatch_lease_secs` and `dispatch_recheck_ms`: what the dispatch poll is timed by;
    /// 10 s and 500 ms unless the file sets them.
    pub(crate) dispatch: DispatchTimes,
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
    cold: ColdSize,
    cold_on: ColdMemoryOn,
    cold_cpu_on: ColdCpuOn,
    curves: Curves,
    job_listen: Option<SocketAddr>,
}

impl AgentKeys {
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

    /// Whether the deprecated `estimate_feed` key is on; off unless the file says so.
    pub(crate) fn estimate_feed(&self) -> bool {
        self.estimate_feed.unwrap_or(false)
    }

    /// The CPU slack the file gives, if it gives one; absent, the engine's default.
    pub(crate) fn cpu_slack(&self) -> Option<CpuSlack> {
        self.cpu_slack
    }

    /// What a job that books nothing books while neither its class, its fallback class nor its
    /// client has a run:
    /// `cold_cpu_millis` and `cold_memory_bytes` as the file gives them, each absent one its
    /// default (22 cores and 4 GiB, [`ColdSize::default`]).
    pub(crate) fn cold(&self) -> ColdSize {
        self.cold
    }

    /// The cold memory of the jobs of each operating system `cold_memory_bytes_on` names, in
    /// place of `cold_memory_bytes`; none when the file gives no such table.
    pub(crate) fn cold_memory_on(&self) -> ColdMemoryOn {
        self.cold_on.clone()
    }

    /// The cold CPU of the jobs of each operating system `cold_cpu_millis_on` names, in place
    /// of `cold_cpu_millis`; none when the file gives no such table.
    pub(crate) fn cold_cpu_on(&self) -> ColdCpuOn {
        self.cold_cpu_on.clone()
    }

    /// The placement engine's curves mode the file gives (`placement_curves`); `off` when
    /// the key is absent.
    pub(crate) fn curves(&self) -> Curves {
        self.curves
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
    let ready_listen = read_ready_listen(&mut s)?;
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
    let agent_client_ca = s.path("agent_client_ca")?;
    if agent_client_ca.is_some() && tls.is_none() {
        let reason =
            "requires server.tls_cert and server.tls_key: a certificate is asked for over TLS only";
        return Err(s.fail("agent_client_ca", reason));
    }
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
        drain_hold: s.duration_or("drain_hold", defaults.drain_hold)?,
    };
    let burst = s.bounded_given("agent_hello_burst", 1, u64::from(u32::MAX))?;
    let agents = AgentKeys {
        burst: burst.map(|burst| u32::try_from(burst).unwrap_or(u32::MAX)),
        interval: s.duration("agent_hello_interval")?,
        require_host_credential: s.boolean("require_host_credential")?,
        placement: read_placement(&mut s)?,
        estimate_feed: s.boolean("estimate_feed")?,
        cpu_slack: read_cpu_slack(&mut s)?,
        cold: read_cold(&mut s)?,
        cold_on: ColdMemoryOn::new(read_per_os(&mut s, &COLD_MEMORY_ON)?),
        cold_cpu_on: ColdCpuOn::new(read_per_os(&mut s, &COLD_CPU_ON)?),
        curves: read_curves(&mut s)?,
        job_listen: s.address("job_listen")?,
    };
    let usage = read_usage(&mut s)?;
    // The rule of `--estimate-feed`, kept with the deprecated key: not beside usage off.
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
    let dispatch = read_dispatch(&mut s)?;
    let max_lease_wall = s.duration("max_lease_wall")?;
    if let Some(wall) = max_lease_wall {
        check_ceiling(&s, wall)?;
    }
    Ok(Server {
        listen,
        agent_listen,
        metrics_listen,
        ready_listen,
        advertise_url,
        scheduler,
        state_file,
        boot,
        client_token_file,
        tls,
        agent_client_ca,
        tick,
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
        dispatch,
    })
}

/// The dispatch poll's keys: `dispatch_lease_secs` from 1 to [`MOST_LEASE_SECS`] and
/// `dispatch_recheck_ms` from 1 to [`MOST_RECHECK_MS`] (below the hold), each its default when
/// the file leaves it out.
fn read_dispatch(s: &mut Section) -> Result<DispatchTimes, StartupError> {
    let lease = s.bounded(
        "dispatch_lease_secs",
        1,
        MOST_LEASE_SECS,
        DEFAULT_LEASE_SECS,
    )?;
    let recheck = s.bounded(
        "dispatch_recheck_ms",
        1,
        MOST_RECHECK_MS,
        DEFAULT_RECHECK_MS,
    )?;
    Ok(DispatchTimes {
        lease: Duration::from_secs(lease),
        recheck: Duration::from_millis(recheck),
        ..DispatchTimes::default()
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

/// The `ready_listen` key, refused when it is not a loopback address as `--ready-listen` is;
/// `None` when the file does not give it.
fn read_ready_listen(s: &mut Section) -> Result<Option<ReadyListen>, StartupError> {
    let Some(addr) = s.address("ready_listen")? else {
        return Ok(None);
    };
    if let Some(reason) = not_loopback(addr) {
        return Err(s.fail("ready_listen", &reason));
    }
    let flag = "server.ready_listen";
    Ok(Some(ReadyListen { addr, flag }))
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

/// Refuses a ceiling `wall` on a work's wall time that is longer than 30 days.
fn check_ceiling(s: &Section, wall: Duration) -> Result<(), StartupError> {
    match wall > super::lifetime::LONGEST {
        true => Err(s.fail("max_lease_wall", "must not be longer than 30 days")),
        false => Ok(()),
    }
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

/// `placement_curves`, the key of `--placement-curves`: `"off"` or `"shadow"`, read as the
/// flag reads its text; absent, `off`.
fn read_curves(s: &mut Section) -> Result<Curves, StartupError> {
    let text = s.string("placement_curves")?;
    let read = text.as_deref().map(parse_curves).transpose();
    let curves = read.map_err(|said| s.fail("placement_curves", &said))?;
    Ok(curves.unwrap_or_default())
}

/// The least `cold_cpu_millis`: a quarter core, the least CPU the estimator books.
const COLD_CPU_LEAST: u64 = 250;

/// The least `cold_memory_bytes`: 1 GiB, the least memory the estimator books.
const COLD_MEMORY_LEAST: u64 = 1 << 30;

/// `cold_cpu_millis` and `cold_memory_bytes`: whole numbers, the CPU from a quarter core
/// (250) to the most millicores a host can report, the memory from 1 GiB (the least memory
/// the estimator books). Each left out is its default ([`ColdSize::default`]).
fn read_cold(s: &mut Section) -> Result<ColdSize, StartupError> {
    let most_cpu = u64::from(u32::MAX);
    let default = ColdSize::default();
    Ok(ColdSize {
        cpu_millis: s.bounded(
            "cold_cpu_millis",
            COLD_CPU_LEAST,
            most_cpu,
            default.cpu_millis,
        )?,
        memory_bytes: s.bounded(
            "cold_memory_bytes",
            COLD_MEMORY_LEAST,
            u64::MAX,
            default.memory_bytes,
        )?,
    })
}

/// How one entry of a per-system table is checked: the name of the operating system and its
/// value, to the entry or what is wrong.
type CheckEntry = fn(&str, u64) -> Result<(String, u64), String>;

/// A table of each operating system's value: its key, what its values count and how an entry
/// is checked.
struct PerOs {
    /// The key of the `[server]` table.
    key: &'static str,
    /// What a value counts, as a refusal names it: `bytes`, `millicores`.
    unit: &'static str,
    /// The check of one entry, the flag's.
    entry: CheckEntry,
}

/// `cold_memory_bytes_on`: each operating system's cold memory, whole numbers of bytes from
/// 1 GiB (the rules of `--cold-memory-bytes-on`).
const COLD_MEMORY_ON: PerOs = PerOs {
    key: "cold_memory_bytes_on",
    unit: "bytes",
    entry: cold_entry,
};

/// `cold_cpu_millis_on`: each operating system's cold CPU, whole numbers of millicores from
/// 250 to 4294967295 (the rules of `--cold-cpu-millis-on`).
const COLD_CPU_ON: PerOs = PerOs {
    key: "cold_cpu_millis_on",
    unit: "millicores",
    entry: cold_cpu_entry,
};

/// The table `of` names, each of its keys the name of an operating system as jobs give it
/// (`macos = 4294967296`) and each value a whole number its check takes. Left out, none.
fn read_per_os(s: &mut Section, of: &PerOs) -> Result<BTreeMap<String, u64>, StartupError> {
    let PerOs { key, unit, entry } = *of;
    let Some(table) = s.table(key)? else {
        return Ok(BTreeMap::new());
    };
    let mut entries = BTreeMap::new();
    for (os, value) in table {
        let number = value
            .as_integer()
            .and_then(|number| u64::try_from(number).ok())
            .ok_or_else(|| s.fail(key, &format!("{os}: must be a whole number of {unit}")))?;
        let (os, number) = entry(&os, number).map_err(|why| s.fail(key, &why))?;
        entries.insert(os, number);
    }
    Ok(entries)
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

    /// Asserts that a ceiling of 30 days is accepted and one a second longer is refused,
    /// naming the key. It catches a ceiling above the longest wall time that passes as valid.
    #[test]
    fn a_ceiling_above_thirty_days_is_refused_naming_the_key() {
        let s = Section::open(toml::Table::new(), "server.".to_owned());
        let longest = super::super::lifetime::LONGEST;
        assert!(check_ceiling(&s, longest).is_ok());
        let said = check_ceiling(&s, longest + Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert_eq!(
            said,
            "server.max_lease_wall: must not be longer than 30 days"
        );
    }

    /// The text of the refusal of the `[server]` table `text`.
    fn refusal(text: &str) -> String {
        let table: toml::Table = text.parse().unwrap();
        read_server(table).err().unwrap().to_string()
    }

    /// The cold size the `[server]` table with its required keys and `extra` gives.
    fn cold_of(extra: &str) -> Result<ColdSize, String> {
        let required = "state_file = \"js\"\nboot = \"initialise\"\nclient_token_file = \"t\"";
        let table: toml::Table = format!("{required}\n{extra}").parse().unwrap();
        let server = read_server(table).map_err(|err| err.to_string())?;
        Ok(server.agents.cold())
    }

    /// Asserts that `cold_cpu_millis` and `cold_memory_bytes` are read as given, each on its
    /// own, from their least (250 and 1 GiB) up to the most CPU (4294967295); that left out
    /// each is its default (22000 and 4 GiB, not anything of a host); and that a value under the
    /// least, negative or not an integer is refused at start naming the key and what it must be.
    /// Catches a key that is not read, a key read into the other dimension, and a cold size of
    /// no CPU or under the estimator's least memory.
    #[test]
    fn the_cold_size_keys_are_read_each_on_its_own_and_refused_out_of_range() {
        let none = ColdSize {
            cpu_millis: 22_000,
            memory_bytes: 4 << 30,
        };
        assert_eq!(cold_of(""), Ok(none));
        assert_eq!(ColdSize::default(), none);
        let farm = "cold_cpu_millis = 2000\ncold_memory_bytes = 8589934592";
        let read = cold_of(farm).unwrap();
        assert_eq!((read.cpu_millis, read.memory_bytes), (2000, 8 << 30));
        let least = cold_of("cold_cpu_millis = 250\ncold_memory_bytes = 1073741824").unwrap();
        assert_eq!((least.cpu_millis, least.memory_bytes), (250, 1 << 30));
        let most = cold_of("cold_cpu_millis = 4294967295").unwrap();
        assert_eq!(most.cpu_millis, u64::from(u32::MAX));
        let cpu = cold_of("cold_cpu_millis = 2000").unwrap();
        assert_eq!((cpu.cpu_millis, cpu.memory_bytes), (2000, 4 << 30));
        let memory = cold_of("cold_memory_bytes = 8589934592").unwrap();
        assert_eq!((memory.cpu_millis, memory.memory_bytes), (22_000, 8 << 30));
        let cpu_range = "server.cold_cpu_millis: must be from 250 to 4294967295";
        let memory_range =
            "server.cold_memory_bytes: must be from 1073741824 to 18446744073709551615";
        for (extra, said) in [
            ("cold_cpu_millis = 249", cpu_range),
            ("cold_cpu_millis = 0", cpu_range),
            ("cold_cpu_millis = 4294967296", cpu_range),
            (
                "cold_cpu_millis = -1",
                "server.cold_cpu_millis: must not be negative",
            ),
            (
                "cold_cpu_millis = \"2000\"",
                "server.cold_cpu_millis: must be an integer",
            ),
            (
                "cold_cpu_millis = 2.5",
                "server.cold_cpu_millis: must be an integer",
            ),
            ("cold_memory_bytes = 1073741823", memory_range),
            ("cold_memory_bytes = 0", memory_range),
            (
                "cold_memory_bytes = \"8GiB\"",
                "server.cold_memory_bytes: must be an integer",
            ),
        ] {
            assert_eq!(cold_of(extra), Err(said.to_owned()), "{extra}");
        }
    }

    /// The cold memory of each operating system the `[server]` table with its required keys
    /// and `extra` gives.
    fn cold_on_of(extra: &str) -> Result<ColdMemoryOn, String> {
        let required = "state_file = \"js\"\nboot = \"initialise\"\nclient_token_file = \"t\"";
        let table: toml::Table = format!("{required}\n{extra}").parse().unwrap();
        let server = read_server(table).map_err(|err| err.to_string())?;
        Ok(server.agents.cold_memory_on())
    }

    /// Asserts that `cold_memory_bytes_on` is read as a table of each operating system's
    /// bytes, from the least (1 GiB), and that left out it names none; and that a value that is
    /// not a whole number of bytes, one under the least, an empty name or a key that is not a
    /// table is refused at start naming the key and what is wrong. Catches a table that is not
    /// read, an entry under the estimator's least memory, and a name no job could give.
    #[test]
    fn the_cold_memory_of_each_os_is_read_and_refused_out_of_range() {
        assert_eq!(cold_on_of(""), Ok(ColdMemoryOn::default()));
        let both = "cold_memory_bytes_on = { macos = 4294967296, linux = 1073741824 }";
        let entries = [("macos".to_owned(), 4 << 30), ("linux".to_owned(), 1 << 30)];
        assert_eq!(
            cold_on_of(both),
            Ok(ColdMemoryOn::new(entries.into_iter().collect()))
        );
        let key = "server.cold_memory_bytes_on";
        for (extra, said) in [
            (
                "cold_memory_bytes_on = { macos = \"4GiB\" }",
                format!("{key}: macos: must be a whole number of bytes"),
            ),
            (
                "cold_memory_bytes_on = { macos = -1 }",
                format!("{key}: macos: must be a whole number of bytes"),
            ),
            (
                "cold_memory_bytes_on = { macos = 1073741823 }",
                format!("{key}: macos: 1073741823 is below the least, 1073741824 (1 GiB)"),
            ),
            (
                "cold_memory_bytes_on = { \"\" = 4294967296 }",
                format!("{key}: the operating system is empty"),
            ),
            (
                "cold_memory_bytes_on = 4294967296",
                format!("{key}: must be a table"),
            ),
        ] {
            assert_eq!(cold_on_of(extra), Err(said), "{extra}");
        }
    }

    /// The cold CPU of each operating system the `[server]` table with its required keys and
    /// `extra` gives.
    fn cold_cpu_on_of(extra: &str) -> Result<ColdCpuOn, String> {
        let required = "state_file = \"js\"\nboot = \"initialise\"\nclient_token_file = \"t\"";
        let table: toml::Table = format!("{required}\n{extra}").parse().unwrap();
        let server = read_server(table).map_err(|err| err.to_string())?;
        Ok(server.agents.cold_cpu_on())
    }

    /// Asserts that `cold_cpu_millis_on` is read as a table of each operating system's
    /// millicores, from the least (250) to the most (4294967295), beside the cold memory of
    /// `cold_memory_bytes_on` and apart from it, and that left out it names none; and that a
    /// value that is not a whole number of millicores, one under the least or over the most, an
    /// empty name or a key that is not a table is refused at start naming the key and what is
    /// wrong. Catches a table that is not read, one read into the cold memory's place, an entry
    /// under the estimator's least CPU, and a name no job could give.
    #[test]
    fn the_cold_cpu_of_each_os_is_read_and_refused_out_of_range() {
        assert_eq!(cold_cpu_on_of(""), Ok(ColdCpuOn::default()));
        let both = "cold_cpu_millis_on = { macos = 250, linux = 4294967295 }\n\
                    cold_memory_bytes_on = { macos = 4294967296 }";
        let entries = [
            ("macos".to_owned(), 250),
            ("linux".to_owned(), 4_294_967_295),
        ];
        assert_eq!(
            cold_cpu_on_of(both),
            Ok(ColdCpuOn::new(entries.into_iter().collect()))
        );
        let memory = [("macos".to_owned(), 4 << 30)];
        assert_eq!(
            cold_on_of(both),
            Ok(ColdMemoryOn::new(memory.into_iter().collect()))
        );
        let key = "server.cold_cpu_millis_on";
        let range = "is not from 250 (a quarter core) to 4294967295";
        for (extra, said) in [
            (
                "cold_cpu_millis_on = { macos = \"1000m\" }",
                format!("{key}: macos: must be a whole number of millicores"),
            ),
            (
                "cold_cpu_millis_on = { macos = -1 }",
                format!("{key}: macos: must be a whole number of millicores"),
            ),
            (
                "cold_cpu_millis_on = { macos = 249 }",
                format!("{key}: macos: 249 {range}"),
            ),
            (
                "cold_cpu_millis_on = { macos = 4294967296 }",
                format!("{key}: macos: 4294967296 {range}"),
            ),
            (
                "cold_cpu_millis_on = { \"\" = 1000 }",
                format!("{key}: the operating system is empty"),
            ),
            (
                "cold_cpu_millis_on = 1000",
                format!("{key}: must be a table"),
            ),
        ] {
            assert_eq!(cold_cpu_on_of(extra), Err(said), "{extra}");
        }
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
