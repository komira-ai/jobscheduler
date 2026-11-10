//! The `[[machine_provider]]` and `[[platform]]` tables, a pool's `platforms`, and the one
//! structure both spellings of a pool resolve to (design 3.5 and 4.2).
//!
//! A machine provider supplies machines that run `js-agent`: `static` is the hosts that
//! register themselves, and any other kind is a provider kind compiled in (such as
//! `lambdalabs`), whose own keys are handed to its constructor when a pool is built. A
//! platform of kind `default` names machine providers in priority order; a pool names
//! platforms in priority order.
//!
//! A pool that names its provider directly (`provider = { kind = "lambdalabs", ... }`)
//! resolves to exactly what the explicit tables would give: a machine provider made from
//! the table, the `static` machine provider, and a `default` platform that names the two,
//! `static` first. The names the loader makes start with [`MADE`], which no name in the
//! file may hold, so they cannot collide with the operator's. A pool whose provider is the
//! agentless `static` kind is not resolved: it is built as before, and goes away with that
//! kind.
//!
//! Everything downstream reads [`Resolved`]; the job pass places on the hosts of a pool's
//! machine providers in the order its platforms, and each platform's `machine_providers`,
//! list them (`crate::sched::rank::order_of`), in any order. A pool reaches exactly one
//! launching machine provider.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use js_platform::PlatformId;
use js_types::{MEMORY_PERCENT_KEY, ProviderId};

use super::Section;
use super::limits::{self, ProviderLimits, Reached};
use crate::error::StartupError;
use crate::job_expiry::JobLimits;
use crate::kinds::{Kinds, PlatformKind};

/// The first character of a name the loader makes; the grammar of a name in the file has
/// no `+`.
const MADE: char = '+';

/// The machine provider kind of the hosts that register themselves.
const STATIC: &str = "static";

/// Platform kinds that are planned and not built.
const UNBUILT_PLATFORMS: &[&str] = &["kubernetes", "cloud_run", "ecs"];

const PLATFORM_KEYS: &[&str] = &["name", "kind", "machine_providers"];

const HOSTLESS_PLATFORM_KEYS: &[&str] = &["name", "kind"];

/// The keys of a pool that describe hosts, refused on a pool that names only hostless
/// platforms.
const HOST_KEYS: &[&str] = &[
    "shape",
    "max_machines",
    "max_instances",
    "join_token_file",
    MEMORY_PERCENT_KEY,
    "max_queue_wait",
    "start_timeout",
    "provider_limits",
];

/// The keys of a pool that names only hostless platforms.
const HOSTLESS_POOL_KEYS: &[&str] = &["name", "platforms", "path_grace"];

const DESCRIBES_HOSTS: &str = "describes hosts; the pool names only hostless platforms";

/// Why a pool that reaches no launching machine provider, or more than one, is refused.
const ONE_LAUNCHING: &str = "a pool's platforms reach exactly one machine provider that launches in this version; a pool of registered hosts alone is kind = \"joined\"";

const NAME_RULE: &str = "must be 1 to 63 characters of [a-z0-9._-]";

/// How long a queued job waits for a host before it ends as `no-capacity`, unless the pool
/// says.
const MAX_QUEUE_WAIT: Duration = JobLimits::DEFAULT.max_queue_wait;

/// How long a placed or starting job has to start before it ends as `start-timeout`,
/// unless the pool says.
const START_TIMEOUT: Duration = JobLimits::DEFAULT.start_timeout;

/// How long a queued job whose last capable path went away stays queued before it ends as
/// `no-capable-host`, unless the pool says.
pub(super) const PATH_GRACE: Duration = Duration::from_secs(15);

/// A machine provider: one `[[machine_provider]]` table, or one the loader made for a pool
/// that names its provider directly.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ResolvedMachineProvider {
    /// Its name.
    pub name: String,
    /// Its kind: `static`, or a provider kind compiled in.
    pub kind: String,
    /// The kind's own keys: the table without `name` and `kind`, handed to the kind's
    /// constructor when a pool is built. Empty for `static`.
    table: toml::Table,
}

impl ResolvedMachineProvider {
    /// Whether it launches machines: every kind but `static` does.
    pub fn launches(&self) -> bool {
        self.kind != STATIC
    }

    pub(super) fn table(&self) -> &toml::Table {
        &self.table
    }
}

/// A container platform: one `[[platform]]` table, or the `default` platform the loader
/// made for a pool that names its provider directly.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ResolvedPlatform {
    /// Its name.
    pub id: PlatformId,
    /// Its kind.
    pub kind: PlatformKind,
    /// Its machine providers, by name, first to last; none for a hostless kind.
    pub machine_providers: Vec<String>,
}

/// The settings of a pool about how long a job may wait. Read and checked at start; the job
/// pass of the tick enforces `max_queue_wait` and `start_timeout` on the jobs of the server's
/// job tables, and `path_grace` is the registry's `regain_grace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolSettings {
    /// How long a queued job waits for a host before it ends as `no-capacity` (default 1h),
    /// counted from when it was queued.
    pub max_queue_wait: Duration,
    /// How long a placed or starting job has to start before it ends as `start-timeout`
    /// (default 15m), counted from its placement.
    pub start_timeout: Duration,
    /// How long a queued job whose last capable path went away stays queued before it ends
    /// as `no-capable-host` (default 15s): the registry's `regain_grace` for the pool's
    /// whole-node requests, which are the requests that wait.
    pub path_grace: Duration,
}

/// A pool of the platform model, resolved.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ResolvedPool {
    /// Its name.
    pub name: String,
    /// Its platforms, first to last.
    pub platforms: Vec<PlatformId>,
    /// The most machines it has, every provider counted: `max_machines`, or
    /// `max_instances`, which is the same number. `None` for a pool of hostless platforms.
    pub max_machines: Option<u64>,
    /// How long a job may wait.
    pub settings: PoolSettings,
    /// What each launching machine provider of its platforms may launch for it, by name.
    pub provider_limits: BTreeMap<String, ProviderLimits>,
}

/// The machine providers, platforms and pools of a configuration file, whichever way each
/// pool was written. A pool whose provider is the agentless `static` kind and a `joined`
/// pool are not in it.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Resolved {
    /// The machine providers: those the file declares, in its order, then those the loader
    /// made, in the order of the pools that needed them.
    pub machine_providers: Vec<ResolvedMachineProvider>,
    /// The platforms, in the same order.
    pub platforms: Vec<ResolvedPlatform>,
    /// The pools, in the file's order.
    pub pools: Vec<ResolvedPool>,
}

impl Resolved {
    /// The pool called `name`, when it is resolved.
    pub fn pool(&self, name: &str) -> Option<&ResolvedPool> {
        self.pools.iter().find(|pool| pool.name == name)
    }

    /// One line per machine provider, platform, pool and pool limit: names, kinds, the
    /// names of a provider's own keys, order, durations and limits. It holds no value of a
    /// provider's keys, so no path and nothing a secret file holds.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for provider in &self.machine_providers {
            let keys: Vec<&str> = provider.table.keys().map(String::as_str).collect();
            let _ = writeln!(
                out,
                "machine_provider {}: kind {}; keys {}",
                provider.name,
                provider.kind,
                listed(&keys)
            );
        }
        for platform in &self.platforms {
            let hosts = match platform.kind.has_hosts() {
                true => "yes",
                false => "no",
            };
            let _ = writeln!(
                out,
                "platform {}: kind {}; hosts {hosts}; machine_providers {}",
                platform.id,
                platform.kind.name(),
                listed(&platform.machine_providers)
            );
        }
        for pool in &self.pools {
            let settings = pool.settings;
            let _ = writeln!(
                out,
                "pool {}: platforms {}; max_machines {}; max_queue_wait {}; start_timeout {}; path_grace {}",
                pool.name,
                listed(&pool.platforms),
                shown(pool.max_machines),
                span(settings.max_queue_wait),
                span(settings.start_timeout),
                span(settings.path_grace)
            );
            for (name, limit) in &pool.provider_limits {
                let types = limit.allowed_instance_types.as_deref().unwrap_or_default();
                let _ = writeln!(
                    out,
                    "pool {}: provider_limits {name}: allowed_instance_types {}; max_price_cents_per_hour {}; max_machines {}; max_spot_machines {}; quota_backoff {}; acquire_retry {}; catalog_refresh {}; offer_cooldown {}; size_tolerance_percent {}",
                    pool.name,
                    listed(types),
                    shown(limit.max_price_cents_per_hour),
                    shown(limit.max_machines),
                    shown(limit.max_spot_machines),
                    span(limit.quota_backoff),
                    span(limit.acquire_retry),
                    span(limit.catalog_refresh),
                    span(limit.offer_cooldown),
                    limit.size_tolerance_percent
                );
            }
        }
        out
    }

    fn provider(&self, name: &str) -> Option<&ResolvedMachineProvider> {
        self.machine_providers.iter().find(|p| p.name == name)
    }

    fn platform(&self, name: &str) -> Option<&ResolvedPlatform> {
        self.platforms.iter().find(|p| p.id.as_str() == name)
    }

    /// The machine provider of the shorthand `provider` table `table` of kind `kind` for
    /// `pool`, the `static` machine provider, and the `default` platform naming the two:
    /// made once, and a provider made from an equal table is used again. What the pool
    /// reaches through the platform.
    pub(super) fn shorthand(&mut self, pool: &str, kind: &str, table: &toml::Table) -> Reach {
        let registered = format!("{MADE}{STATIC}");
        if self.provider(&registered).is_none() {
            self.machine_providers.push(ResolvedMachineProvider {
                name: registered.clone(),
                kind: STATIC.to_owned(),
                table: toml::Table::new(),
            });
        }
        let same = self.machine_providers.iter().find(|p| {
            let made = p.name.starts_with(MADE);
            made & (p.kind == kind) & (&p.table == table)
        });
        let launching = match same {
            Some(provider) => provider.name.clone(),
            None => {
                let name = format!("{MADE}{pool}.{kind}");
                self.machine_providers.push(ResolvedMachineProvider {
                    name: name.clone(),
                    kind: kind.to_owned(),
                    table: table.clone(),
                });
                name
            }
        };
        let id = PlatformId::new(format!("{MADE}{pool}"));
        let providers = vec![registered, launching];
        self.platforms.push(ResolvedPlatform {
            id: id.clone(),
            kind: PlatformKind::DEFAULT,
            machine_providers: providers.clone(),
        });
        Reach {
            platforms: vec![id],
            providers,
            hosts: true,
        }
    }

    /// What the platforms `names` of a pool reach, refusing a name that is not a declared
    /// platform. `s` is the pool's table, for the messages.
    pub(super) fn reach(&self, s: &Section, names: &[String]) -> Result<Reach, StartupError> {
        let mut reach = Reach {
            platforms: Vec::new(),
            providers: Vec::new(),
            hosts: false,
        };
        for name in names {
            let Some(platform) = self.platform(name) else {
                let reason = format!("{name:?} is not a declared platform");
                return Err(s.fail("platforms", &reason));
            };
            reach.platforms.push(platform.id.clone());
            reach.hosts |= platform.kind.has_hosts();
            for provider in &platform.machine_providers {
                if !reach.providers.contains(provider) {
                    reach.providers.push(provider.clone());
                }
            }
        }
        Ok(reach)
    }

    /// The one launching machine provider `reach` names, refused unless it is exactly one.
    pub(super) fn launching(
        &self,
        s: &Section,
        reach: &Reach,
    ) -> Result<&ResolvedMachineProvider, StartupError> {
        let mut launching = reach
            .providers
            .iter()
            .filter_map(|name| self.provider(name))
            .filter(|provider| provider.launches());
        match (launching.next(), launching.next()) {
            (Some(provider), None) => Ok(provider),
            _ => Err(s.fail("platforms", ONE_LAUNCHING)),
        }
    }

    /// The pool's `provider_limits` table, checked against what `reach` names.
    pub(super) fn limits(
        &self,
        table: Option<toml::Table>,
        head: &str,
        reach: &[String],
        cap: Option<u64>,
    ) -> Result<BTreeMap<String, ProviderLimits>, StartupError> {
        let Some(table) = table else {
            return Ok(BTreeMap::new());
        };
        let reached: Vec<Reached<'_>> = reach
            .iter()
            .filter_map(|name| self.provider(name))
            .map(|provider| Reached {
                name: &provider.name,
                launches: provider.launches(),
            })
            .collect();
        limits::read(table, head, &reached, cap)
    }

    /// Refuses a declared machine provider no platform names and a declared platform no
    /// pool names: their keys would never be checked.
    pub(super) fn check_named(&self) -> Result<(), StartupError> {
        let declared = |name: &str| !name.starts_with(MADE);
        for provider in self.machine_providers.iter().filter(|p| declared(&p.name)) {
            let named = self
                .platforms
                .iter()
                .any(|platform| platform.machine_providers.contains(&provider.name));
            if !named {
                let at = format!("machine_provider {:?}", provider.name);
                return Err(StartupError::at(at, "named by no platform"));
            }
        }
        for platform in self.platforms.iter().filter(|p| declared(p.id.as_str())) {
            let named = self
                .pools
                .iter()
                .any(|pool| pool.platforms.contains(&platform.id));
            if !named {
                let at = format!("platform {:?}", platform.id.as_str());
                return Err(StartupError::at(at, "named by no pool"));
            }
        }
        Ok(())
    }
}

/// What a pool's platforms reach.
pub(super) struct Reach {
    /// The platforms, first to last.
    pub(super) platforms: Vec<PlatformId>,
    /// Their machine providers by name, first to last, each once.
    pub(super) providers: Vec<String>,
    /// Whether any of the platforms has hosts.
    pub(super) hosts: bool,
}

/// `items` joined by `, `, or `-` when there are none.
fn listed(items: &[impl AsRef<str>]) -> String {
    match items.is_empty() {
        true => "-".to_owned(),
        false => items
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// `value`, or `-`.
fn shown(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// `duration` in the largest unit that holds it whole: `1h`, `15m`, `90s`, `250ms`.
fn span(duration: Duration) -> String {
    let millis = duration.as_millis();
    let units = [(3_600_000, "h"), (60_000, "m"), (1000, "s")];
    let whole = units
        .into_iter()
        .find(|(size, _)| millis.is_multiple_of(*size));
    match whole {
        Some((size, unit)) => format!("{}{unit}", millis / size),
        None => format!("{millis}ms"),
    }
}

/// The head of the messages about a table of `what` (`machine_provider`, `platform`): its
/// name when it is one, else its number.
fn head_of(table: &toml::Table, what: &str, number: usize) -> String {
    match table.get("name").and_then(toml::Value::as_str) {
        Some(name) if ProviderId::new(name).is_ok() => format!("{what} {name:?}: "),
        _ => format!("{what} #{number}: "),
    }
}

/// The `name` of `s`, which must follow the grammar of names and not be in `taken`.
fn name_of(s: &mut Section, taken: &[&str], what: &str) -> Result<String, StartupError> {
    let name = s.required_string("name")?;
    if ProviderId::new(&name).is_err() {
        return Err(s.fail("name", NAME_RULE));
    }
    match taken.contains(&name.as_str()) {
        true => Err(s.fail("name", &format!("also the name of an earlier {what}"))),
        false => Ok(name),
    }
}

/// The machine providers and platforms the file declares, checked against each other and
/// against the kinds compiled in. No kind's constructor runs here: a provider is built for
/// each pool that reaches it.
pub(super) fn declared(
    providers: Vec<toml::Table>,
    platforms: Vec<toml::Table>,
    kinds: &Kinds,
) -> Result<Resolved, StartupError> {
    let mut resolved = Resolved::default();
    for (index, table) in providers.into_iter().enumerate() {
        let head = head_of(&table, "machine_provider", index + 1);
        let provider = machine_provider(Section::open(table, head), &resolved, kinds)?;
        resolved.machine_providers.push(provider);
    }
    for (index, table) in platforms.into_iter().enumerate() {
        let head = head_of(&table, "platform", index + 1);
        let platform = platform(Section::open(table, head), &resolved, kinds)?;
        resolved.platforms.push(platform);
    }
    Ok(resolved)
}

fn machine_provider(
    mut s: Section,
    resolved: &Resolved,
    kinds: &Kinds,
) -> Result<ResolvedMachineProvider, StartupError> {
    let taken: Vec<&str> = resolved
        .machine_providers
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let name = name_of(&mut s, &taken, "machine provider")?;
    s.head = format!("machine_provider {name:?}: ");
    let kind = s.required_string("kind")?;
    if kind == STATIC {
        s.only(&["name", "kind"])?;
    } else if !kinds.has(&kind) {
        let known = kinds.names().join(", ");
        let reason = format!("unknown machine provider kind {kind:?}; known: {known}");
        return Err(s.fail("kind", &reason));
    }
    Ok(ResolvedMachineProvider {
        name,
        kind,
        table: s.map,
    })
}

fn platform(
    mut s: Section,
    resolved: &Resolved,
    kinds: &Kinds,
) -> Result<ResolvedPlatform, StartupError> {
    let taken: Vec<&str> = resolved.platforms.iter().map(|p| p.id.as_str()).collect();
    let name = name_of(&mut s, &taken, "platform")?;
    s.head = format!("platform {name:?}: ");
    let kind_name = s.required_string("kind")?;
    let Some(kind) = kinds.platform(&kind_name) else {
        let reason = match UNBUILT_PLATFORMS.contains(&kind_name.as_str()) {
            true => format!("{kind_name:?} is not built in this version"),
            false => {
                let known = kinds.platform_names().join(", ");
                format!("unknown platform kind {kind_name:?}; known: {known}")
            }
        };
        return Err(s.fail("kind", &reason));
    };
    if !kind.has_hosts() {
        s.only(HOSTLESS_PLATFORM_KEYS)?;
        return Ok(ResolvedPlatform {
            id: PlatformId::new(name),
            kind,
            machine_providers: Vec::new(),
        });
    }
    s.only(PLATFORM_KEYS)?;
    let Some(names) = s.names("machine_providers")? else {
        return Err(s.fail("machine_providers", "required"));
    };
    for named in &names {
        if resolved.provider(named).is_none() {
            let reason = format!("{named:?} is not a declared machine provider");
            return Err(s.fail("machine_providers", &reason));
        }
    }
    Ok(ResolvedPlatform {
        id: PlatformId::new(name),
        kind,
        machine_providers: names,
    })
}

/// A pool that names only hostless platforms: it may give its name, its platforms and
/// `path_grace`, and each key that describes hosts is refused naming it.
pub(super) fn hostless(
    mut s: Section,
    reach: Reach,
    resolved: &mut Resolved,
) -> Result<String, StartupError> {
    if let Some(key) = HOST_KEYS.iter().find(|key| s.map.contains_key(**key)) {
        return Err(s.fail(key, DESCRIBES_HOSTS));
    }
    s.only(HOSTLESS_POOL_KEYS)?;
    let name = name_of(&mut s, &[], "pool")?;
    let path_grace = s.duration_or("path_grace", PATH_GRACE)?;
    resolved.pools.push(ResolvedPool {
        name: name.clone(),
        platforms: reach.platforms,
        max_machines: None,
        settings: PoolSettings {
            max_queue_wait: MAX_QUEUE_WAIT,
            start_timeout: START_TIMEOUT,
            path_grace,
        },
        provider_limits: BTreeMap::new(),
    });
    Ok(name)
}

/// The settings of a pool with hosts, each a positive duration when given.
pub(super) fn settings(
    s: &mut Section,
) -> Result<(PoolSettings, Option<&'static str>), StartupError> {
    let given = [
        ("max_queue_wait", s.duration("max_queue_wait")?),
        ("start_timeout", s.duration("start_timeout")?),
        ("path_grace", s.duration("path_grace")?),
    ];
    let first = given
        .iter()
        .find(|(_, value)| value.is_some())
        .map(|(key, _)| *key);
    let settings = PoolSettings {
        max_queue_wait: given[0].1.unwrap_or(MAX_QUEUE_WAIT),
        start_timeout: given[1].1.unwrap_or(START_TIMEOUT),
        path_grace: given[2].1.unwrap_or(PATH_GRACE),
    };
    Ok((settings, first))
}
