//! `channels check-apt`: the release installed the way a host installs it, from what was published
//! to the suite `testing`, before the suite `stable` changes. `.github/workflows/release.yml` runs
//! it in its `check-apt` job, in a clean Ubuntu container on an amd64 and an arm64 runner, at
//! release time only.
//!
//! `check-apt` brings the image's packages up to date (the pinned image is older than the
//! archive), adds `packages.komira.ai` with the published keyring (a binary `.gpg`, referred to by
//! a deb822 `Signed-By`) and the suite `testing`, installs `js-agent=<version>`, and then holds the
//! installed system to what the package promises: dpkg lists the package at the version, the
//! binary's `--version` says `js-agent <version>`, every file of the package ([`files`]) is there,
//! the package's postinst made the job user ([`JOB_USER`]) so that the agent's own start check
//! (`js_agent::crun_host::check`) finds nothing missing about the account, the account is a
//! system one with no home and no login, and no unit is enabled and no `js-agent` process runs.
//!
//! The programs (`apt-get`, `dpkg`, `dpkg-query`, `curl`, the installed binary) run behind
//! [`Host`], so a test answers in place of them; `--root` is where the checked system is (`/`),
//! so a test points it at a scratch directory.

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
use js_agent::crun_host::{self, CrunHost, Piece, Request};

use super::apt::state::PACKAGE;
use super::aws::Answer;
use super::{Error, is_plain_version};

/// Where the package puts the binary (below the root).
pub const BINARY: &str = "usr/bin/js-agent";

/// The files of the package below the root: the paths of `release deb`'s table
/// ([`crate::release::deb::FILES`]), in its order.
pub fn files() -> impl Iterator<Item = &'static str> {
    crate::release::deb::FILES
        .iter()
        .map(|file| file.path.trim_start_matches('/'))
}

/// The units of the package: the package enables none of them.
pub const UNITS: [&str; 3] = [
    "js-agent-crun.service",
    "js-agent-crun-gpu.service",
    "js-agent.service",
];

/// The job user: the account `js-agent-crun.service` runs every `crun` as (`--job-user`), which
/// the package's postinst makes from its `sysusers.d` file.
pub const JOB_USER: &str = "jsjob";

/// The home and the shell of the job user: none and no login.
pub const JOB_USER_HOME: &str = "/nonexistent";
/// See [`JOB_USER_HOME`].
pub const JOB_USER_SHELL: &str = "/usr/sbin/nologin";

/// The highest uid of a system account (`SYS_UID_MAX` of Debian's and Ubuntu's `login.defs`).
pub const SYS_UID_MAX: u32 = 999;

/// Where the keyring is put (below the root) and the name the source file refers to it by.
pub const KEYRING: &str = "usr/share/keyrings/komira-archive-keyring.gpg";
const KEYRING_PATH: &str = "/usr/share/keyrings/komira-archive-keyring.gpg";

/// The source file (below the root).
pub const SOURCES: &str = "etc/apt/sources.list.d/komira.sources";

/// The keyring's file name in the repository.
pub const KEYRING_NAME: &str = "komira-archive-keyring.gpg";

/// The programs outside, behind a trait so a test answers in place of them.
pub trait Host {
    /// `program <args>`. A refusal is an [`Answer`] that is not `ok`.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error>;
}

/// The real programs.
pub struct RealHost;

impl Host for RealHost {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(program)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .map_err(Error::spawn(program.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Runs `program <args>`; its standard output, or why it failed.
fn must(host: &dyn Host, program: &Path, args: &[&str]) -> Result<String, Error> {
    let answer = host.run(program, &strings(args))?;
    if answer.ok {
        return Ok(answer.stdout);
    }
    Err(Error::Failed(format!(
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        answer.stderr.trim_end()
    )))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let what = || format!("write {}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(Error::spawn(what()))?;
    std::fs::write(path, bytes).map_err(Error::spawn(what()))
}

fn version_flag(version: &str) -> Result<(), Error> {
    if is_plain_version(version) {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )))
    }
}

/// The first line `js-agent --version` prints for the release `version`.
#[must_use]
pub fn version_line(version: &str) -> String {
    format!("{PACKAGE} {version}")
}

/// The binary at `binary` says the release it is: its `--version` starts with the line
/// `js-agent <version>`. The line it said.
fn says(host: &dyn Host, binary: &Path, version: &str) -> Result<String, Error> {
    let said = must(host, binary, &["--version"])?;
    let first = said.lines().next().unwrap_or_default();
    let expected = version_line(version);
    if first == expected {
        return Ok(first.to_owned());
    }
    Err(Error::Failed(format!(
        "the binary says {first:?}, not {expected:?}"
    )))
}

/// What `check-apt` is asked.
pub struct Apt<'a> {
    /// The system to check: `/`, or a scratch directory in a test.
    pub root: &'a Path,
    /// The Debian architecture of the machine, `amd64` or `arm64`.
    pub arch: &'a str,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
    /// The repository, `https://packages.komira.ai/apt`.
    pub apt_url: &'a str,
    /// The `apt-get` program.
    pub apt_get: &'a Path,
    /// The `dpkg-query` program.
    pub dpkg_query: &'a Path,
    /// The `dpkg` program.
    pub dpkg: &'a Path,
    /// The `curl` program.
    pub curl: &'a Path,
}

/// The deb822 source of the suite `testing`, signed by the keyring alone.
#[must_use]
pub fn source(apt_url: &str, arch: &str) -> String {
    format!(
        "Types: deb\nURIs: {apt_url}\nSuites: testing\nComponents: main\nArchitectures: {arch}\nSigned-By: {KEYRING_PATH}\n"
    )
}

/// The entries of systemd's `wants` and `requires` directories, and the units of the
/// administrator, for a unit of the package below `etc/systemd/system`: what enabling one makes.
fn enabled_links(root: &Path) -> Vec<PathBuf> {
    let base = root.join("etc/systemd/system");
    let mut found = Vec::new();
    for name in UNITS {
        let unit = base.join(name);
        if unit.symlink_metadata().is_ok() {
            found.push(unit);
        }
        for dir in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            let link = dir.path().join(name);
            if dir.path().is_dir() && link.symlink_metadata().is_ok() {
                found.push(link);
            }
        }
    }
    found.sort();
    found
}

/// The processes (by `comm`) named `js-agent` below `root/proc`.
fn running(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join("proc"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim_end() == PACKAGE {
            found.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    found
}

/// A runner that runs no program: the job user's part of the start check runs none, and the
/// pieces that do (`crun --version`) are not the job user's.
#[cfg(target_os = "linux")]
struct NoPrograms;

#[cfg(target_os = "linux")]
impl js_agent::CommandRunner for NoPrograms {
    fn run(&self, program: &str, _args: &[&str]) -> Result<String, String> {
        Err(format!("{program}: not run by check-apt"))
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run(program, args)
    }
}

/// The agent's start check of `--runtime crun` against the system below `root` (every file it
/// reads is below `root`), as the packaged unit asks it (`--job-user jsjob`, the distribution's
/// crun, the unit's state and scratch roots): what it finds missing about the job user, one
/// problem each. The other pieces (crun, the subordinate ranges, the roots, cgroups, AppArmor)
/// are the host's, which the package does not provide, and are not judged here.
///
/// Linux only, as the start check is (`check-apt` runs in a Linux container; xtask also builds
/// on macOS, where there is no such check to run).
#[cfg(target_os = "linux")]
fn job_user_refusals(root: &Path) -> Vec<String> {
    let below = |path: &str| root.join(path.trim_start_matches('/'));
    let mut host = CrunHost::system();
    host.ids.passwd = below("/etc/passwd");
    host.ids.subuid = below("/etc/subuid");
    host.ids.subgid = below("/etc/subgid");
    host.apparmor_userns = below(&host.apparmor_userns.to_string_lossy());
    host.apparmor_profiles = below(&host.apparmor_profiles.to_string_lossy());
    host.cgroup_mount = below(&host.cgroup_mount.to_string_lossy());
    host.self_cgroup = below(&host.self_cgroup.to_string_lossy());
    host.id_tool_dirs = host
        .id_tool_dirs
        .iter()
        .map(|dir| below(&dir.to_string_lossy()))
        .collect();
    let crun = below(crun_host::DISTRO_CRUN);
    let state_root = below("/var/lib/js-agent/state");
    let scratch_root = below("/var/lib/js-agent/scratch/runs");
    let request = Request {
        crun: &crun,
        job_user: JOB_USER,
        state_root: &state_root,
        scratch_root: &scratch_root,
    };
    crun_host::check(&request, &host, &NoPrograms)
        .missing
        .iter()
        .filter(|missing| missing.piece == Piece::JobUser)
        .map(|missing| format!("the agent's start check refuses the host: {missing}"))
        .collect()
}

/// The job user's line of the passwd-format `passwd`, held to what the package's `sysusers.d`
/// file makes: a system uid, the home [`JOB_USER_HOME`] and the shell [`JOB_USER_SHELL`]. No
/// line is no problem here: [`job_user_refusals`] names it.
fn job_user_account_problems(passwd: &str) -> Vec<String> {
    let Some(fields) = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.first() == Some(&JOB_USER))
    else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    let field = |at: usize| fields.get(at).copied().unwrap_or_default();
    if !field(2)
        .parse::<u32>()
        .is_ok_and(|uid| (1..=SYS_UID_MAX).contains(&uid))
    {
        problems.push(format!(
            "the job user {JOB_USER} has uid {:?}, not a system uid (1 to {SYS_UID_MAX})",
            field(2)
        ));
    }
    if field(5) != JOB_USER_HOME {
        problems.push(format!(
            "the job user {JOB_USER} has the home {:?}, not {JOB_USER_HOME:?}",
            field(5)
        ));
    }
    if field(6) != JOB_USER_SHELL {
        problems.push(format!(
            "the job user {JOB_USER} has the shell {:?}, not {JOB_USER_SHELL:?}",
            field(6)
        ));
    }
    problems
}

/// The checks of an installed system, as problems.
fn installed_problems(r: &Apt, installed: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if installed != r.version {
        problems.push(format!(
            "dpkg says {PACKAGE} {installed:?} is installed, not {:?}",
            r.version
        ));
    }
    for rel in files() {
        if !r.root.join(rel).is_file() {
            problems.push(format!("the package's file /{rel} is not installed"));
        }
    }
    #[cfg(target_os = "linux")]
    problems.extend(job_user_refusals(r.root));
    let passwd = std::fs::read_to_string(r.root.join("etc/passwd")).unwrap_or_default();
    problems.extend(job_user_account_problems(&passwd));
    for link in enabled_links(r.root) {
        problems.push(format!(
            "a unit is enabled: {} exists, and the package enables nothing",
            link.display()
        ));
    }
    for pid in running(r.root) {
        problems.push(format!(
            "{PACKAGE} is running (process {pid}), and the package starts nothing"
        ));
    }
    problems
}

/// The `check-apt` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a program that fails (naming it and what it said), a keyring that is
/// not a binary keyring, a machine of another architecture, a binary that does not say the
/// release, or one problem per way the installed system is not what the package promises.
pub fn check_apt(host: &dyn Host, r: &Apt) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if !["amd64", "arm64"].contains(&r.arch) {
        return Err(Error::Usage(format!(
            "--arch {:?} is not amd64 or arm64",
            r.arch
        )));
    }
    if !r.apt_url.starts_with("https://") || r.apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {:?} is not an https URL without a trailing /",
            r.apt_url
        )));
    }
    let machine = must(host, r.dpkg, &["--print-architecture"])?;
    if machine.trim() != r.arch {
        return Err(Error::Failed(format!(
            "this machine is {}, not {}: the check would install another package",
            machine.trim(),
            r.arch
        )));
    }
    // A clean image has no certificates: the first update goes over the mirrors' plain HTTP.
    // The image is pinned by digest, so its packages are as old as the pin: the archive has moved
    // on, and `ca-certificates` and `curl` may need newer versions of packages the image has
    // ("you have held broken packages"). So the installed packages are brought up to date before
    // anything is installed. `dist-upgrade` may install or remove packages to do so, which
    // `upgrade` never does; `--force-confold` keeps the image's configuration files, as there
    // is nobody to answer dpkg's question.
    must(host, r.apt_get, &["update"])?;
    must(
        host,
        r.apt_get,
        &[
            "dist-upgrade",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confold",
        ],
    )?;
    must(
        host,
        r.apt_get,
        &[
            "install",
            "-y",
            "--no-install-recommends",
            "ca-certificates",
            "curl",
        ],
    )?;
    let keyring = r.root.join(KEYRING);
    let keyring_arg = keyring.display().to_string();
    must(
        host,
        r.curl,
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--create-dirs",
            "--output",
            &keyring_arg,
            &format!("{}/{KEYRING_NAME}", r.apt_url),
        ],
    )?;
    let bytes =
        std::fs::read(&keyring).map_err(Error::spawn(format!("read {}", keyring.display())))?;
    // A binary OpenPGP keyring starts with a packet header (bit 7 set); an armored one is text
    // that a deb822 `Signed-By` path does not accept.
    if bytes.first().is_none_or(|b| b & 0x80 == 0) {
        return Err(Error::Failed(format!(
            "{KEYRING_NAME} is not a binary keyring: apt takes a .gpg file in Signed-By, not armor"
        )));
    }
    write(&r.root.join(SOURCES), source(r.apt_url, r.arch).as_bytes())?;
    must(host, r.apt_get, &["update"])?;
    let target = format!("{PACKAGE}={}", r.version);
    must(
        host,
        r.apt_get,
        &["install", "-y", "--no-install-recommends", &target],
    )?;
    let installed = must(
        host,
        r.dpkg_query,
        &["--show", "--showformat=${Version}", PACKAGE],
    )?;
    let binary = r.root.join(BINARY);
    let said = says(host, &binary, r.version)?;
    let problems = installed_problems(r, &installed);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "check-apt: {PACKAGE} {} installed from the suite testing on {}; it says {said:?}; its {} files are there; the agent's start check accepts the job user {JOB_USER}; no unit is enabled and nothing runs",
        r.version,
        r.arch,
        crate::release::deb::FILES.len()
    )])
}

#[cfg(test)]
mod tests;
