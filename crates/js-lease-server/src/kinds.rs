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

use js_elastic::{ElasticProvider, GpuClass, SchedulerId};
use js_lease::Clock as LeaseClock;
use js_provider_lambdalabs as lambda;
use js_types::{ExecMode, MemoryPercent, ResourceClass};

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
    pub gpu_class: Option<GpuClass>,
}

/// An elastic provider and what the server learns of it offline.
pub struct ElasticBuilt {
    /// The provider.
    pub provider: Box<dyn ElasticProvider>,
    /// The pool floor its sizes give.
    pub floor: Floor,
    /// The execution modes its hosts can run jobs in. A pool whose `exec` is not among
    /// them is refused at start.
    pub exec: Vec<ExecMode>,
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

/// The kinds the server knows, by name.
#[derive(Clone, Default)]
pub struct Kinds {
    kinds: BTreeMap<&'static str, Ctor>,
}

impl Kinds {
    /// The kinds compiled into the server: `static` and `lambdalabs`.
    pub fn standard() -> Self {
        Self::default()
            .with("static", static_kind)
            .with("lambdalabs", lambdalabs_kind)
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
        provider: Box::new(built.provider),
        floor: Floor {
            gpus: built.gpus,
            gpu_class: Some(built.gpu_class),
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
}
