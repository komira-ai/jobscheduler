//! `cargo xtask crun-tests`: the real-`crun` integration tests of `js-contain`
//! (`crates/js-contain/tests/crun/`, every test `#[ignore]` with the reason) on a Linux
//! machine that has `crun`, `newuidmap`/`newgidmap`, a subordinate id range for the user,
//! cgroup v2 and passwordless `sudo`, such as a GitHub-hosted runner.
//!
//! - this architecture's busybox manifest is one of two pinned digests ([`manifest_for`]),
//!   so no index is resolved here and the tests name the image as a node is told to: by
//!   the per-architecture manifest digest ([`arch_for`] maps the machine);
//! - the test targets are built as the runner user ([`test_build::build`]), before any
//!   unit starts, so a unit only runs a binary;
//! - each target's binary is run in its own system unit with `Delegate=yes`, as the runner
//!   user ([`systemd_run_args`]): the user manager alone gets no io controller, and a
//!   daemon must own its subtree. The tests set the unit's cgroup up with the same code a
//!   daemon uses (`js_contain::delegate`). The unit lowers its OOM score as a node's
//!   agent unit does, so the tests see what a job inherits from it.
//!
//! Every target runs even when one before it failed; the verb fails if any did.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::test_build::{self, Built, Error};

/// The busybox image the tests run, without its digest.
pub const BUSYBOX_NAME: &str = "docker.io/library/busybox";
/// The `linux/amd64` manifest of busybox 1.37.0.
pub const BUSYBOX_AMD64: &str =
    "sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";
/// The `linux/arm64` manifest of busybox 1.37.0.
pub const BUSYBOX_ARM64: &str =
    "sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c";
/// The package the tests are in.
pub const PACKAGE: &str = "js-contain";
/// The slice the units run in, as a node's agent unit does.
pub const SLICE: &str = "js-agent.slice";

/// The longest a target's unit may run, in seconds: the bound on a hung test. The tests hold
/// no timeout or deadline of their own, so this is the only one; the unit is killed at the
/// limit and the target fails.
pub const UNIT_MAX_RUNTIME_SECS: u64 = 1200;

/// The real-`crun` test targets (files or directories of the package's `tests/`), in the
/// order they run.
pub const TARGETS: &[&str] = &["crun"];

/// The OCI architecture of the machine `uname -m` names.
///
/// # Errors
/// A machine with no busybox architecture here.
pub fn arch_for(machine: &str) -> Result<&'static str, Error> {
    match machine {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        _ => Err(Error::Failed(format!(
            "no busybox architecture for {machine}"
        ))),
    }
}

/// The pinned busybox manifest of `arch` (`amd64` or `arm64`, [`arch_for`]'s answers).
///
/// # Errors
/// An architecture with no pin.
pub fn manifest_for(arch: &str) -> Result<&'static str, Error> {
    match arch {
        "amd64" => Ok(BUSYBOX_AMD64),
        "arm64" => Ok(BUSYBOX_ARM64),
        _ => Err(Error::Failed(format!(
            "no pinned {BUSYBOX_NAME} manifest for linux/{arch}"
        ))),
    }
}

/// One file under `root` (relative), trimmed, or `n/a` when it cannot be read.
fn read_or_na(root: &Path, file: &str) -> String {
    match std::fs::read_to_string(root.join(file)) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => "n/a".to_owned(),
    }
}

/// How the kernel under `root` (`/` on a runner) reclaims into swap and its swap areas:
/// lines for the log of a run that fails on a memory limit.
pub fn host_report(root: &Path) -> String {
    format!(
        "kernel {}; vm.swappiness {}; zswap {}; lru_gen {}; THP {}\nswap areas:\n{}",
        read_or_na(root, "proc/sys/kernel/osrelease"),
        read_or_na(root, "proc/sys/vm/swappiness"),
        read_or_na(root, "sys/module/zswap/parameters/enabled"),
        read_or_na(root, "sys/kernel/mm/lru_gen/enabled"),
        read_or_na(root, "sys/kernel/mm/transparent_hugepage/enabled"),
        read_or_na(root, "proc/swaps"),
    )
}

/// `sudo -n <args>`: its exit status. Its output is this process's.
fn sudo(program: &Path, args: &[OsString]) -> Result<ExitStatus, Error> {
    Command::new(program)
        .arg("-n")
        .args(args)
        .status()
        .map_err(Error::spawn(program.display().to_string()))
}

/// What one target's unit is given.
pub struct Unit<'a> {
    /// The target's name.
    pub target: &'a str,
    /// Its test binary and package directory.
    pub built: &'a Built,
    /// The runner user's uid, which the unit runs as.
    pub uid: u32,
    /// The runner user's gid.
    pub gid: u32,
    /// The runner user's `HOME`.
    pub home: &'a OsStr,
    /// The runner user's `PATH`.
    pub path: &'a OsStr,
    /// The digest of this architecture's busybox manifest ([`manifest_for`]).
    pub manifest: &'a str,
}

/// `<prefix><value>`, for a value that need not be UTF-8.
fn joined(prefix: &str, value: &OsStr) -> OsString {
    let mut out = OsString::from(prefix);
    out.push(value);
    out
}

/// The arguments for `sudo -n` that run one target's binary in its own system unit:
/// `systemd-run`, waiting for it and passing its output and exit status through, in
/// [`SLICE`] with `Delegate=yes`, `OOMScoreAdjust=-900` and a `RuntimeMaxSec` of
/// [`UNIT_MAX_RUNTIME_SECS`], as the runner user, from the
/// package's directory (where `cargo test` runs a binary), with the binary's ignored
/// tests included.
///
/// The environment is the runner user's `HOME` and `PATH`, and the manifest as
/// `JS_TEST_IMAGE`.
pub fn systemd_run_args(u: &Unit<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemd-run", "--quiet", "--wait", "--collect", "--pipe"]
        .map(OsString::from)
        .into();
    args.push(format!("--unit=js-crun-tests-{}", u.target).into());
    args.push(format!("--slice={SLICE}").into());
    args.extend(["-p", "Delegate=yes", "-p", "OOMScoreAdjust=-900"].map(OsString::from));
    args.extend(["-p".into(), format!("RuntimeMaxSec={UNIT_MAX_RUNTIME_SECS}").into()]);
    args.push(format!("--uid={}", u.uid).into());
    args.push(format!("--gid={}", u.gid).into());
    args.push(joined("--working-directory=", u.built.dir.as_os_str()));
    let env = [
        joined("HOME=", u.home),
        joined("PATH=", u.path),
        format!("JS_TEST_IMAGE=docker://{BUSYBOX_NAME}@{}", u.manifest).into(),
    ];
    for e in env {
        args.push("-E".into());
        args.push(e);
    }
    args.push(u.built.executable.clone().into());
    args.push("--include-ignored".into());
    args
}

/// What `crun-tests` runs and where.
pub struct CrunTests<'a> {
    /// The cargo program (on `cargo xtask`, the `CARGO` cargo sets).
    pub cargo: &'a OsStr,
    /// The sudo program; it is run with `-n`, so it never prompts.
    pub sudo: &'a Path,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
}

/// Reports the host, builds the test targets and runs each in its own unit: the line to
/// print when every target passed. The steps' output and the tests' go to this process's
/// stdout and stderr as they run.
///
/// # Errors
/// A step failed (the first one's error), or one or more targets failed (one line
/// each, after every target ran).
pub fn run(p: &CrunTests<'_>) -> Result<String, Error> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    println!("{}", host_report(Path::new("/")));
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    let arch = arch_for(&machine)?;
    let manifest = manifest_for(arch)?;
    println!("busybox manifest for {arch}: {manifest}");

    let messages = test_build::build(p.cargo, p.root, PACKAGE, TARGETS)?;
    let mut built = Vec::new();
    for name in TARGETS {
        built.push(test_build::find(&messages, name)?);
    }

    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut failed = Vec::new();
    for (target, built) in TARGETS.iter().zip(&built) {
        let unit = Unit {
            target,
            built,
            uid,
            gid,
            home: &home,
            path: &path,
            manifest,
        };
        let status = sudo(p.sudo, &systemd_run_args(&unit))?;
        if !status.success() {
            failed.push(format!(
                "the tests of -p {PACKAGE} --test {target} failed in their unit ({}): {status}",
                built.executable.display()
            ));
        }
    }
    if failed.is_empty() {
        return Ok(format!(
            "ok: the real-crun tests of -p {PACKAGE} passed, each target in its own \
             Delegate=yes unit"
        ));
    }
    Err(Error::Refused(failed))
}

#[cfg(test)]
mod tests;
