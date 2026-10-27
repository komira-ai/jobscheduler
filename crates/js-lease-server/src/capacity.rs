//! Where the hosts a server leases come from.

use std::path::PathBuf;

use js_pool_static::StaticPool;
use js_provider::Provider;
use js_types::ProviderId;

use crate::error::StartupError;

/// Builds the provider a server's lease table runs on.
///
/// The server asks its source once, at start. Today the only source is the operator's
/// static pool ([`StaticPoolSource`]): the hosts are listed in a file. Hosts that agents
/// register themselves are a future source, a second implementation of this trait that
/// returns a provider backed by those registrations; the server needs no change for it,
/// and it is not built here.
pub trait CapacitySource {
    /// The provider, or the flag that made it impossible to build.
    fn provider(&self) -> Result<Box<dyn Provider + Send>, StartupError>;
}

/// The hosts an operator lists in a TOML file (`--pool-config`), as
/// [`js_pool_static::StaticPool`] reads them.
#[derive(Debug, Clone)]
pub struct StaticPoolSource {
    path: PathBuf,
}

impl StaticPoolSource {
    /// A source over the pool configuration file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl CapacitySource for StaticPoolSource {
    fn provider(&self) -> Result<Box<dyn Provider + Send>, StartupError> {
        let id = ProviderId::new("static-pool").map_err(StartupError::because("--pool-config"))?;
        StaticPool::from_config_file(id, &self.path)
            .map(|pool| Box::new(pool) as Box<dyn Provider + Send>)
            .map_err(StartupError::because("--pool-config"))
    }
}
