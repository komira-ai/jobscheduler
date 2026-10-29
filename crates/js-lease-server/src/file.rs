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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{Demand, ElasticProvider};
use js_registry::{PoolConfig, WholeNodeConfig};
use js_types::{Admission, ClassId, ExecMode, ProviderId, ResourceClass, Resources, Shape};

use crate::adapt::SharedClock;
use crate::agent_pool::{MIXED_UNSERVED, shipped_class};
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::config::{Token, parse_duration};
use crate::error::StartupError;
use crate::kinds::{BuildCtx, Built, ElasticBuilt, Floor, Kinds, SecretFiles};
use crate::network::{OAuthClient, valid_tag};

mod joined;
mod server;

pub use joined::JoinedPool;
pub(crate) use server::Server;
use server::{check_listeners, read_client_token, read_server};

/// The most instances one pool may allow: the join-token table holds 256 waiting tokens.
const MAX_INSTANCES: u64 = 255;

/// The cpu a host of an elastic pool must report at least: one core.
const FLOOR_CPU_MILLIS: u64 = 1000;

/// The memory a host of an elastic pool must report at least: 1 GiB.
const FLOOR_MEMORY_BYTES: u64 = 1 << 30;

const ROOT_KEYS: &[&str] = &["server", "pool"];

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

impl Launch {
    /// The word the file spells it with.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Live => "live",
        }
    }
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

impl NetworkPlan {
    /// The word the file spells the mode's kind with.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Tailscale { .. } => "tailscale",
        }
    }
}

/// An elastic pool, as the file describes it.
pub struct ElasticPlan {
    /// The pool's name.
    pub name: String,
    /// The provider kind the pool names, such as `lambdalabs`.
    pub kind: String,
    /// The least the pool's sizes bring, as far as the kind knows it offline.
    pub floor: Floor,
    /// A request for exactly the floor: what a launch is planned for while no request
    /// waits.
    pub(crate) floor_demand: Demand,
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
    pub(crate) provider: Arc<dyn ElasticProvider>,
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
            .field("kind", &self.kind)
            .field("floor", &self.floor)
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

/// The registering pool: the plan, what the registry is built from, and the join tokens.
pub(crate) struct Registering {
    /// The launching side; `None` for a pool of hand-joined agents.
    pub(crate) plan: Option<ElasticPlan>,
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

    /// Refuses a class that books part of a host: this server serves whole hosts only.
    fn class_served(&self, class: &ResourceClass) -> Result<(), StartupError> {
        match class.admission() {
            Admission::Book(_) => Err(self.fail("class", MIXED_UNSERVED)),
            _ => Ok(()),
        }
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

    /// The integer `key` when the file gives it, which must be from `low` to `high`.
    fn bounded_given(
        &mut self,
        key: &str,
        low: u64,
        high: u64,
    ) -> Result<Option<u64>, StartupError> {
        let Some(number) = self.integer(key)? else {
            return Ok(None);
        };
        match (low..=high).contains(&number) {
            true => Ok(Some(number)),
            false => Err(self.fail(key, &format!("must be from {low} to {high}"))),
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
        Ok(self.bounded_given(key, low, high)?.unwrap_or(default))
    }

    fn boolean(&mut self, key: &str) -> Result<Option<bool>, StartupError> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(toml::Value::Boolean(flag)) => Ok(Some(flag)),
            Some(_) => Err(self.fail(key, "must be true or false")),
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
    if let (false, Some(key)) = (elastic, server.agents.first_given()) {
        let reason = "needs an elastic pool, whose hosts register agents";
        return Err(StartupError::at(format!("server.{key}"), reason));
    }
    let warnings = check_listeners(&server, elastic)?;
    Ok(Loaded {
        server,
        client_token,
        hosts,
        warnings,
    })
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
    if table.contains_key("kind") {
        return joined::read(table, client);
    }
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
    let class = shipped_class(class);
    s.class_served(&class)?;
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
        let floor_demand = Demand {
            resources: resources.clone(),
            disk_bytes: None,
            os: None,
            arch: None,
            exec,
        };
        let shape = Shape::new(resources, BTreeMap::new());
        let max = usize::try_from(max_instances).unwrap_or(usize::MAX);
        let pool = registry_pool(s, id, &class, shape, max, exec)?;
        let plan = ElasticPlan {
            name: name.to_owned(),
            kind: kind.to_owned(),
            floor: built.floor,
            floor_demand,
            launch,
            max_instances: max,
            max_spend_per_hour_cents: hour,
            max_spend_per_day_cents: day,
            idle: keys.idle,
            boot_timeout: keys.boot_timeout.unwrap_or(Duration::from_secs(900)),
            exec,
            network,
            provider: Arc::from(built.provider),
        };
        Ok(Registering {
            plan: Some(plan),
            pool,
            tokens,
        })
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
    use js_types::Dims;

    use super::*;

    fn section() -> Section {
        Section::open(toml::Table::new(), "pool \"x\": ".to_owned())
    }

    fn floor() -> Shape {
        let floor = Resources::new(1000, 1 << 30, 0, None).unwrap();
        Shape::new(floor, BTreeMap::new())
    }

    /// Asserts that a class that books part of a host is refused naming the pool and
    /// `class`, with the reason the pool file of the flags gives, and that a class that
    /// books whole hosts is served. No class a file can name books part of a host today,
    /// so the check is tried on a class built here. It catches a mixed pool that is served
    /// as if it booked whole hosts.
    #[test]
    fn a_class_that_books_part_of_a_host_is_refused() {
        let mixed = ResourceClass::new(
            ClassId::new("mixed").unwrap(),
            Admission::Book(Dims::new()),
            0,
        );
        let refused = section().class_served(&mixed).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "pool \"x\": class: a mixed pool (a class that books part of a host) is not served by this server yet"
        );
        assert!(section().class_served(&ResourceClass::whole_node()).is_ok());
        assert!(section().class_served(&ResourceClass::gpu_host()).is_ok());
    }

    /// Asserts how the registry's pool is built: a whole-node pool of no machines is
    /// refused naming the pool and `class` with the registry's own reason; a whole-node
    /// pool of machines gets the dedicated settings; a pool of another class gets none; and
    /// the host contract is set when asked for and absent when not. It catches a dedicated
    /// pool whose settings are dropped when the registry refuses them.
    #[test]
    fn the_registry_pool_takes_the_class_and_the_contract() {
        let id = ProviderId::new("x").unwrap();
        let whole = ResourceClass::whole_node();
        let refused = registry_pool(&section(), id.clone(), &whole, floor(), 0, None);
        let said = refused.err().unwrap().to_string();
        assert!(said.starts_with("pool \"x\": class: "), "{said}");
        assert!(said.ends_with("must be at least 1"), "{said}");
        let exec = Some(ExecMode::Container);
        let built = registry_pool(&section(), id.clone(), &whole, floor(), 2, exec).unwrap();
        assert_eq!(built.whole_node().unwrap().max_whole_node, 2);
        assert_eq!(built.exec(), exec);
        let gpu = ResourceClass::gpu_host();
        let plain = registry_pool(&section(), id, &gpu, floor(), 2, None).unwrap();
        assert!(plain.whole_node().is_none());
        assert_eq!(plain.exec(), None);
    }
}
