//! The hook's declaration, `/etc/updater.d/js-agent.toml` (host-updates design, 2.4 and 4.2):
//! the part of it the hook checks its record against.
//!
//! ```toml
//! state_dir = "/var/lib/js-agent-update"
//!
//! [hook]
//! units       = ["js-agent-crun.service"]
//! write_paths = ["/opt/js-agent", "/opt/js-agent-link", "/var/lib/js-agent-update"]
//!
//! [hook.timeout_secs]
//! prepare = 3900
//! # cutover, verify, rollback, cleanup likewise
//! ```
//!
//! Keys the hook does not read are ignored: the file is the updater's, and it adds keys.
//! The release directories are the `write_paths` that are not the `state_dir`: the declaration
//! has no key of its own for them, because a staged release and the link it is moved to are
//! both written by the hook and the state directory is the one write path that is not a
//! release.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where the declaration is installed.
pub const DECLARATION_PATH: &str = "/etc/updater.d/js-agent.toml";

/// Why a declaration was refused.
#[derive(Debug, thiserror::Error)]
pub enum DeclarationError {
    /// The file could not be read.
    #[error("cannot read the declaration: {0}")]
    Read(#[from] io::Error),
    /// The file is not TOML, or lacks a key the hook needs, or has one of the wrong type.
    #[error("the declaration is not usable: {0}")]
    Parse(#[from] toml::de::Error),
    /// `state_dir` or a write path is relative.
    #[error("state_dir and every write path must be absolute")]
    Relative,
    /// `hook.units` is empty: no unit could ever match the record's.
    #[error("hook.units is empty")]
    NoUnits,
    /// Every write path is the state directory: no release could lie under any.
    #[error("hook.write_paths names no directory but state_dir for a release")]
    NoReleaseDir,
}

/// The `[hook.timeout_secs]` table: the updater's bound on each step, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Timeouts {
    /// `prepare`.
    pub prepare: u64,
    /// `cutover`.
    pub cutover: u64,
    /// `verify`.
    pub verify: u64,
    /// `rollback`.
    pub rollback: u64,
    /// `cleanup`.
    pub cleanup: u64,
}

#[derive(Deserialize)]
struct Wire {
    state_dir: PathBuf,
    hook: WireHook,
}

#[derive(Deserialize)]
struct WireHook {
    units: Vec<String>,
    write_paths: Vec<PathBuf>,
    timeout_secs: Timeouts,
}

/// What the hook reads of its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The directory of the hook record: `root:root`, mode 0700.
    pub state_dir: PathBuf,
    /// The units the hook may manage; a record's unit must be one of them.
    pub units: Vec<String>,
    /// Every directory the hook may write, the state directory among them.
    pub write_paths: Vec<PathBuf>,
    /// The bound on each step.
    pub timeouts: Timeouts,
}

impl Declaration {
    /// Reads the declaration at `path`.
    ///
    /// # Errors
    /// [`DeclarationError`]: unreadable, not a usable declaration, or a path that is not
    /// absolute.
    pub fn read(path: &Path) -> Result<Declaration, DeclarationError> {
        Declaration::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a declaration.
    ///
    /// # Errors
    /// [`DeclarationError`]: see [`Declaration::read`].
    pub fn parse(text: &str) -> Result<Declaration, DeclarationError> {
        let wire: Wire = toml::from_str(text)?;
        let declaration = Declaration {
            state_dir: wire.state_dir,
            units: wire.hook.units,
            write_paths: wire.hook.write_paths,
            timeouts: wire.hook.timeout_secs,
        };
        let mut paths = std::iter::once(&declaration.state_dir).chain(&declaration.write_paths);
        if paths.any(|path| !path.is_absolute()) {
            return Err(DeclarationError::Relative);
        }
        if declaration.units.is_empty() {
            return Err(DeclarationError::NoUnits);
        }
        if declaration.release_dirs().next().is_none() {
            return Err(DeclarationError::NoReleaseDir);
        }
        Ok(declaration)
    }

    /// The directories a release may lie under: the write paths other than the state
    /// directory.
    pub fn release_dirs(&self) -> impl Iterator<Item = &Path> {
        self.write_paths
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| *path != self.state_dir)
    }
}
