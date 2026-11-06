//! The registry of kinds: the provider kinds a `[[pool]]` of the configuration file may
//! name, compiled into the server.
//!
//! The server reads a pool's `provider.kind`, removes it, and hands the rest of the table to
//! the kind's constructor ([`Ctor`]). It never looks inside: each constructor owns the
//! fields of its kind, refuses the ones it does not know, and reads the secret files
//! through [`BuildCtx::files`]. A constructor makes no network call; what only the
//! network can say is the business of the checks of a running server.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{CapacityProvider, SchedulerId};
use js_lease::Clock as LeaseClock;
use js_provider_ec2 as ec2;
use js_provider_lambdalabs as lambda;
use js_types::{AcceleratorNeed, ExecMode, MemoryPercent, Model, ResourceClass};

use crate::adapt::SharedClock;
use crate::capacity::{CapacitySource, StaticPoolSource};
use crate::config::read_private_reason;
use crate::file::Launch;

/// A table that a constructor refused: the field at fault, relative to the provider table,
/// and why. The reason holds nothing the table said, and no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindError {
    /// The field at fault, such as `api_key_file`, or `kind`.
    pub field: String,
    /// What is wrong with it.
    pub reason: String,
}

/// Reads the files that hold secrets. The server reads them owner-only
/// ([`OwnerOnlyFiles`]); a test injects its own.
pub trait SecretFiles: Send + Sync {
    /// The bytes of the file at `path`, or why it was not read. The reason names the
    /// path and never the content.
    fn read(&self, path: &Path) -> Result<Vec<u8>, String>;
}

/// The secret files of the server: a regular file owned by the user running the server
/// that nobody else can read or write (mode 0600 or 0400), as `--token-file` is.
#[derive(Debug, Clone, Copy)]
pub struct OwnerOnlyFiles;

impl SecretFiles for OwnerOnlyFiles {
    fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        read_private_reason(path)
    }
}

/// What a constructor may use besides its table.
pub struct BuildCtx<'a> {
    /// The name of the pool being built.
    pub pool: &'a str,
    /// The class the pool serves.
    pub class: &'a ResourceClass,
    /// The pool's `memory_schedulable_percent`, when its table sets one: a kind that
    /// lists hosts the operator describes books that share of their memory.
    pub memory_percent: Option<MemoryPercent>,
    /// The server's clock.
    pub clock: SharedClock,
    /// How the constructor reads a secret file.
    pub files: &'a dyn SecretFiles,
    /// The scheduler's id, which marks what the pool's machines are launched as.
    pub scheduler: &'a SchedulerId,
    /// The pool's `max_instances`, or 0 when the file gave none (the pool is refused for
    /// it after the kind is built). A kind may use it as its own backstop.
    pub max_instances: u64,
    /// Whether the pool launches. A kind with a second lock of its own sets it from this.
    pub launch: Launch,
}

/// The least a host of an elastic pool brings, as far as the kind knows it offline:
/// the fewest GPUs over the sizes the pool may launch, and their class. The registry
/// refuses a hello below it (the pool floor); real capacity is what the agent reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Floor {
    /// The fewest GPUs of any size.
    pub gpus: u64,
    /// The class of those GPUs; `Some` exactly when `gpus` is above zero.
    pub gpu_class: Option<Model>,
}

impl Floor {
    /// The accelerators a demand at the floor asks for: its GPUs of its class; `None`
    /// when the floor has no class.
    #[must_use]
    pub fn need(&self) -> Option<AcceleratorNeed> {
        self.gpu_class.clone().map(|model| AcceleratorNeed {
            model: Some(model),
            count: u32::try_from(self.gpus).unwrap_or(u32::MAX),
            ..AcceleratorNeed::default()
        })
    }
}

/// An elastic provider and what the server learns of it offline.
pub struct ElasticBuilt {
    /// The provider.
    pub provider: Box<dyn CapacityProvider>,
    /// The pool floor its sizes give.
    pub floor: Floor,
    /// The execution modes its hosts can run jobs in. A pool whose `exec` is not among
    /// them is refused at start.
    pub exec: Vec<ExecMode>,
    /// Whether an idle host of this kind costs money by the hour even though the floor
    /// reports no GPU, as when a catalog mixes sizes with and without accelerators. The
    /// pool is then held to the lifetime bounds a pool with accelerators is.
    pub idle_costs: bool,
}

/// What a constructor builds.
pub enum Built {
    /// Hosts that exist already, listed by the operator.
    Static(Box<dyn CapacitySource>),
    /// Hosts the scheduler launches.
    Elastic(ElasticBuilt),
}

/// A constructor of a kind: the provider table without its `kind`.
pub type Ctor = fn(&toml::Table, &BuildCtx<'_>) -> Result<Built, KindError>;

/// A kind of container platform a `[[platform]]` table may name, and whether its jobs run
/// on hosts of ours (design 3.5). Only `default` is built; the kinds that run jobs with no
/// host of ours (Kubernetes, Cloud Run, ECS) are refused as not built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformKind {
    name: &'static str,
    hosts: bool,
}

impl PlatformKind {
    /// `default`: hosts of ours, from its ordered machine providers.
    pub const DEFAULT: Self = Self {
        name: "default",
        hosts: true,
    };

    /// A kind called `name` whose jobs run with no host of ours. None is built into the
    /// server; a test registers one ([`Kinds::with_platform`]) to reach the checks of a
    /// pool that names only hostless platforms.
    pub const fn hostless(name: &'static str) -> Self {
        Self { name, hosts: false }
    }

    /// The name a `[[platform]]` table's `kind` spells it with.
    pub fn name(self) -> &'static str {
        self.name
    }

    /// Whether the platform's jobs run on hosts of ours, so that the keys of a pool that
    /// describe hosts (`max_machines`, `join_token_file`, the queue settings, ...) apply.
    pub fn has_hosts(self) -> bool {
        self.hosts
    }
}

/// The kinds the server knows, by name: the provider kinds of a pool's `provider` and of a
/// `[[machine_provider]]`, and the kinds of a `[[platform]]`.
#[derive(Clone, Default)]
pub struct Kinds {
    kinds: BTreeMap<&'static str, Ctor>,
    platforms: BTreeMap<&'static str, PlatformKind>,
}

impl Kinds {
    /// The kinds compiled into the server: provider kinds `static`, `lambdalabs` and `ec2`,
    /// and platform kind `default`.
    pub fn standard() -> Self {
        Self::default()
            .with("static", static_kind)
            .with("lambdalabs", lambdalabs_kind)
            .with("ec2", ec2_kind)
            .with_platform(PlatformKind::DEFAULT)
    }

    /// These kinds and the platform kind `kind`.
    #[must_use]
    pub fn with_platform(mut self, kind: PlatformKind) -> Self {
        self.platforms.insert(kind.name, kind);
        self
    }

    /// The platform kind called `name`, when it is one.
    pub fn platform(&self, name: &str) -> Option<PlatformKind> {
        self.platforms.get(name).copied()
    }

    /// The names of the platform kinds, in order.
    pub fn platform_names(&self) -> Vec<&'static str> {
        self.platforms.keys().copied().collect()
    }

    /// Whether a provider kind called `name` is compiled in.
    pub fn has(&self, name: &str) -> bool {
        self.kinds.contains_key(name)
    }

    /// These kinds and `name`, built by `ctor`.
    #[must_use]
    pub fn with(mut self, name: &'static str, ctor: Ctor) -> Self {
        self.kinds.insert(name, ctor);
        self
    }

    /// The names of the kinds, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.kinds.keys().copied().collect()
    }

    /// Builds the kind `kind` from `table`.
    ///
    /// # Errors
    ///
    /// [`KindError`] on field `kind` when `kind` is not one of [`Kinds::names`], else the
    /// constructor's.
    pub fn build(
        &self,
        kind: &str,
        table: &toml::Table,
        ctx: &BuildCtx<'_>,
    ) -> Result<Built, KindError> {
        match self.kinds.get(kind) {
            Some(ctor) => ctor(table, ctx),
            None => Err(KindError {
                field: "kind".to_owned(),
                reason: format!("{kind:?} is not a kind; known: {}", self.names().join(", ")),
            }),
        }
    }
}

/// The `static` kind: `js_pool_static::kind::build` over the operator's hosts file.
fn static_kind(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = js_pool_static::kind::build(table).map_err(|refused| KindError {
        field: refused.field,
        reason: refused.reason,
    })?;
    let source = StaticPoolSource::new(path).with_classes(vec![ctx.class.clone()]);
    let source = match ctx.memory_percent {
        Some(percent) => source.with_memory_percent(percent),
        None => source,
    };
    Ok(Built::Static(Box::new(source)))
}

/// The provider's clock over the server's: the milliseconds the server's clock reads.
struct ProviderClock(SharedClock);

impl lambda::Clock for ProviderClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.now().millis())
    }
}

/// The server's reader of secret files, as the Lambda kind asks for it.
struct Reader<'a>(&'a dyn SecretFiles);

impl lambda::kind::SecretFiles for Reader<'_> {
    fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        self.0.read(path)
    }
}

/// The `lambdalabs` kind: `js_provider_lambdalabs::kind::build` over the pool's table. The
/// provider is built disabled and in dry-run unless the pool's `launch` is `live`; the
/// hosts it launches run containers.
fn lambdalabs_kind(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let max = usize::try_from(ctx.max_instances).unwrap_or(usize::MAX);
    let kind_ctx = lambda::kind::Context {
        scheduler: ctx.scheduler.clone(),
        max_machines: NonZeroUsize::new(max).unwrap_or(NonZeroUsize::MIN),
        live: ctx.launch == Launch::Live,
        clock: Arc::new(ProviderClock(ctx.clock.clone())),
        files: &Reader(ctx.files),
    };
    let built = lambda::kind::build(table, &kind_ctx).map_err(|refused| KindError {
        field: refused.field,
        reason: refused.reason,
    })?;
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(built.provider),
        floor: Floor {
            gpus: built.gpus,
            gpu_class: Some(built.gpu_class),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// The EC2 provider's clock over the server's: the milliseconds the server's clock reads.
struct Ec2Clock(SharedClock);

impl ec2::Clock for Ec2Clock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.now().millis())
    }
}

/// The `ec2` kind: `js_provider_ec2::kind::build` over the pool's table. The provider is
/// built disabled and in dry-run unless the pool's `launch` is `live`; the hosts it
/// launches run containers, and the kind promises no GPU, so its floor is zero; but its
/// catalog has GPU sizes and EC2 has no run-duration limit, so `idle_costs` holds the
/// pool to the lifetime keys.
fn ec2_kind(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let max = usize::try_from(ctx.max_instances).unwrap_or(usize::MAX);
    let kind_ctx = ec2::kind::Context {
        scheduler: ctx.scheduler.clone(),
        max_machines: NonZeroUsize::new(max).unwrap_or(NonZeroUsize::MIN),
        live: ctx.launch == Launch::Live,
        clock: Arc::new(Ec2Clock(ctx.clock.clone())),
    };
    let built = ec2::kind::build(table, &kind_ctx).map_err(|refused| KindError {
        field: refused.field,
        reason: refused.reason,
    })?;
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: true,
        provider: Box::new(built.provider),
        floor: Floor {
            gpus: 0,
            gpu_class: None,
        },
        exec: vec![ExecMode::Container],
    }))
}

#[cfg(test)]
mod tests {
    use js_lease::{ManualClock, Timestamp};

    use super::*;

    /// Asserts that the provider's clock reads the server's, in milliseconds since the
    /// epoch, and moves when it moves. It catches a provider clock that counts from
    /// another origin, which would make the launch interval a guess.
    #[test]
    fn the_providers_clock_is_the_servers() {
        use lambda::Clock as _;
        let server = ManualClock::new(Timestamp::from_millis(5_000));
        let clock = ProviderClock(SharedClock::new(server.clone()));
        assert_eq!(clock.now(), Duration::from_millis(5_000));
        server.advance(Duration::from_secs(2));
        assert_eq!(clock.now(), Duration::from_millis(7_000));
    }

    /// Asserts that the EC2 provider's clock reads the server's, in milliseconds since the
    /// epoch, and moves when it moves. It catches a provider clock that counts from another
    /// origin, which would make the launch interval and the unknown hold a guess.
    #[test]
    fn the_ec2_providers_clock_is_the_servers() {
        use ec2::Clock as _;
        let server = ManualClock::new(Timestamp::from_millis(9_000));
        let clock = Ec2Clock(SharedClock::new(server.clone()));
        assert_eq!(clock.now(), Duration::from_millis(9_000));
        server.advance(Duration::from_secs(3));
        assert_eq!(clock.now(), Duration::from_millis(12_000));
    }
}
