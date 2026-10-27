//! The registering side of the server: the registry, the join tokens, the hello limit
//! and the file of bindings, and how they are put together at start.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_registry::Registry;
use tracing::{error, warn};

use crate::adapt::SharedClock;
use crate::agent_state::{self, BindingStore};
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::config::{AgentSetup, Hosts};
use crate::error::StartupError;
use crate::layered::LayeredSource;
use crate::rate::HelloLimiter;

/// What the agent routes and the tick share. Agent handlers take the registry's lock and
/// nothing else of the server's: the lease table's lock is taken before the registry's
/// (the table calls the provider, which calls the registry), never the reverse.
pub(crate) struct Agents {
    pub(crate) registry: Registry,
    pub(crate) tokens: AgentTokens,
    pub(crate) limiter: HelloLimiter,
    store: BindingStore,
    errors: AtomicU64,
}

/// Where the bindings of the lease file at `state_file` are kept: beside it.
fn state_path(state_file: &Path) -> PathBuf {
    let mut name = state_file.file_name().unwrap_or_default().to_os_string();
    name.push(".agents");
    state_file.with_file_name(name)
}

/// What [`assemble`] returns: the capacity source the lease table runs on, and the agent
/// side when it is on.
pub(crate) type Assembled = (Box<dyn CapacitySource>, Option<Arc<Agents>>);

/// The capacity source the lease table runs on, and the agent side when it is on.
///
/// Without agent flags this is the source `--pool-config` names, untouched. With them it
/// is a [`LayeredSource`] over that source (if given) and a registry on `clock`, seeded
/// with the bindings the previous run persisted.
pub(crate) fn assemble(
    hosts: Hosts,
    clock: &SharedClock,
    state_file: &Path,
) -> Result<Assembled, StartupError> {
    match hosts {
        Hosts::Static(source) => Ok((source, None)),
        Hosts::Layered {
            static_pool,
            agents,
        } => {
            let AgentSetup {
                tokens,
                pool,
                hello_limit,
            } = *agents;
            let path = state_path(state_file);
            let bindings = agent_state::load(&path)?;
            let registry = Registry::new(pool, clock.clone());
            let source = LayeredSource::new(static_pool, registry.clone(), bindings.clone());
            let agents = Agents {
                registry,
                tokens,
                limiter: HelloLimiter::new(hello_limit, clock.clone()),
                store: BindingStore::new(path, bindings),
                errors: AtomicU64::new(0),
            };
            Ok((Box::new(source), Some(Arc::new(agents))))
        }
    }
}

impl Agents {
    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again, and the file of
    /// bindings that could not be written.
    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }

    /// What every tick does for the agent side: read the join tokens again, and make the
    /// file of bindings current (a write that failed earlier is tried again here). A
    /// token file that cannot be read keeps the tokens in force.
    pub(crate) fn refresh(&self) {
        if let Err(err) = self.tokens.reload() {
            self.errors.fetch_add(1, Ordering::SeqCst);
            warn!(error = %err, "cannot read the agent token file again; the tokens in force stay");
        }
        self.save_bindings();
    }

    /// Makes the file of bindings hold what the registry has bound. A host that was
    /// admitted stays admitted when this fails; the lease on it would then survive a
    /// restart without its binding, until the next tick writes it.
    pub(crate) fn save_bindings(&self) {
        if let Err(err) = self.store.save(&self.registry.bindings()) {
            self.errors.fetch_add(1, Ordering::SeqCst);
            error!(error = %err, "cannot write the agent state file");
        }
    }
}
