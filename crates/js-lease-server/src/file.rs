//! The configuration file of `--config`: a `[server]` table and `[[pool]]` tables.
//!
//! Every table refuses a field it does not know, and the file holds no secret: a secret is
//! the path of a file, read owner-only. A refusal names the pool and the field, such as
//! `pool "gpu": provider.api_key_file: ...`, and never repeats a value the file holds: a
//! syntax error says the line, a value of the wrong type says what the field must be.
//!
//! The reader works on [`toml::Table`] values rather than on derived structures, because
//! the messages of a derived reader quote the value they stopped at, and the value may be
//! a secret written where a path belongs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper::Uri;
use js_elastic::ElasticProvider;
use js_lease::Settings;
use js_registry::{PoolConfig, WholeNodeConfig};
use js_types::{Admission, ClassId, ExecMode, ProviderId, ResourceClass, Resources, Shape};

use crate::adapt::SharedClock;
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::config::{BootMode, Limits, TlsFiles, Token, parse_duration, parse_token};
use crate::error::StartupError;
use crate::kinds::{BuildCtx, Built, ElasticBuilt, Kinds, SecretFiles};
use crate::network::{OAuthClient, valid_tag};

/// The most instances one pool may allow: the join-token table holds 256 waiting tokens.
const MAX_INSTANCES: u64 = 255;

/// The cpu a host of an elastic pool must report at least: one core.
const FLOOR_CPU_MILLIS: u64 = 1000;

/// The memory a host of an elastic pool must report at least: 1 GiB.
const FLOOR_MEMORY_BYTES: u64 = 1 << 30;

const ROOT_KEYS: &[&str] = &["server", "pool"];

const SERVER_KEYS: &[&str] = &[
    "listen",
    "agent_listen",
    "advertise_url",
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
];

const POOL_KEYS: &[&str] = &[
    "name",
    "class",
    "exec",
    "max_instances",
    "max_spend_per_hour",
    "max_spend_per_day",
    "idle",
    "boot_timeout",
    "launch",
    "join_token_file",
    "provider",
    "network",
];

/// Whether an elastic pool launches. A pool starts in `Plan`: it checks and shows its
/// plan and launches nothing. `Live` is the line that spends money; the scaler honours it,
/// and until the scaler exists nothing launches in either.
#[derive(Debug, PartialEq)]
pub enum Launch {
    /// Check and plan only.
    Plan,
    /// Launch.
    Live,
}

/// How a launched host reaches the server, as the pool's `network` table says it.
#[derive(Debug)]
pub enum NetworkPlan {
    /// The host uses the network it has.
    Default,
    /// The host joins a tailnet first.
    Tailscale {
        /// The OAuth client, read from its owner-only file; it prints as its id and `***`.
        client: OAuthClient,
        /// The tag every launched host carries, such as `tag:js-agent`.
        tag: String,
    },
}

/// An elastic pool, as the file describes it.
pub struct ElasticPlan {
    /// The pool's name.
    pub name: String,
    /// Whether it launches.
    pub launch: Launch,
    /// The most instances at once.
    pub max_instances: usize,
    /// The most it may spend in an hour, in cents.
    pub max_spend_per_hour_cents: u64,
    /// The most it may spend in a day, in cents.
    pub max_spend_per_day_cents: u64,
    /// How long a host may sit idle before it is released, when the file says.
    pub idle: Option<Duration>,
    /// How long a launched host has to say hello.
    pub boot_timeout: Duration,
    /// The execution mode the hosts must report, or `None` when the pool asked for none.
    pub exec: Option<ExecMode>,
    /// How a launched host reaches the server.
    pub network: NetworkPlan,
    provider: Box<dyn ElasticProvider>,
}

impl ElasticPlan {
    /// The provider the pool launches with.
    pub fn provider(&self) -> &dyn ElasticProvider {
        self.provider.as_ref()
    }
}

impl fmt::Debug for ElasticPlan {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("ElasticPlan")
            .field("name", &self.name)
            .field("launch", &self.launch)
            .field("max_instances", &self.max_instances)
            .field("max_spend_per_hour_cents", &self.max_spend_per_hour_cents)
            .field("max_spend_per_day_cents", &self.max_spend_per_day_cents)
            .field("idle", &self.idle)
            .field("boot_timeout", &self.boot_timeout)
            .field("exec", &self.exec)
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// What the `[server]` table says.
pub(crate) struct Server {
    pub(crate) listen: SocketAddr,
    pub(crate) agent_listen: Option<SocketAddr>,
    pub(crate) advertise_url: Option<String>,
    pub(crate) state_file: PathBuf,
    pub(crate) boot: BootMode,
    pub(crate) client_token_file: PathBuf,
    pub(crate) tls: Option<TlsFiles>,
    pub(crate) tick: Duration,
    pub(crate) settings: Settings,
    pub(crate) limits: Limits,
}

/// The registering pool: the plan, what the registry is built from, and the join tokens.
pub(crate) struct Registering {
    pub(crate) plan: ElasticPlan,
    pub(crate) pool: PoolConfig,
    pub(crate) tokens: AgentTokens,
}

/// The pools of the file, as the sources of hosts they make.
pub(crate) enum LoadedHosts {
    /// Static pools only.
    Static(Box<dyn CapacitySource>),
    /// An elastic pool that agents register into, beside a static pool when there is one.
    Registering {
        static_pool: Option<Box<dyn CapacitySource>>,
        registering: Box<Registering>,
    },
}

/// A configuration file that passed every check that needs no network.
pub(crate) struct Loaded {
    pub(crate) server: Server,
    pub(crate) client_token: Token,
    pub(crate) hosts: LoadedHosts,
    pub(crate) warnings: Vec<String>,
}

/// What reading the file needs besides its path.
pub(crate) struct Load<'a> {
    pub(crate) kinds: &'a Kinds,
    pub(crate) files: &'a dyn SecretFiles,
    pub(crate) clock: SharedClock,
    /// The class a name stands for; a seam, so a test can offer a class no name stands
    /// for today.
    pub(crate) classes: fn(ClassId) -> ResourceClass,
}

/// One table of the file, and where it is, for the messages.
struct Section {
    map: toml::Table,
    head: String,
}

impl Section {
    fn open(map: toml::Table, head: String) -> Self {
        Self { map, head }
    }

    /// A refusal of `key` of this table.
    fn fail(&self, key: &str, reason: &str) -> StartupError {
        StartupError::at(format!("{}{key}", self.head), reason)
    }

    /// Refuses the first field that is not one of `known`.
    fn only(&self, known: &[&str]) -> Result<(), StartupError> {
        for key in self.map.keys() {
            if !known.contains(&key.as_str()) {
                let reason = format!("unknown field; known: {}", known.join(", "));
                return Err(self.fail(key, &reason));
            }
        }
        Ok(())
    }

    fn string(&mut self, key: &str) -> Result<Option<String>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(self.fail(key, "must be a string")),
        }
    }

    fn required_string(&mut self, key: &str) -> Result<String, StartupError> {
        match self.string(key)? {
            Some(text) => Ok(text),
            None => Err(self.fail(key, "required")),
        }
    }

    fn path(&mut self, key: &str) -> Result<Option<PathBuf>, StartupError> {
        Ok(self.string(key)?.map(PathBuf::from))
    }

    fn required_path(&mut self, key: &str) -> Result<PathBuf, StartupError> {
        match self.path(key)? {
            Some(path) => Ok(path),
            None => Err(self.fail(key, "required")),
        }
    }

    fn integer(&mut self, key: &str) -> Result<Option<u64>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Integer(number)) => match u64::try_from(number) {
                Ok(number) => Ok(Some(number)),
                Err(_) => Err(self.fail(key, "must not be negative")),
            },
            Some(_) => Err(self.fail(key, "must be an integer")),
        }
    }

    /// The integer `key`, or `default`, which must be from `low` to `high`.
    fn bounded(
        &mut self,
        key: &str,
        low: u64,
        high: u64,
        default: u64,
    ) -> Result<u64, StartupError> {
        let number = self.integer(key)?.unwrap_or(default);
        match (low..=high).contains(&number) {
            true => Ok(number),
            false => Err(self.fail(key, &format!("must be from {low} to {high}"))),
        }
    }

    fn duration(&mut self, key: &str) -> Result<Option<Duration>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match parse_duration(&text) {
            Ok(duration) => Ok(Some(duration)),
            Err(_) => Err(self.fail(key, "must be a positive duration such as 600s, 10m or 1h")),
        }
    }

    fn duration_or(&mut self, key: &str, default: Duration) -> Result<Duration, StartupError> {
        Ok(self.duration(key)?.unwrap_or(default))
    }

    fn address(&mut self, key: &str) -> Result<Option<SocketAddr>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match text.parse::<SocketAddr>() {
            Ok(address) => Ok(Some(address)),
            Err(_) => Err(self.fail(key, "must be an address such as 127.0.0.1:7300")),
        }
    }

    fn table(&mut self, key: &str) -> Result<Option<toml::Table>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Table(table)) => Ok(Some(table)),
            Some(_) => Err(self.fail(key, "must be a table")),
        }
    }

    fn tables(&mut self, key: &str) -> Result<Vec<toml::Table>, StartupError> {
        let must = "must be an array of tables, written [[pool]]";
        let Some(value) = self.map.remove(key) else {
            return Ok(Vec::new());
        };
        let toml::Value::Array(items) = value else {
            return Err(self.fail(key, must));
        };
        let mut tables = Vec::new();
        for item in items {
            let toml::Value::Table(table) = item else {
                return Err(self.fail(key, must));
            };
            tables.push(table);
        }
        Ok(tables)
    }

    /// A spend in cents: a string holding a decimal with at most two places, above zero.
    fn spend(&mut self, key: &str) -> Result<Option<u64>, StartupError> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        match cents(&text) {
            None => Err(self.fail(
                key,
                "must be a decimal with at most two places, such as \"12.00\"",
            )),
            Some(0) => Err(self.fail(key, "must be above zero")),
            Some(amount) => Ok(Some(amount)),
        }
    }
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn digits(part: &str) -> bool {
    part.bytes().all(is_digit)
}

/// `text` as cents: digits, then optionally a point and one or two digits.
fn cents(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let valid = [
        !whole.is_empty(),
        fraction.len() <= 2,
        !text.ends_with('.'),
        digits(whole),
        digits(fraction),
    ];
    match valid.contains(&false) {
        true => None,
        false => format!("{whole}{fraction:0<2}").parse().ok(),
    }
}

/// The line of `text` at which the parser stopped; the first when it gives no position.
fn line_of(text: &str, error: &toml::de::Error) -> usize {
    let start = error.span().unwrap_or(0..0).start.min(text.len());
    1 + text.get(..start).unwrap_or("").matches('\n').count()
}

/// Reads the file at `path` and checks it, offline.
pub(crate) fn load(path: &Path, ld: &Load<'_>) -> Result<Loaded, StartupError> {
    let place = format!("--config {}", path.display());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => return Err(StartupError::at(&place, format!("cannot read: {err}"))),
    };
    // Neither `message` nor `Display` of a TOML error: both can quote the value the parser
    // stopped at, and the value may be a secret. Only the line is said.
    let document = match text.parse::<toml::Table>() {
        Ok(document) => document,
        Err(err) => {
            let line = line_of(&text, &err);
            let reason = format!("not valid TOML (line {line})");
            return Err(StartupError::at(&place, reason));
        }
    };
    let mut root = Section::open(document, String::new());
    root.only(ROOT_KEYS)?;
    let server = match root.table("server")? {
        Some(table) => table,
        None => return Err(root.fail("server", "required: add a [server] table")),
    };
    let pools = root.tables("pool")?;
    let server = read_server(server)?;
    let client_token = read_client_token(&server, ld)?;
    let mut static_pool = None;
    let mut registering = None;
    let mut names = BTreeSet::new();
    for (index, table) in pools.into_iter().enumerate() {
        let read = read_pool(table, index + 1, ld, &client_token)?;
        let pool = format!("pool {:?}", read.name);
        if !names.insert(read.name.clone()) {
            let reason = "also the name of an earlier pool";
            return Err(StartupError::at(format!("{pool}: name"), reason));
        }
        match read.pool {
            Pool::Static(source) => {
                if static_pool.replace(source).is_some() {
                    let reason = "only one static pool is served in this version";
                    return Err(StartupError::at(pool, reason));
                }
            }
            Pool::Elastic(elastic) => {
                if registering.replace(elastic).is_some() {
                    let reason = "only one pool may register agents in this version";
                    return Err(StartupError::at(pool, reason));
                }
            }
        }
    }
    let elastic = registering.is_some();
    let hosts = match (static_pool, registering) {
        (static_pool, Some(registering)) => LoadedHosts::Registering {
            static_pool,
            registering,
        },
        (Some(source), None) => LoadedHosts::Static(source),
        (None, None) => {
            let reason = "required: add at least one [[pool]] table";
            return Err(StartupError::at("pool", reason));
        }
    };
    let warnings = check_listeners(&server, elastic)?;
    Ok(Loaded {
        server,
        client_token,
        hosts,
        warnings,
    })
}

/// What the listeners and the advertised address must be, given whether a pool launches
/// hosts; the warnings the server shows.
fn check_listeners(server: &Server, elastic: bool) -> Result<Vec<String>, StartupError> {
    if elastic {
        let apart = match server.agent_listen {
            Some(agent) => agent != server.listen,
            None => false,
        };
        if !apart {
            return Err(StartupError::at(
                "server.agent_listen",
                "required with an elastic pool; must differ from server.listen",
            ));
        }
        let refused = match &server.advertise_url {
            Some(url) => plain_http(url),
            None => false,
        };
        if refused {
            return Err(StartupError::at(
                "server.advertise_url",
                "http:// is refused by agents; use https (or loopback)",
            ));
        }
    }
    let mut listeners = vec![("server.listen", server.listen)];
    if let Some(agent) = server.agent_listen {
        listeners.push(("server.agent_listen", agent));
    }
    let mut warnings = Vec::new();
    for (key, addr) in listeners {
        if server.tls.is_none() & !addr.ip().is_loopback() {
            return Err(StartupError::at(
                key,
                format!(
                    "{addr} is not a loopback address, so server.tls_cert and server.tls_key are required: a bearer token never crosses a network in clear"
                ),
            ));
        }
        if addr.ip().is_unspecified() {
            warnings.push(format!(
                "server: warning: binds all interfaces ({key} {addr})"
            ));
        }
    }
    Ok(warnings)
}

/// Whether `url` is `http://` to a host that is not loopback, which an agent refuses.
fn plain_http(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let host = uri.host().unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let address = match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    };
    let loopback = (host == "localhost") | address;
    let plain = uri.scheme_str() == Some("http");
    plain & !loopback
}

fn read_server(table: toml::Table) -> Result<Server, StartupError> {
    let mut s = Section::open(table, "server.".to_owned());
    s.only(SERVER_KEYS)?;
    let listen = s.address("listen")?;
    let listen = listen.unwrap_or(SocketAddr::from(([127, 0, 0, 1], 7300)));
    let agent_listen = s.address("agent_listen")?;
    let advertise_url = s.string("advertise_url")?;
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
    // The same check as `--default-ttl` and `--max-ttl`.
    let settings = match Settings::new(default_ttl, max_ttl) {
        Ok(settings) => settings,
        Err(err) => return Err(s.fail("default_ttl", &err.to_string())),
    };
    Ok(Server {
        listen,
        agent_listen,
        advertise_url,
        state_file,
        boot,
        client_token_file,
        tls,
        tick,
        settings,
        limits,
    })
}

fn read_client_token(server: &Server, ld: &Load<'_>) -> Result<Token, StartupError> {
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

/// A pool of the file, built.
struct Read {
    name: String,
    pool: Pool,
}

enum Pool {
    Static(Box<dyn CapacitySource>),
    Elastic(Box<Registering>),
}

/// The keys of a pool that belong to an elastic pool, as the file gave them.
struct ElasticKeys {
    exec: Option<String>,
    max_instances: Option<u64>,
    hour: Option<u64>,
    day: Option<u64>,
    idle: Option<Duration>,
    boot_timeout: Option<Duration>,
    join_token_file: Option<PathBuf>,
    network: Option<toml::Table>,
}

impl ElasticKeys {
    /// The first key a static pool was given, in the order of the file's reference.
    fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("exec", self.exec.is_some()),
            ("max_instances", self.max_instances.is_some()),
            ("max_spend_per_hour", self.hour.is_some()),
            ("max_spend_per_day", self.day.is_some()),
            ("idle", self.idle.is_some()),
            ("boot_timeout", self.boot_timeout.is_some()),
            ("join_token_file", self.join_token_file.is_some()),
            ("network", self.network.is_some()),
        ];
        for (key, set) in given {
            if set {
                return Some(key);
            }
        }
        None
    }
}

fn read_pool(
    table: toml::Table,
    number: usize,
    ld: &Load<'_>,
    client: &Token,
) -> Result<Read, StartupError> {
    // The name, when it is one, so that every refusal below names the pool; otherwise the
    // pool is its number, and the value is not repeated.
    let head = match table.get("name").and_then(toml::Value::as_str) {
        Some(name) if ProviderId::new(name).is_ok() => format!("pool {name:?}: "),
        _ => format!("pool #{number}: "),
    };
    let mut s = Section::open(table, head);
    s.only(POOL_KEYS)?;
    let name = s.required_string("name")?;
    let id = match ProviderId::new(&name) {
        Ok(id) => id,
        Err(_) => return Err(s.fail("name", "must be 1 to 63 characters of [a-z0-9._-]")),
    };
    s.head = format!("pool {name:?}: ");
    let class = s.required_string("class")?;
    let class = match ClassId::new(&class) {
        Ok(class) => class,
        Err(_) => return Err(s.fail("class", "must be 1 to 63 characters of [a-z0-9._-]")),
    };
    let class = (ld.classes)(class);
    if matches!(class.admission(), Admission::Book(_)) {
        return Err(s.fail("class", crate::agent_pool::MIXED_UNSERVED));
    }
    let keys = ElasticKeys {
        exec: s.string("exec")?,
        max_instances: s.integer("max_instances")?,
        hour: s.spend("max_spend_per_hour")?,
        day: s.spend("max_spend_per_day")?,
        idle: s.duration("idle")?,
        boot_timeout: s.duration("boot_timeout")?,
        join_token_file: s.path("join_token_file")?,
        network: s.table("network")?,
    };
    let launch = match s.string("launch")?.as_deref() {
        None | Some("plan") => Launch::Plan,
        Some("live") => Launch::Live,
        Some(_) => return Err(s.fail("launch", "must be plan or live")),
    };
    let provider = match s.table("provider")? {
        Some(provider) => provider,
        None => return Err(s.fail("provider", "required")),
    };
    let mut p = Section::open(provider, format!("pool {name:?}: provider."));
    let kind = p.required_string("kind")?;
    let built = {
        let ctx = BuildCtx {
            pool: &name,
            class: &class,
            clock: ld.clock.clone(),
            files: ld.files,
        };
        ld.kinds.build(&kind, &p.map, &ctx)
    };
    let built = match built {
        Ok(built) => built,
        Err(err) => return Err(p.fail(&err.field, &err.reason)),
    };
    let pool = match built {
        Built::Static(source) => {
            if let Some(key) = keys.first_given() {
                return Err(s.fail(key, "only for an elastic pool"));
            }
            if launch == Launch::Live {
                let reason = "live needs an elastic provider; this kind launches nothing";
                return Err(s.fail("launch", reason));
            }
            Pool::Static(source)
        }
        Built::Elastic(built) => {
            let elastic = Elastic {
                s: &s,
                name: &name,
                id,
                class,
                keys,
                launch,
                kind: &kind,
            };
            Pool::Elastic(Box::new(elastic.finish(built, client)?))
        }
    };
    Ok(Read { name, pool })
}

/// What an elastic pool is made from.
struct Elastic<'a> {
    s: &'a Section,
    name: &'a str,
    id: ProviderId,
    class: ResourceClass,
    keys: ElasticKeys,
    launch: Launch,
    kind: &'a str,
}

impl Elastic<'_> {
    fn finish(self, built: ElasticBuilt, client: &Token) -> Result<Registering, StartupError> {
        let Self {
            s,
            name,
            id,
            class,
            keys,
            launch,
            kind,
        } = self;
        let max_instances = require(s, "max_instances", keys.max_instances)?;
        if max_instances == 0 {
            return Err(s.fail("max_instances", "must be at least 1"));
        }
        if max_instances > MAX_INSTANCES {
            let reason = format!("{max_instances} exceeds {MAX_INSTANCES}");
            return Err(s.fail("max_instances", &reason));
        }
        let hour = require(s, "max_spend_per_hour", keys.hour)?;
        let day = require(s, "max_spend_per_day", keys.day)?;
        if day < hour {
            let reason = "must not be below max_spend_per_hour";
            return Err(s.fail("max_spend_per_day", reason));
        }
        let exec = match keys.exec.as_deref().unwrap_or("container") {
            "none" => None,
            other => match other.parse::<ExecMode>() {
                Ok(mode) => Some(mode),
                Err(_) => return Err(s.fail("exec", "must be container, sandbox-exec or none")),
            },
        };
        match exec {
            Some(mode) if !built.exec.contains(&mode) => {
                let reason = format!("{mode} hosts cannot be launched by provider kind {kind:?}");
                return Err(s.fail("exec", &reason));
            }
            _ => {}
        }
        let network = read_network(keys.network, format!("{}network.", s.head))?;
        let tokens = match &keys.join_token_file {
            Some(path) => match AgentTokens::open(path, client) {
                Ok(tokens) => tokens,
                Err(err) => return Err(s.fail("join_token_file", err.reason())),
            },
            None => AgentTokens::none(),
        };
        let gpu_class = built.floor.gpu_class.as_ref().map(ToString::to_string);
        let resources = match Resources::new(
            FLOOR_CPU_MILLIS,
            FLOOR_MEMORY_BYTES,
            built.floor.gpus,
            gpu_class,
        ) {
            Ok(resources) => resources,
            Err(err) => return Err(s.fail("provider", &err.to_string())),
        };
        let shape = Shape::new(resources, BTreeMap::new());
        let max = usize::try_from(max_instances).unwrap_or(usize::MAX);
        let pool = registry_pool(s, id, &class, shape, max, exec)?;
        let plan = ElasticPlan {
            name: name.to_owned(),
            launch,
            max_instances: max,
            max_spend_per_hour_cents: hour,
            max_spend_per_day_cents: day,
            idle: keys.idle,
            boot_timeout: keys.boot_timeout.unwrap_or(Duration::from_secs(900)),
            exec,
            network,
            provider: built.provider,
        };
        Ok(Registering { plan, pool, tokens })
    }
}

/// `value`, or the refusal that `key` of the pool is required.
fn require(s: &Section, key: &str, value: Option<u64>) -> Result<u64, StartupError> {
    match value {
        Some(value) => Ok(value),
        None => Err(s.fail(key, "required")),
    }
}

/// The registry's pool for an elastic pool of `class`: `max` machines, the floor `shape`,
/// the host contract of `exec` when the pool asks for one, and, for a whole-node class,
/// the settings of a dedicated pool.
fn registry_pool(
    s: &Section,
    id: ProviderId,
    class: &ResourceClass,
    shape: Shape,
    max: usize,
    exec: Option<ExecMode>,
) -> Result<PoolConfig, StartupError> {
    let pool = PoolConfig::new(id, class.clone(), shape, max);
    let pool = match exec {
        Some(mode) => pool.with_exec(mode),
        None => pool,
    };
    match class.admission() {
        Admission::Whole => match pool.with_whole_node(WholeNodeConfig::dedicated(max)) {
            Ok(pool) => Ok(pool),
            Err(err) => Err(s.fail("class", &err.to_string())),
        },
        _ => Ok(pool),
    }
}

fn read_network(table: Option<toml::Table>, head: String) -> Result<NetworkPlan, StartupError> {
    let Some(table) = table else {
        return Ok(NetworkPlan::Default);
    };
    let mut s = Section::open(table, head);
    let kind = s.required_string("kind")?;
    match kind.as_str() {
        "default" => {
            s.only(&["kind"])?;
            Ok(NetworkPlan::Default)
        }
        "tailscale" => {
            s.only(&["kind", "oauth_client_file", "tag"])?;
            let file = s.required_path("oauth_client_file")?;
            let tag = s.required_string("tag")?;
            if !valid_tag(&tag) {
                return Err(s.fail("tag", "must be \"tag:\" and letters, digits or '-'"));
            }
            match OAuthClient::from_file(&file) {
                Ok(client) => Ok(NetworkPlan::Tailscale { client, tag }),
                Err(err) => Err(s.fail("oauth_client_file", &err.to_string())),
            }
        }
        _ => {
            let reason = format!("{kind:?} is not a kind; known: default, tailscale");
            Err(s.fail("kind", &reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use js_lease::SystemClock;
    use js_testkit::dir::TestDir;
    use js_types::Dims;

    use super::*;
    use crate::kinds::OwnerOnlyFiles;

    /// Asserts that a whole-node pool of no machines is refused naming the pool and
    /// `class`, with the registry's own reason, and that a pool of machines is built. It
    /// catches a dedicated pool whose settings are dropped when the registry refuses them.
    #[test]
    fn a_dedicated_pool_of_no_machines_is_refused_naming_the_class() {
        let section = Section::open(toml::Table::new(), "pool \"x\": ".to_owned());
        let floor = Resources::new(1000, 1 << 30, 0, None).unwrap();
        let shape = Shape::new(floor, BTreeMap::new());
        let id = ProviderId::new("x").unwrap();
        let whole = ResourceClass::whole_node();
        let refused = registry_pool(&section, id.clone(), &whole, shape.clone(), 0, None);
        let said = refused.err().unwrap().to_string();
        assert!(said.starts_with("pool \"x\": class: "), "{said}");
        assert!(said.ends_with("must be at least 1"), "{said}");
        let built = registry_pool(&section, id, &whole, shape, 2, None).unwrap();
        assert_eq!(built.whole_node().unwrap().max_whole_node, 2);
    }

    /// A class that books part of a host, which no name stands for today.
    fn mixed(id: ClassId) -> ResourceClass {
        ResourceClass::new(id, Admission::Book(Dims::new()), 0)
    }

    /// Asserts that a pool whose class books part of a host is refused naming the pool
    /// and `class`, with the reason the pool file of the flags gives. No class a file can
    /// name does this today, so the test offers one through the class seam. It catches a
    /// mixed pool that is served as if it booked whole hosts.
    #[test]
    fn a_class_that_books_part_of_a_host_is_refused() {
        let dir = TestDir::under(std::env::temp_dir(), "js-file-book");
        let token = dir.path().join("token");
        std::fs::write(&token, "t".repeat(40)).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let text = format!(
            "[server]\nstate_file = \"s\"\nboot = \"resume\"\nclient_token_file = {:?}\n\
             [[pool]]\nname = \"b\"\nclass = \"mixed\"\n\
             provider = {{ kind = \"static\", hosts_file = \"/x\" }}\n",
            token.display().to_string()
        );
        let path = dir.path().join("js.toml");
        std::fs::write(&path, text).unwrap();
        let kinds = Kinds::standard();
        let load_with = Load {
            kinds: &kinds,
            files: &OwnerOnlyFiles,
            clock: SharedClock::new(SystemClock),
            classes: mixed,
        };
        let refused = load(&path, &load_with).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "pool \"b\": class: a mixed pool (a class that books part of a host) is not served by this server yet"
        );
    }
}
