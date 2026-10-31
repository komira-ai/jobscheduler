//! What the package's postinstall script does.
//!
//! STUB: the body arrives with the code commit; the tests of `tests/postinstall.rs` come first.

use std::io;
use std::path::{Path, PathBuf};

/// `/bin/launchctl`, by the absolute path it has on a stock system.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The part of `launchctl` the postinstall uses.
pub trait Launchctl {
    /// Whether a daemon of `label` is loaded in the system domain.
    ///
    /// # Errors
    /// The program could not be started.
    fn is_loaded(&self, label: &str) -> io::Result<bool>;

    /// Loads the daemon `plist` describes into the system domain.
    ///
    /// # Errors
    /// The program could not be started, or refused.
    fn bootstrap(&self, plist: &Path) -> io::Result<()>;
}

/// [`Launchctl`] over the program.
#[derive(Clone, Debug)]
pub struct SystemLaunchctl {
    program: PathBuf,
}

impl SystemLaunchctl {
    /// A runner of the program at `program`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Launchctl for SystemLaunchctl {
    fn is_loaded(&self, _label: &str) -> io::Result<bool> {
        let _ = &self.program;
        Ok(false)
    }

    fn bootstrap(&self, _plist: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// Why the postinstall stopped.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The version is not a plain version string.
    #[error("{0:?} is not a plain version")]
    BadVersion(String),
    /// The release directory is not on the volume.
    #[error("{0} is not a directory")]
    NoRelease(PathBuf),
    /// A plist the package should have put in its share directory is not there.
    #[error("{0} is not a file")]
    NoTemplate(PathBuf),
    /// A file operation failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// Why it failed.
        source: io::Error,
    },
    /// `launchctl` failed.
    #[error("launchctl, {label}: {source}")]
    Launchctl {
        /// The daemon's label.
        label: String,
        /// Why it failed.
        source: io::Error,
    },
}

/// Does the postinstall of the release `version` on `volume`; the lines to print.
///
/// # Errors
/// [`Error`].
pub fn run(
    _volume: &Path,
    _version: &str,
    _launchctl: Option<&dyn Launchctl>,
) -> Result<Vec<String>, Error> {
    Ok(Vec::new())
}
