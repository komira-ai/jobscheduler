//! What the package's postinstall script does.
//!
//! STUB: the body arrives with the code commit; the tests of `tests/postinstall.rs` come first.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::daemon::{
    AGENT_LABEL, LAUNCH_DAEMONS_DIR, RELEASE_LINK, RELEASES_DIR, SESSION_LABEL, SHARE_DIR,
};

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
    /// `launchctl print system/<label>` exits 0 for a loaded service. Any other status
    /// counts as not loaded: a bootstrap of a service that was loaded after all fails, loudly.
    fn is_loaded(&self, label: &str) -> io::Result<bool> {
        let status = Command::new(&self.program)
            .args(["print", &format!("system/{label}")])
            .output()?
            .status;
        Ok(status.success())
    }

    fn bootstrap(&self, plist: &Path) -> io::Result<()> {
        let out = Command::new(&self.program)
            .arg("bootstrap")
            .arg("system")
            .arg(plist)
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&out.stderr);
        Err(io::Error::other(format!(
            "launchctl bootstrap {}: {}: {}",
            plist.display(),
            out.status,
            said.trim()
        )))
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

/// The daemons, in the order they are loaded: the helper first, for the agent dials it.
const DAEMONS: [&str; 2] = [SESSION_LABEL, AGENT_LABEL];

/// Whether `version` can be one directory name of the releases: letters, digits, `.`, `-` and
/// `+`, not empty and not starting with a dot.
fn is_plain_version(version: &str) -> bool {
    !version.starts_with('.')
        && !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// `path` below `volume`.
fn on(volume: &Path, path: &str) -> PathBuf {
    volume.join(path.trim_start_matches('/'))
}

/// Whether something, a dangling link included, is at `path`.
fn present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Copies `from` to `to`, mode 0644, by a file of another name and a rename.
fn install(from: &Path, to: &Path) -> Result<(), Error> {
    let temp = to.with_extension("plist.new");
    let result = std::fs::copy(from, &temp)
        .and_then(|_| std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| std::fs::rename(&temp, to));
    result.map_err(io_error(format!("install {}", to.display())))
}

/// Does the postinstall of the release `version` on `volume`; the lines to print. With
/// `launchctl` `None` the volume is not the running system and nothing is loaded.
///
/// # Errors
/// [`Error`].
pub fn run(
    volume: &Path,
    version: &str,
    launchctl: Option<&dyn Launchctl>,
) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::BadVersion(version.to_owned()));
    }
    let release = format!("{RELEASES_DIR}/{version}");
    if !on(volume, &release).is_dir() {
        return Err(Error::NoRelease(on(volume, &release)));
    }
    for label in DAEMONS {
        let template = on(volume, &format!("{SHARE_DIR}/{label}.plist"));
        if !template.is_file() {
            return Err(Error::NoTemplate(template));
        }
    }
    let mut lines = Vec::new();
    let link = on(volume, RELEASE_LINK);
    if present(&link) {
        lines.push(format!("kept the release link {RELEASE_LINK}"));
    } else {
        let dir = link.parent().unwrap_or(volume);
        std::fs::create_dir_all(dir)
            .and_then(|()| std::os::unix::fs::symlink(&release, &link))
            .map_err(io_error(format!("link {}", link.display())))?;
        lines.push(format!("linked {RELEASE_LINK} to {release}"));
    }
    for label in DAEMONS {
        let shown = format!("{LAUNCH_DAEMONS_DIR}/{label}.plist");
        let plist = on(volume, &shown);
        if present(&plist) {
            lines.push(format!("kept {shown}, which exists"));
        } else {
            let template = on(volume, &format!("{SHARE_DIR}/{label}.plist"));
            install(&template, &plist)?;
            lines.push(format!("installed {shown}"));
        }
        let Some(ctl) = launchctl else { continue };
        let launchctl_error = |source| Error::Launchctl {
            label: label.to_owned(),
            source,
        };
        if ctl.is_loaded(label).map_err(launchctl_error)? {
            lines.push(format!("{label} is loaded already"));
        } else {
            ctl.bootstrap(&plist).map_err(launchctl_error)?;
            lines.push(format!("loaded {label}"));
        }
    }
    if launchctl.is_none() {
        lines.push("the volume is not the running system: nothing was loaded".to_owned());
    }
    Ok(lines)
}
