//! The hosts an operator lists in a file, as the server shows them.

use std::path::PathBuf;

use js_pool_static::StaticPool;
use js_types::{HostCapacity, MemoryPercent, ProviderId};

use crate::error::StartupError;

/// The hosts an operator lists in a TOML file (`--pool-config`), as
/// [`js_pool_static::StaticPool`] reads them.
///
/// The server reads the file once, at start, and lists these hosts (`GET /v1/hosts`, the
/// job service's host reads, the metrics) beside the hosts that register themselves. No
/// work is placed on them, and the registry refuses a hello for any of their ids.
#[derive(Debug, Clone)]
pub struct StaticPoolSource {
    path: PathBuf,
    memory_percent: Option<MemoryPercent>,
}

impl StaticPoolSource {
    /// A source over the pool configuration file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            memory_percent: None,
        }
    }

    /// The same source listing `percent` of the memory of each host the file lists as
    /// bookable, in place of the percent the file itself says (which is all of it when it
    /// says none).
    #[must_use]
    pub fn with_memory_percent(mut self, percent: MemoryPercent) -> Self {
        self.memory_percent = Some(percent);
        self
    }

    /// Every host the file lists, in id order, or the flag that made it impossible to read
    /// the file.
    pub fn hosts(&self) -> Result<Vec<HostCapacity>, StartupError> {
        let id = ProviderId::new("static-pool").map_err(StartupError::because("--pool-config"))?;
        let pool = StaticPool::from_config_file_with_percent(id, &self.path, self.memory_percent)
            .map_err(StartupError::because("--pool-config"))?;
        Ok(pool.hosts())
    }
}
