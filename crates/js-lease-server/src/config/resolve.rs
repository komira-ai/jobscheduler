//! Reading a configuration file for its machine providers, platforms and pools without
//! starting on it.

use std::path::Path;

use super::Config;
use crate::adapt::SharedClock;
use crate::error::StartupError;
use crate::file::{self, Load, Resolved};
use crate::kinds::{Kinds, SecretFiles};

impl Config {
    /// Checks the configuration file at `path` as [`Config::from_config_file`] does and
    /// answers its machine providers, platforms and pools, however each pool was written.
    /// A file that is valid but that this version cannot start on (more than one pool that
    /// registers agents) is resolved too.
    ///
    /// # Errors
    ///
    /// [`StartupError`] naming the table and the field at fault.
    pub fn resolve_file(
        path: &Path,
        kinds: &Kinds,
        files: &dyn SecretFiles,
        clock: SharedClock,
    ) -> Result<Resolved, StartupError> {
        let load = Load {
            kinds,
            files,
            clock,
        };
        Ok(file::load(path, &load)?.resolved)
    }
}
