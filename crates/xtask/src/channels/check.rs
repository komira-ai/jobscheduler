//! `channels check-apt`: the release installed the way a host installs it, from what was published
//! to the suite `testing`, before the suite `stable` changes. `.github/workflows/release.yml` runs
//! it in its `check-apt` job, in a clean Ubuntu container on an amd64 and an arm64 runner, at
//! release time only.
//!
//! `check-apt` brings the image's packages up to date (the pinned image is older than the
//! archive), adds `packages.komira.ai` with the published keyring (a binary `.gpg`, referred to by
//! a deb822 `Signed-By`) and the suite `testing`, installs `js-agent=<version>`, and then holds the
//! installed system to what the package promises: dpkg lists the package at the version, the
//! binary's `--version` says `js-agent <version>`, every file of the package ([`FILES`]) is there,
//! and no unit is enabled and no `js-agent` process runs.
//!
//! The programs (`apt-get`, `dpkg`, `dpkg-query`, `curl`, the installed binary) run behind
//! [`Host`], so a test answers in place of them; `--root` is where the checked system is (`/`),
//! so a test points it at a scratch directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::apt::state::PACKAGE;
use super::aws::Answer;
use super::{Error, is_plain_version};

/// Where the package puts the binary (below the root).
pub const BINARY: &str = "usr/bin/js-agent";

/// The eight files of the package, below the root, as the package lists them (the same paths, in
/// the same order, as the table of `release deb`).
pub const FILES: [&str; 8] = [
    BINARY,
    "usr/lib/systemd/system/js-agent-crun.service",
    "usr/lib/systemd/system/js-agent-crun-gpu.service",
    "usr/lib/tmpfiles.d/js-agent-crun.conf",
    "usr/lib/systemd/system/js-agent.service",
    "usr/lib/tmpfiles.d/js-agent.conf",
    "usr/share/js-agent/crun_host_setup.sh",
    "usr/share/doc/js-agent/copyright",
];

/// The units of the package: the package enables none of them.
pub const UNITS: [&str; 3] = [
    "js-agent-crun.service",
    "js-agent-crun-gpu.service",
    "js-agent.service",
];

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

/// The checks of an installed system, as problems.
fn installed_problems(r: &Apt, installed: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if installed != r.version {
        problems.push(format!(
            "dpkg says {PACKAGE} {installed:?} is installed, not {:?}",
            r.version
        ));
    }
    for rel in FILES {
        if !r.root.join(rel).is_file() {
            problems.push(format!("the package's file /{rel} is not installed"));
        }
    }
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
        "check-apt: {PACKAGE} {} installed from the suite testing on {}; it says {said:?}; its {} files are there; no unit is enabled and nothing runs",
        r.version,
        r.arch,
        FILES.len()
    )])
}

#[cfg(test)]
mod tests;
