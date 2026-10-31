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
//! With `--job-user <name>` the verb runs [`ROOT_TARGETS`] instead, as an agent runs on a
//! node: each in a `Delegate=yes` unit as ROOT ([`root_systemd_run_args`]), whose tests run
//! every container as the non-root job user `<name>` (an account with a subordinate id
//! range, which the caller made).
//!
//! A unit's result is read from the unit, never taken from `systemd-run`: on a hosted runner
//! `systemd-run --wait --collect --pipe` exited 0 after the unit's test binary exited 101.
//! After each run the verb reads the unit's own `Result`, `ExecMainCode` and `ExecMainStatus`
//! with `systemctl show` ([`unit_verdict`]), and a target passes only if `systemd-run` and
//! that read both say so. Units are not collected on exit ([`unit_name`] makes each run's
//! name unique), so a failed one stays loaded to be read; the verb resets it afterwards.
//!
//! Every target runs even when one before it failed; the verb fails if any did.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

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

/// The real-`crun` test targets (files or directories of the package's `tests/`), in the
/// order they run.
pub const TARGETS: &[&str] = &["crun"];

/// The real-`crun` test targets run as root for a job user (`--job-user`), in the order
/// they run.
pub const ROOT_TARGETS: &[&str] = &["crun_root"];

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

/// What the verb asks of `sudo -n`: the seam the tests put a fake behind.
pub trait Sudo {
    /// Runs `args` as root: its exit status. Its output is this process's.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, args: &[OsString]) -> Result<ExitStatus, Error>;

    /// Runs `args` as root and returns what it printed on stdout.
    ///
    /// # Errors
    /// The program could not be started, or exited with a failure status.
    fn read(&self, args: &[OsString]) -> Result<String, Error>;
}

/// The real `sudo` program, run with `-n` so it never prompts.
pub struct SudoProgram<'a>(pub &'a Path);

impl Sudo for SudoProgram<'_> {
    fn run(&self, args: &[OsString]) -> Result<ExitStatus, Error> {
        Command::new(self.0)
            .arg("-n")
            .args(args)
            .status()
            .map_err(Error::spawn(self.0.display().to_string()))
    }

    fn read(&self, args: &[OsString]) -> Result<String, Error> {
        let out = Command::new(self.0)
            .arg("-n")
            .args(args)
            .stderr(Stdio::inherit())
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        if !out.status.success() {
            return Err(Error::Failed(format!(
                "{} -n {} failed: {}",
                self.0.display(),
                args.iter()
                    .take(2)
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                out.status
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// The name of the unit that runs `target` in the run of the process `run_id`: unique per
/// run, so a read of the unit's result is about this run's unit and no other.
pub fn unit_name(target: &str, run_id: u32) -> String {
    format!("js-crun-tests-{target}-{run_id}")
}

/// The properties of a unit that say how its main process ended.
const UNIT_PROPERTIES: [&str; 4] = ["Result", "ExecMainCode", "ExecMainStatus", "LoadState"];

/// The arguments for `sudo -n` that read how `unit` ended.
pub fn show_args(unit: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemctl", "show"].map(OsString::from).into();
    args.push(unit.into());
    for property in UNIT_PROPERTIES {
        args.push("-p".into());
        args.push(property.into());
    }
    args
}

/// The arguments for `sudo -n` that clear `unit`'s failed state, so it does not linger.
pub fn reset_args(unit: &str) -> Vec<OsString> {
    ["systemctl", "reset-failed", unit]
        .map(OsString::from)
        .into()
}

/// Whether `show`, the output of `systemctl show <unit> -p Result -p ExecMainCode
/// -p ExecMainStatus -p LoadState`, says the unit's main process exited with status 0.
///
/// A unit that ended cleanly may already be unloaded (a unit is collected when inactive
/// unless it failed), and then systemd reports `LoadState=not-found` with its defaults,
/// which is accepted; a failed unit is never collected, so it is always read as it ended.
///
/// # Errors
/// A line that is not `Key=Value`, a property missing, or a unit that did not end with
/// `Result=success` and `ExecMainStatus=0` (exited, `ExecMainCode=1`, unless unloaded): the
/// reason, with what was read.
pub fn unit_verdict(show: &str) -> Result<(), String> {
    let mut props = HashMap::new();
    for line in show.lines().filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once('=').ok_or_else(|| {
            format!("systemctl show printed a line that is not Key=Value: {line:?}")
        })?;
        props.insert(key, value);
    }
    let get = |key: &str| {
        props
            .get(key)
            .copied()
            .ok_or_else(|| format!("systemctl show printed no {key}: {show:?}"))
    };
    let (result, code, status, load) = (
        get("Result")?,
        get("ExecMainCode")?,
        get("ExecMainStatus")?,
        get("LoadState")?,
    );
    // A pass with `LoadState=not-found` is sound: without `--collect` systemd collects a unit
    // only when it ended cleanly (inactive, not failed), and a failed unit stays loaded, so a
    // failure is read from the unit itself and never lands in this arm. The collected unit
    // reports its defaults (`ExecMainCode=0`), hence `code` is not checked then; `Result` and
    // `ExecMainStatus` must still read as a clean exit.
    if result == "success" && status == "0" && (code == "1" || load == "not-found") {
        return Ok(());
    }
    Err(format!(
        "the unit did not exit 0: Result={result}, ExecMainCode={code}, \
         ExecMainStatus={status}, LoadState={load}"
    ))
}

/// Clears `unit`'s failed state after its result was read as `shown`. A unit that was
/// collected (`LoadState=not-found`) is not loaded, so `reset-failed` would only print
/// "not loaded"; it is skipped then. Any other `reset-failed` error is reported by `sudo`
/// and never changes the verdict.
fn reset_unit(sudo: &dyn Sudo, unit: &str, shown: &Result<String, Error>) {
    let collected = shown
        .as_ref()
        .is_ok_and(|text| text.lines().any(|l| l == "LoadState=not-found"));
    if !collected {
        let _ = sudo.run(&reset_args(unit));
    }
}

/// Runs the `systemd-run` command `args` for `unit`, then reads and resets the unit: `Ok`
/// only when `systemd-run` exited 0 and the unit's own result is a clean exit. Every
/// reason it is not, joined.
fn run_unit(sudo: &dyn Sudo, unit: &str, args: &[OsString]) -> Result<(), String> {
    let ran = sudo.run(args);
    let shown = sudo.read(&show_args(unit));
    reset_unit(sudo, unit, &shown);
    let mut why = Vec::new();
    match ran {
        Ok(status) if status.success() => {}
        Ok(status) => why.push(format!("systemd-run: {status}")),
        Err(e) => why.push(e.to_string()),
    }
    match shown {
        Ok(text) => why.extend(unit_verdict(&text).err()),
        Err(e) => why.push(format!("could not read the result of unit {unit}: {e}")),
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(why.join("; "))
    }
}

/// Reads and resets `unit`, which a step started with `systemd-run --wait` (without
/// `--collect`): the line to print when it exited 0.
///
/// # Errors
/// The unit's result could not be read or is not a clean exit.
pub fn check_unit(sudo: &dyn Sudo, unit: &str) -> Result<String, Error> {
    let shown = sudo.read(&show_args(unit));
    reset_unit(sudo, unit, &shown);
    let text = shown
        .map_err(|e| Error::Failed(format!("could not read the result of unit {unit}: {e}")))?;
    unit_verdict(&text).map_err(|why| Error::Failed(format!("unit {unit}: {why}")))?;
    Ok(format!("ok: unit {unit} exited 0"))
}

/// What one target's unit is given.
pub struct Unit<'a> {
    /// The target's name.
    pub target: &'a str,
    /// The unit's name ([`unit_name`]).
    pub unit: &'a str,
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
/// `systemd-run`, waiting for it and passing its output through (and not collecting it, so a
/// failed unit stays to be read), in
/// [`SLICE`] with `Delegate=yes` and `OOMScoreAdjust=-900`, as the runner user, from the
/// package's directory (where `cargo test` runs a binary), with the binary's ignored
/// tests included.
///
/// The environment is the runner user's `HOME` and `PATH`, and the manifest as
/// `JS_TEST_IMAGE`.
pub fn systemd_run_args(u: &Unit<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemd-run", "--quiet", "--wait", "--pipe"]
        .map(OsString::from)
        .into();
    args.push(format!("--unit={}", u.unit).into());
    args.push(format!("--slice={SLICE}").into());
    args.extend(["-p", "Delegate=yes", "-p", "OOMScoreAdjust=-900"].map(OsString::from));
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

/// What one root target's unit is given.
pub struct RootUnit<'a> {
    /// The target's name.
    pub target: &'a str,
    /// The unit's name ([`unit_name`]).
    pub unit: &'a str,
    /// Its test binary and package directory.
    pub built: &'a Built,
    /// The job user's account name: the tests start every container as this user.
    pub job_user: &'a str,
    /// The `PATH` the tests find `crun`, `busybox` and the id tools on.
    pub path: &'a OsStr,
}

/// [`systemd_run_args`] for a target whose tests run as root: the same unit (in [`SLICE`],
/// with `Delegate=yes` and `OOMScoreAdjust=-900`, from the package's directory, with the
/// binary's ignored tests included), with no `--uid` or `--gid`, so a system unit runs as
/// root, and the environment the tests read: `PATH` and `JS_JOB_USER`.
pub fn root_systemd_run_args(u: &RootUnit<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemd-run", "--quiet", "--wait", "--pipe"]
        .map(OsString::from)
        .into();
    args.push(format!("--unit={}", u.unit).into());
    args.push(format!("--slice={SLICE}").into());
    args.extend(["-p", "Delegate=yes", "-p", "OOMScoreAdjust=-900"].map(OsString::from));
    args.push(joined("--working-directory=", u.built.dir.as_os_str()));
    let env = [
        joined("PATH=", u.path),
        format!("JS_JOB_USER={}", u.job_user).into(),
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
    pub sudo: &'a dyn Sudo,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
    /// With a job user, [`ROOT_TARGETS`] run as root for that user; without, [`TARGETS`]
    /// run as the user running this verb.
    pub job_user: Option<&'a str>,
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

    let targets = if p.job_user.is_some() {
        ROOT_TARGETS
    } else {
        TARGETS
    };
    let messages = test_build::build(p.cargo, p.root, PACKAGE, targets)?;
    let mut built = Vec::new();
    for name in targets {
        built.push(test_build::find(&messages, name)?);
    }

    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut failed = Vec::new();
    for (target, built) in targets.iter().zip(&built) {
        let unit = unit_name(target, std::process::id());
        let args = match p.job_user {
            Some(job_user) => root_systemd_run_args(&RootUnit {
                target,
                unit: &unit,
                built,
                job_user,
                path: &path,
            }),
            None => systemd_run_args(&Unit {
                target,
                unit: &unit,
                built,
                uid,
                gid,
                home: &home,
                path: &path,
                manifest,
            }),
        };
        if let Err(why) = run_unit(p.sudo, &unit, &args) {
            failed.push(format!(
                "the tests of -p {PACKAGE} --test {target} failed in their unit ({}): {why}",
                built.executable.display()
            ));
        }
    }
    if failed.is_empty() {
        let who = match p.job_user {
            Some(user) => format!(", as root with job user {user}"),
            None => String::new(),
        };
        return Ok(format!(
            "ok: the real-crun tests of -p {PACKAGE} passed, each target in its own \
             Delegate=yes unit{who}"
        ));
    }
    Err(Error::Refused(failed))
}

#[cfg(test)]
mod tests;
