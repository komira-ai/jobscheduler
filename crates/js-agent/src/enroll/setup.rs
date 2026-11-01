//! The host setup script the deb installs: whether it may be run, and how it is run.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::Failure;

/// The `PATH` the script runs with. It is a root process: no directory of the caller's.
const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Where the script records what an apply did.
pub const MANIFEST: &str = "/var/lib/js-crun-host-setup/manifest";

/// Whether the script at `path` may be run as root: a regular file (not a link) that is
/// root's and that group and others cannot write.
pub(super) fn fit(path: &Path) -> Result<(), Failure> {
    let meta = fs::symlink_metadata(path).map_err(|why| {
        Failure::Usage(format!(
            "{}: {why}; the js-agent deb installs the host setup script",
            path.display()
        ))
    })?;
    let problems = [
        (!meta.is_file(), "is not a regular file"),
        (
            meta.uid() != rustix::process::geteuid().as_raw(),
            "is not owned by root",
        ),
        (meta.mode() & 0o022 != 0, "is writable by group or others"),
    ];
    problems
        .iter()
        .find(|(bad, _)| *bad)
        .map_or(Ok(()), |(_, why)| {
            Err(Failure::Usage(format!(
                "{}: {why}; it will not be run as root",
                path.display()
            )))
        })
}

/// Runs the script with `flags` through `shell`, with an empty environment but `PATH` and
/// no standard input (the token on this process's standard input is not the script's). What
/// it prints goes to `err`. Returns the word for the report: `done` (`planned` when `apply`
/// is false) for its exit 0, `nothing-to-do` for 3.
pub(super) fn run(
    shell: &Path,
    script: &Path,
    flags: &[OsString],
    apply: bool,
    err: &mut dyn Write,
) -> Result<&'static str, Failure> {
    let output = Command::new(shell)
        .arg(script)
        .args(flags)
        .env_clear()
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|why| Failure::Failed(format!("cannot run {}: {why}", shell.display())))?;
    let _ = err.write_all(&output.stdout);
    let _ = err.write_all(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(if apply { "done" } else { "planned" }),
        Some(3) => Ok("nothing-to-do"),
        Some(2) => Err(Failure::Usage(
            "the host setup refused; nothing was changed (its words are above)".to_owned(),
        )),
        other => Err(Failure::Failed(format!(
            "the host setup failed (exit status {other:?}); {MANIFEST} records what it did \
             and its words are above"
        ))),
    }
}
