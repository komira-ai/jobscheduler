//! The configuration file of `--config`: a `[server]` table, `[[pool]]` tables and the
//! `[logs]` table of live logs.
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{Artifact, CapacityProvider, Demand, SchedulerId};
use js_lease::Clock;
use js_registry::{PoolConfig, WholeNodeConfig};
use js_scaler::{Budgets, Millis};
use js_sched::admission_ever::PoolLimits;
use js_types::{
    Admission, ClassId, ExecMode, MEMORY_PERCENT_KEY, MemoryHeadroom, MemoryPercent, ProviderId,
    ResourceClass, Resources, Shape,
};

use crate::adapt::SharedClock;
use crate::agent_pool::shipped_class;
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::config::Token;
use crate::error::StartupError;
use crate::file_checks::{check_listeners, check_live};
use crate::job_expiry::JobLimits;
use crate::kinds::{BuildCtx, Built, ElasticBuilt, Floor, Kinds, SecretFiles};
use crate::network::OAuthClient;

mod brake;
mod budget;
mod joined;
mod lifetime;
mod limits;
mod logs;
mod network;
mod pins;
mod platform;
mod section;
mod server;

pub use brake::BrakePlan;
pub use joined::JoinedPool;
pub use limits::ProviderLimits;
pub use logs::{AccessKey, LogsConfig, Minting, ObjectStoreLogs};
use network::read_network;
use pins::{LIVE_NEEDS, read_agents};
pub use platform::{
    PoolSettings, Resolved, ResolvedMachineProvider, ResolvedPlatform, ResolvedPool,
};
use section::Section;
pub(crate) use server::{AlertKeys, Server, UsageKeys};
use server::{read_client_token, read_server};

/// The most instances one pool may allow: the join-token table holds 256 waiting tokens.
const MAX_INSTANCES: u64 = 255;

/// The cpu a host of an elastic pool must report at least: one core.
const FLOOR_CPU_MILLIS: u64 = 1000;

/// The memory a host of an elastic pool must report at least: 1 GiB.
const FLOOR_MEMORY_BYTES: u64 = 1 << 30;

const ROOT_KEYS: &[&str] = &[
    "server",
    "pool",
    "budget",
    "machine_provider",
    "platform",
    "logs",
];

const POOL_KEYS: &[&str] = &[
    "name",
    "class",
    "exec",
    MEMORY_PERCENT_KEY,
    "max_instances",
    "max_machines",
    "max_spend_per_hour",
    "max_spend_per_day",
    "idle",
    "max_age",
    "min_host_lifetime",
    "boot_timeout",
    "boot_fail_cooldown_max",
    "boot_fail_trip_after",
    "boot_fail_reset_after",
    "launch",
    "live_until",
    "agent",
    "join_token_file",
    "provider",
    "platforms",
    "provider_limits",
    "max_queue_wait",
    "start_timeout",
    "path_grace",
    "network",
];

/// Whether an elastic pool launches. A pool starts in `Plan`: it checks and shows its
/// plan and launches nothing. `Live` is the line that spends money; the scaler honours it,
/// and until the scaler exists nothing launches in either.
#[derive(Debug, Clone, Copy, PartialEq)]
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
        /// The Tailscale build the host downloads, with its hash. Required when the pool
        /// launches.
        binary: Option<Artifact>,
        /// The address of the Tailscale API, when the file names one (for a test or a
        /// proxy); the service's own default otherwise.
        api_url: Option<String>,
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
    /// The floor's memory, the percent of it the pool books and what that comes to.
    pub memory: MemoryHeadroom,
    /// A request for exactly the floor: what a launch is planned for while no request
    /// waits.
    pub(crate) floor_demand: Demand,
    /// Whether it launches. A pool whose `live_until` had passed when the file was read is
    /// `Plan`, whatever the file says.
    pub launch: Launch,
    /// When the go expires: from then on the pool is in plan mode ([`Settings::mode`]).
    ///
    /// [`Settings::mode`]: js_scaler::Settings::mode
    pub live_until: Option<Millis>,
    /// The server-wide totals: the `[server]` ones and those of each `[budget.<kind>]`.
    /// Every pool carries the same.
    pub budgets: Budgets,
    /// The most instances at once.
    pub max_instances: usize,
    /// The most it may spend in an hour, in cents.
    pub max_spend_per_hour_cents: u64,
    /// The most it may spend in a day, in cents.
    pub max_spend_per_day_cents: u64,
    /// How long a host may sit idle before it is released, when the file says. Required of
    /// a pool with accelerators.
    pub idle: Option<Duration>,
    /// The longest a launched machine lives: at this age its host is drained and released
    /// whether or not it is idle. Required of a pool with accelerators.
    pub max_age: Option<Duration>,
    /// The least a launched machine lives before `idle` may release it, so a short `idle`
    /// cannot make every billed boot a launch for nothing. Required of a pool with
    /// accelerators; none for any other pool unless the file says.
    pub min_host_lifetime: Duration,
    /// `server.max_lease_wall`: the ceiling on a lease's wall time, so the longest a draining
    /// host takes to empty. Required of a pool with accelerators, which is why the plan
    /// holds it.
    pub max_lease_wall: Option<Duration>,
    /// How long a launched host has to say hello.
    pub boot_timeout: Duration,
    /// The failure brake: the cooldown's ceiling, the failures in a row that trip the pool,
    /// and how long a host is heard before the count ends.
    pub brake: BrakePlan,
    /// The execution mode the hosts must report, or `None` when the pool asked for none.
    pub exec: Option<ExecMode>,
    /// How a launched host reaches the server.
    pub network: NetworkPlan,
    /// The agent the host downloads, with its hash, by CPU architecture in the one
    /// spelling (`x86_64`, `arm64`; the file may write `aarch64`, read as `arm64`). A pool
    /// that launches needs at least one; an offer of an architecture with no entry is never
    /// launched with another's.
    pub agent: BTreeMap<String, Artifact>,
    /// The id that marks what this scheduler launches (`server.scheduler_id`).
    pub scheduler: SchedulerId,
    /// The pool's name as the registry knows it.
    pub(crate) id: ProviderId,
    pub(crate) provider: Arc<dyn CapacityProvider>,
    /// How many machines the provider may hold for the pool, as admission reads it: its
    /// `provider_limits` `max_machines`, else the pool's.
    pub(crate) share: u32,
    /// The pool's `provider_limits` on the provider, as admission reads them.
    pub(crate) limits: PoolLimits,
}

impl ElasticPlan {
    /// Takes the network table out of the plan, leaving `default` in its place, for the
    /// one thing that builds the mode from it (the credential in it cannot be copied).
    pub(crate) fn take_network(&mut self) -> NetworkPlan {
        std::mem::replace(&mut self.network, NetworkPlan::Default)
    }

    /// The provider the pool launches with.
    pub fn provider(&self) -> &dyn CapacityProvider {
        self.provider.as_ref()
    }
}

impl fmt::Debug for ElasticPlan {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("ElasticPlan")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("floor", &self.floor)
            .field("memory", &self.memory)
            .field("launch", &self.launch)
            .field("live_until", &self.live_until)
            .field("budgets", &self.budgets)
            .field("max_instances", &self.max_instances)
            .field("max_spend_per_hour_cents", &self.max_spend_per_hour_cents)
            .field("max_spend_per_day_cents", &self.max_spend_per_day_cents)
            .field("idle", &self.idle)
            .field("max_age", &self.max_age)
            .field("min_host_lifetime", &self.min_host_lifetime)
            .field("max_lease_wall", &self.max_lease_wall)
            .field("boot_timeout", &self.boot_timeout)
            .field("exec", &self.exec)
            .field("network", &self.network)
            .field("agent", &self.agent)
            .field("scheduler", &self.scheduler)
            .finish_non_exhaustive()
    }
}

/// The registering pool: the plan, what the registry is built from, and the join tokens.
pub(crate) struct Registering {
    /// The launching side; `None` for a pool of hand-joined agents.
    pub(crate) plan: Option<ElasticPlan>,
    pub(crate) pool: PoolConfig,
    pub(crate) tokens: AgentTokens,
    /// The one `provider` the mint route accepts, when the pool is the only thing that
    /// admits hosts by hand: a joined pool's name. `None` takes any provider, as an
    /// elastic pool's launches are minted for under the provider kind's name.
    pub(crate) mint_provider: Option<String>,
    /// The pool's `max_queue_wait` and `start_timeout`: an elastic pool's settings, and the
    /// defaults for a joined pool, which has no keys for them.
    pub(crate) job_limits: JobLimits,
}

impl Registering {
    /// Gives the launching side the share and the limits admission reads, from the pool's
    /// `provider_limits` (`limits`; a pool has one launching provider and a limit is refused
    /// for any other, so it holds that provider's limits or none) and its `max_machines`.
    fn admit(&mut self, limits: &BTreeMap<String, ProviderLimits>, max_machines: Option<u64>) {
        let limit = limits.values().next().cloned().unwrap_or_default();
        let share = limit.max_machines.or(max_machines).unwrap_or_default();
        self.plan.iter_mut().for_each(|plan| {
            plan.share = u32::try_from(share).unwrap_or(u32::MAX);
            plan.limits = PoolLimits {
                allowed_instance_types: limit.allowed_instance_types.clone(),
                max_price_cents_per_hour: limit.max_price_cents_per_hour,
            };
        });
    }
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
    /// Where live logs go: the `[logs]` table, or no sink.
    pub(crate) logs: LogsConfig,
    /// The machine providers, platforms and pools, however each pool was written.
    pub(crate) resolved: Resolved,
    /// Why the server cannot start on the file although it is valid: it has more than one
    /// pool that registers agents, which the fixed order of this version cannot run.
    pub(crate) unrunnable: Option<StartupError>,
}

/// What reading the file needs besides its path.
pub(crate) struct Load<'a> {
    pub(crate) kinds: &'a Kinds,
    pub(crate) files: &'a dyn SecretFiles,
    pub(crate) clock: SharedClock,
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
    let budget = root.table("budget")?;
    let machine_providers = root.tables("machine_provider")?;
    let platforms = root.tables("platform")?;
    let logs = root.table("logs")?;
    let server = read_server(server)?;
    let budgets = budget::read_budgets(budget, server.totals, ld.kinds)?;
    let client_token = read_client_token(&server, ld)?;
    let logs = logs::read_logs(logs, ld.files)?;
    let mut resolved = platform::declared(machine_providers, platforms, ld.kinds)?;
    let mut static_pool = None;
    let mut registering = Vec::new();
    let mut names = BTreeSet::new();
    for (index, table) in pools.into_iter().enumerate() {
        let ceiling = server.max_lease_wall;
        let read = read_pool(
            table,
            index + 1,
            ld,
            (&client_token, &server.scheduler, ceiling, &budgets),
            &mut resolved,
        )?;
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
            Pool::Elastic(elastic) => registering.push((pool, elastic)),
            Pool::Hostless => {}
        }
    }
    resolved.check_named()?;
    let elastic = !registering.is_empty();
    // A joined pool has no plan, so only an elastic pool's launch settings are checked.
    for plan in registering
        .iter()
        .filter_map(|(_, pool)| pool.plan.as_ref())
    {
        check_live(&server, plan)?;
    }
    let unrunnable = unrunnable(&registering);
    let registering = registering.into_iter().next().map(|(_, pool)| pool);
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
    let mut warnings = check_listeners(&server, elastic)?;
    let now = Millis::new(ld.clock.now().millis());
    warnings.extend(budget::notes(&hosts, &budgets, now));
    Ok(Loaded {
        server,
        client_token,
        hosts,
        warnings,
        logs,
        resolved,
        unrunnable,
    })
}

/// Why the server cannot run the registering pools `registering` (each with its message
/// head, `pool "<name>"`): this version runs one, through the fixed order of `layered.rs`.
/// The refusal names the second pool.
fn unrunnable(registering: &[(String, Box<Registering>)]) -> Option<StartupError> {
    let (second, _) = registering.get(1)?;
    let elastic = registering.iter().filter(|(_, pool)| pool.plan.is_some());
    let reason = match elastic.count() {
        0 | 1 => SEVERAL_REGISTERING,
        _ => SEVERAL_ELASTIC,
    };
    Some(StartupError::at(second, reason))
}

/// Why a file with two elastic pools does not start.
const SEVERAL_ELASTIC: &str =
    "several elastic pools are parsed; running them needs the ordered walk of design 5.6";

/// Why a file with two pools that register agents, not both elastic, does not start.
const SEVERAL_REGISTERING: &str =
    "several pools register agents; running them needs the ordered walk of design 5.6";

/// A pool of the file, built.
struct Read {
    name: String,
    pool: Pool,
}

enum Pool {
    Static(Box<dyn CapacitySource>),
    Elastic(Box<Registering>),
    /// A pool that names only hostless platforms: in [`Resolved`], and nothing to run.
    Hostless,
}

/// The keys of a pool that belong to an elastic pool, as the file gave them.
struct ElasticKeys {
    exec: Option<String>,
    /// The cap on machines, by either of its names.
    max_instances: Option<u64>,
    /// The name the cap was given by: `max_machines`, or `max_instances` (also when it was
    /// not given).
    cap_key: &'static str,
    /// The first of the queue settings the file gave.
    settings_given: Option<&'static str>,
    provider_limits: Option<toml::Table>,
    hour: Option<u64>,
    day: Option<u64>,
    idle: Option<Duration>,
    max_age: Option<Duration>,
    min_host_lifetime: Option<Duration>,
    live_until: Option<Millis>,
    boot_timeout: Option<Duration>,
    brake: brake::Given,
    join_token_file: Option<PathBuf>,
    network: Option<toml::Table>,
    agent: Option<toml::Table>,
}

impl ElasticKeys {
    /// The first key a static pool was given, in the order of the file's reference.
    fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("exec", self.exec.is_some()),
            (self.cap_key, self.max_instances.is_some()),
            ("max_spend_per_hour", self.hour.is_some()),
            ("max_spend_per_day", self.day.is_some()),
            ("idle", self.idle.is_some()),
            ("max_age", self.max_age.is_some()),
            ("min_host_lifetime", self.min_host_lifetime.is_some()),
            ("live_until", self.live_until.is_some()),
            ("boot_timeout", self.boot_timeout.is_some()),
            ("join_token_file", self.join_token_file.is_some()),
            ("network", self.network.is_some()),
            ("agent", self.agent.is_some()),
            ("provider_limits", self.provider_limits.is_some()),
        ];
        for (key, set) in given {
            if set {
                return Some(key);
            }
        }
        self.settings_given.or_else(|| self.brake.first_given())
    }
}

fn read_pool(
    table: toml::Table,
    number: usize,
    ld: &Load<'_>,
    (client, scheduler, max_lease_wall, budgets): (
        &Token,
        &SchedulerId,
        Option<Duration>,
        &Budgets,
    ),
    resolved: &mut Resolved,
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
    // The platforms first: a pool that names only hostless platforms has other keys.
    let reach = match s.names("platforms")? {
        Some(names) => Some(resolved.reach(&s, &names)?),
        None => None,
    };
    let reach = match reach {
        Some(reach) if !reach.hosts => {
            let name = platform::hostless(s, reach, resolved)?;
            return Ok(Read {
                name,
                pool: Pool::Hostless,
            });
        }
        reach => reach,
    };
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
    let (cap_key, max_instances) = cap(&mut s)?;
    let (settings, settings_given) = platform::settings(&mut s)?;
    let mut keys = ElasticKeys {
        exec: s.string("exec")?,
        max_instances,
        cap_key,
        settings_given,
        provider_limits: s.table("provider_limits")?,
        hour: s.spend("max_spend_per_hour")?,
        day: s.spend("max_spend_per_day")?,
        idle: s.duration("idle")?,
        max_age: s.duration("max_age")?,
        min_host_lifetime: s.duration("min_host_lifetime")?,
        live_until: s.timestamp("live_until")?,
        boot_timeout: s.duration("boot_timeout")?,
        brake: brake::Given::read(&mut s)?,
        join_token_file: s.path("join_token_file")?,
        network: s.table("network")?,
        agent: s.table("agent")?,
    };
    let memory_percent = s.memory_percent()?;
    let launch = match s.string("launch")?.as_deref() {
        None | Some("plan") => Launch::Plan,
        Some("live") => Launch::Live,
        Some(_) => return Err(s.fail("launch", "must be plan or live")),
    };
    // A go that has expired by the time the file is read is not a go: the pool starts in
    // plan, as it would have been switched there by the scaler's first step.
    let now = Millis::new(ld.clock.now().millis());
    let launch = match (launch, keys.live_until) {
        (Launch::Live, Some(until)) if now >= until => Launch::Plan,
        (launch, _) => launch,
    };
    // The provider table to build: the pool's own, or the one launching machine provider
    // its platforms reach.
    let (kind, p) = match (s.table("provider")?, &reach) {
        (Some(_), Some(_)) => {
            let reason = "and platforms both say where the pool's hosts come from; give one";
            return Err(s.fail("provider", reason));
        }
        (None, None) => return Err(s.fail("provider", "required")),
        (Some(provider), None) => {
            let mut p = Section::open(provider, format!("pool {name:?}: provider."));
            (p.required_string("kind")?, p)
        }
        (None, Some(reach)) => {
            let launching = resolved.launching(&s, reach)?;
            let head = format!("machine_provider {:?}: ", launching.name);
            let p = Section::open(launching.table().clone(), head);
            (launching.kind.clone(), p)
        }
    };
    let built = {
        let ctx = BuildCtx {
            pool: &name,
            class: &class,
            memory_percent,
            clock: ld.clock.clone(),
            files: ld.files,
            scheduler,
            max_instances: keys.max_instances.unwrap_or_default(),
            launch,
        };
        ld.kinds.build(&kind, &p.map, &ctx)
    };
    let built = match built {
        Ok(built) => built,
        Err(err) => return Err(p.fail(&err.field, &err.reason)),
    };
    let pool = match built {
        Built::Static(_) if reach.is_some() => {
            let reason = "builds a pool of listed hosts, not a machine provider";
            return Err(p.fail("kind", reason));
        }
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
            let limits = keys.provider_limits.take();
            let elastic = Elastic {
                s: &s,
                name: &name,
                id,
                class,
                memory_percent: memory_percent.unwrap_or_default(),
                keys,
                launch,
                kind: &kind,
                scheduler,
                max_lease_wall,
                budgets,
                path_grace: settings.path_grace,
            };
            let mut registering = elastic.finish(built, client)?;
            registering.job_limits = JobLimits::of(&settings);
            let reach = match reach {
                Some(reach) => reach,
                None => resolved.shorthand(&name, &kind, &p.map),
            };
            let provider_limits =
                resolved.limits(limits, &s.head, &reach.providers, max_instances)?;
            registering.admit(&provider_limits, max_instances);
            resolved.pools.push(ResolvedPool {
                name: name.clone(),
                platforms: reach.platforms,
                max_machines: max_instances,
                settings,
                provider_limits,
            });
            Pool::Elastic(Box::new(registering))
        }
    };
    Ok(Read { name, pool })
}

/// The pool's cap on machines: `max_machines`, or `max_instances`, the same number by its
/// older name, and the name it was given by; never both.
fn cap(s: &mut Section) -> Result<(&'static str, Option<u64>), StartupError> {
    let machines = s.integer("max_machines")?;
    let instances = s.integer("max_instances")?;
    match (machines, instances) {
        (Some(_), Some(_)) => {
            let reason = "and max_instances are one cap by two names; give one";
            Err(s.fail("max_machines", reason))
        }
        (Some(cap), None) => Ok(("max_machines", Some(cap))),
        (None, given) => Ok(("max_instances", given)),
    }
}

/// What an elastic pool is made from.
struct Elastic<'a> {
    s: &'a Section,
    name: &'a str,
    id: ProviderId,
    class: ResourceClass,
    memory_percent: MemoryPercent,
    keys: ElasticKeys,
    launch: Launch,
    kind: &'a str,
    scheduler: &'a SchedulerId,
    max_lease_wall: Option<Duration>,
    budgets: &'a Budgets,
    /// The pool's `path_grace`: how long a waiting whole-node request with no capable host
    /// left keeps its place (the registry's `regain_grace`).
    path_grace: Duration,
}

impl Elastic<'_> {
    fn finish(self, built: ElasticBuilt, client: &Token) -> Result<Registering, StartupError> {
        let Self {
            s,
            name,
            id,
            class,
            memory_percent,
            keys,
            launch,
            kind,
            scheduler,
            max_lease_wall,
            budgets,
            path_grace,
        } = self;
        let max_instances = require(s, keys.cap_key, keys.max_instances)?;
        if max_instances == 0 {
            return Err(s.fail(keys.cap_key, "must be at least 1"));
        }
        if max_instances > MAX_INSTANCES {
            let reason = format!("{max_instances} exceeds {MAX_INSTANCES}");
            return Err(s.fail(keys.cap_key, &reason));
        }
        let hour = require(s, "max_spend_per_hour", keys.hour)?;
        let day = require(s, "max_spend_per_day", keys.day)?;
        if day < hour {
            let reason = "must not be below max_spend_per_hour";
            return Err(s.fail("max_spend_per_day", reason));
        }
        let boot_timeout = keys.boot_timeout.unwrap_or(Duration::from_secs(900));
        let given = lifetime::Given {
            idle: keys.idle,
            max_age: keys.max_age,
            min_host_lifetime: keys.min_host_lifetime,
        };
        let brake = keys.brake.check(s)?;
        let accelerators = built.floor.gpus > 0;
        let (max_age, min_host_lifetime) =
            lifetime::check(s, &given, (accelerators, boot_timeout, max_lease_wall))?;
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
        let agent = match keys.agent {
            Some(table) => read_agents(table, format!("{}agent.", s.head))?,
            None => BTreeMap::new(),
        };
        let live = launch == Launch::Live;
        let unpinned = match &network {
            NetworkPlan::Tailscale { binary: None, .. } => live,
            _ => false,
        };
        if unpinned {
            return Err(s.fail("network.binary", LIVE_NEEDS));
        }
        if live & agent.is_empty() {
            return Err(s.fail("agent", LIVE_NEEDS));
        }
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
            isa_level: None,
            exec,
            accelerator: built.floor.need(),
            network: Default::default(),
            host_ports: Vec::new(),
        };
        let shape = Shape::new(resources, BTreeMap::new());
        let max = usize::try_from(max_instances).unwrap_or(usize::MAX);
        let pool = registry_pool(
            s,
            (id.clone(), &class, shape, max),
            exec,
            memory_percent,
            path_grace,
        )?;
        let plan = ElasticPlan {
            name: name.to_owned(),
            kind: kind.to_owned(),
            floor: built.floor,
            memory: pool.memory_headroom(),
            floor_demand,
            launch,
            live_until: keys.live_until,
            budgets: budgets.clone(),
            max_instances: max,
            max_spend_per_hour_cents: hour,
            max_spend_per_day_cents: day,
            idle: keys.idle,
            max_age,
            min_host_lifetime,
            max_lease_wall,
            boot_timeout,
            brake,
            exec,
            network,
            agent,
            scheduler: scheduler.clone(),
            id,
            provider: Arc::from(built.provider),
            share: u32::try_from(max).unwrap_or(u32::MAX),
            limits: PoolLimits::default(),
        };
        Ok(Registering {
            plan: Some(plan),
            pool,
            tokens,
            mint_provider: None,
            job_limits: JobLimits::default(),
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
/// the host contract of `exec` when the pool asks for one, the booked share of the floor's
/// memory, and, for a whole-node class, the settings of a dedicated pool with `path_grace`
/// as its `regain_grace`: how long a waiting request whose last capable host went away
/// keeps its place before it is refused `no-capable-host`.
fn registry_pool(
    s: &Section,
    (id, class, shape, max): (ProviderId, &ResourceClass, Shape, usize),
    exec: Option<ExecMode>,
    memory_percent: MemoryPercent,
    path_grace: Duration,
) -> Result<PoolConfig, StartupError> {
    let pool = PoolConfig::new(id, class.clone(), shape, max).with_memory_percent(memory_percent);
    let pool = match exec {
        Some(mode) => pool.with_exec(mode),
        None => pool,
    };
    match class.admission() {
        Admission::Whole => match pool.with_whole_node(WholeNodeConfig {
            regain_grace_ms: u64::try_from(path_grace.as_millis()).unwrap_or(u64::MAX),
            ..WholeNodeConfig::dedicated(max)
        }) {
            Ok(pool) => Ok(pool),
            Err(err) => Err(s.fail("class", &err.to_string())),
        },
        _ => Ok(pool),
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
        let full = MemoryPercent::FULL;
        let whole = ResourceClass::whole_node();
        let grace = Duration::from_millis(250);
        let at = |max| (id.clone(), &whole, floor(), max);
        let refused = registry_pool(&section(), at(0), None, full, grace);
        let said = refused.err().unwrap().to_string();
        assert!(said.starts_with("pool \"x\": class: "), "{said}");
        assert!(said.ends_with("must be at least 1"), "{said}");
        let exec = Some(ExecMode::Container);
        let built = registry_pool(&section(), at(2), exec, full, grace).unwrap();
        assert_eq!(built.whole_node().unwrap().max_whole_node, 2);
        assert_eq!(built.whole_node().unwrap().regain_grace_ms, 250);
        assert_eq!(built.exec(), exec);
        let gpu = ResourceClass::gpu_host();
        let plain = registry_pool(&section(), (id, &gpu, floor(), 2), None, full, grace).unwrap();
        assert!(plain.whole_node().is_none());
        assert_eq!(plain.exec(), None);
    }
}
