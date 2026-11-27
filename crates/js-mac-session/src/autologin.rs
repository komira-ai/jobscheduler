//! The login window's auto-login, which the boot reset clears and the leak scan reads,
//! and the small pieces the helper's other parts share: running a program and writing a
//! file of the state directory whole.
//!
//! - **Auto-login** is the login window's `autoLoginUser` and `/etc/kcpassword`, the
//!   password it logs in with. The helper never sets either: the boot reset clears both
//!   ([`clear_auto_login`]), so a Mac of the pool never logs a user in at its start.
//! - Every program runs through [`Host::command`](crate::helper::Host::command), so
//!   the tests fake each one.
//!
//! Files are written whole (a temporary file, `fsync`, `rename`, then the directory's
//! `fsync`) and never through a link.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags};

use crate::helper::Host;

/// `defaults`, which reads and deletes the login window's preference. Absolute, and run
/// with an empty environment.
pub const DEFAULTS: &str = "/usr/bin/defaults";

/// The login window's preference that names the user to log in at start.
pub const AUTO_LOGIN_USER: &str = "autoLoginUser";

/// Where the login window keeps its auto-login.
#[derive(Clone, Debug)]
pub struct LoginPaths {
    /// `/etc/kcpassword`.
    pub kcpassword: PathBuf,
    /// The login window's preferences domain, as `defaults` takes it
    /// (`/Library/Preferences/com.apple.loginwindow`).
    pub loginwindow: String,
}

impl LoginPaths {
    /// The paths a test uses: everything under `dir`.
    #[cfg(test)]
    pub(crate) fn under(dir: &Path) -> Self {
        Self {
            kcpassword: dir.join("kcpassword"),
            loginwindow: dir.join("loginwindow").display().to_string(),
        }
    }
}

/// What a program printed, and whether it exited 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the program exited 0.
    pub success: bool,
    /// What it printed on its standard output.
    pub stdout: String,
    /// What it printed on its standard error.
    pub stderr: String,
}

/// Runs `program` with `args` and an empty environment, and waits for it.
///
/// # Errors
/// The program could not be started or waited for.
pub fn run_command(program: &str, args: &[&str]) -> io::Result<CommandOutput> {
    let output = crate::disclaim::Program::new(program, args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn failed(what: &str, output: &CommandOutput) -> String {
    format!("{what} failed: {}", output.stderr.trim())
}

fn command(host: &dyn Host, program: &str, args: &[&str]) -> Result<CommandOutput, String> {
    host.command(program, args)
        .map_err(|why| format!("running {program}: {why}"))
}

/// Removes `path` (a file, or a link itself), durably; `false` if there was none.
///
/// # Errors
/// It could not be removed.
pub fn remove(path: &Path) -> io::Result<bool> {
    match rustix::fs::unlink(path) {
        Ok(()) => {}
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(why) => return Err(why.into()),
    }
    sync_parent(path)?;
    Ok(true)
}

/// Writes `bytes` to a new temporary file next to `path` (mode 0600, never through a
/// link), `fsync`s it, renames it over `path` and `fsync`s the directory.
pub(crate) fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    match rustix::fs::unlink(&temporary) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(why) => return Err(why.into()),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::openat(CWD, &temporary, flags, Mode::from_raw_mode(0o600))?;
    let mut file = std::fs::File::from(opened);
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    rustix::fs::rename(&temporary, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir = rustix::fs::openat(CWD, parent, flags, Mode::empty())?;
    Ok(rustix::fs::fsync(&dir)?)
}

/// Whether a failed `defaults read`'s `stderr` says the key is not set. macOS 26 says
/// `The domain/default pair of (<domain>, <key>) does not exist`; macOS 27 says
/// `Error: Could not find key '<key>' in domain '<domain>'.`. Both exit 1, as every
/// other failure of `defaults read` does, so only the text tells them apart.
pub(crate) fn key_absent(stderr: &str) -> bool {
    stderr.contains(") does not exist") || stderr.contains("Could not find key '")
}

/// Clears the login window's `autoLoginUser` and removes `/etc/kcpassword`; neither
/// being there already is fine. Returns whether anything was there to clear.
///
/// `defaults delete` of a key that is not there fails, so a failed delete is checked
/// with `defaults read`: the key is cleared only if that fails too.
///
/// # Errors
/// The key is still set, or the file could not be removed.
pub fn clear_auto_login(host: &dyn Host, paths: &LoginPaths) -> Result<bool, String> {
    let domain = paths.loginwindow.as_str();
    let deleted = command(host, DEFAULTS, &["delete", domain, AUTO_LOGIN_USER])?;
    if !deleted.success {
        let read = command(host, DEFAULTS, &["read", domain, AUTO_LOGIN_USER])?;
        if read.success {
            return Err(failed(
                &format!("defaults delete {domain} {AUTO_LOGIN_USER}"),
                &deleted,
            ));
        }
    }
    let removed = remove(&paths.kcpassword)
        .map_err(|why| format!("removing {}: {why}", paths.kcpassword.display()))?;
    Ok(deleted.success || removed)
}

#[cfg(test)]
mod tests;
