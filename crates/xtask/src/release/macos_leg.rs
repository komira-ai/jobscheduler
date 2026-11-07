//! `release macos-leg --target <triple> --builds "<package>:<bin> ..." --min-macos <MAJOR.MINOR>
//! --out <dir>`: the darwin leg of a release, as the pull request's `macos` job runs it
//! (`.github/workflows/ci.yml`) and as `release.yml` builds it.
//!
//! For each `<package>:<bin>` of `--builds`, in order: `cargo build --locked --release --target
//! <triple> -p <package> --bin <bin>` in `--repo-dir`; the archive's directory as `release stage`
//! lays it down (`<out>/stage-<bin>`); the binary's minimum macOS, read from `otool -l`, which
//! must be there and not above `--min-macos`; and `<bin> --version`, whose first line must end
//! with the workspace version. After the last, the hostkeeper package of `js-agent`
//! ([`super::hostkeeper_package`]) is written to `--out`. The first failure ends the run.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::hostkeeper_package::{self, MIN_HOSTKEEPER, PackageArgs};
use super::stage::{self, StageArgs};
use super::{Error, Version, read, workspace_version};

/// The binary the hostkeeper package is made of.
const AGENT: &str = "js-agent";

/// The arguments of `release macos-leg`.
#[derive(clap::Args)]
pub struct LegArgs {
    /// The target to build for (`aarch64-apple-darwin`).
    #[arg(long)]
    pub target: String,
    /// Space-separated `<package>:<bin>` pairs, built in this order; one is `js-agent`.
    #[arg(long)]
    pub builds: String,
    /// The oldest macOS the binaries may need, `MAJOR.MINOR`: a higher `minos` fails.
    #[arg(long)]
    pub min_macos: String,
    /// The directory the stage directories and the hostkeeper package are made in.
    #[arg(long)]
    pub out: PathBuf,
    /// The checkout that is built; the workspace version is read from its `Cargo.toml`.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// The program that builds (`cargo`).
    #[arg(long, default_value = "cargo")]
    pub cargo: String,
    /// The program that lists load commands (`otool`).
    #[arg(long, default_value = "otool")]
    pub otool: String,
}

/// One `<package>:<bin>` of `--builds`.
struct Build<'a> {
    package: &'a str,
    bin: &'a str,
}

/// The entries of `--builds`.
fn parse_builds(builds: &str) -> Result<Vec<Build<'_>>, Error> {
    builds
        .split_whitespace()
        .map(|entry| {
            entry
                .split_once(':')
                .filter(|(package, bin)| !package.is_empty() && !bin.is_empty())
                .map(|(package, bin)| Build { package, bin })
                .ok_or_else(|| Error::BadBuild(entry.to_owned()))
        })
        .collect()
}

/// A dotted number list without its trailing zeros, so `11` and `11.0` are equal and
/// `10.15` is below `11.0`: numbers, not text.
fn dotted(text: &str) -> Result<Vec<u64>, Error> {
    let mut parts = text
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::BadMacosVersion(text.to_owned()))?;
    while parts.last() == Some(&0) {
        parts.pop();
    }
    Ok(parts)
}

/// The `minos` after the first `LC_BUILD_VERSION` of `otool -l` output.
fn minos(listing: &str) -> Option<&str> {
    listing
        .lines()
        .skip_while(|line| !line.contains("LC_BUILD_VERSION"))
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next() == Some("minos") {
                fields.next()
            } else {
                None
            }
        })
}

/// Runs `program args`: its standard output, when it started and succeeded.
fn capture(program: &str, args: &[&str]) -> Result<String, Error> {
    let command = format!("{program} {}", args.join(" "));
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Spawn {
            program: program.to_owned(),
            why: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(Error::Failed {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Builds one binary, its output going to this process's own.
fn build(a: &LegArgs, b: &Build<'_>) -> Result<(), Error> {
    let args = [
        "build",
        "--locked",
        "--release",
        "--target",
        &a.target,
        "-p",
        b.package,
        "--bin",
        b.bin,
    ];
    let command = format!("{} {}", a.cargo, args.join(" "));
    let status = Command::new(&a.cargo)
        .args(args)
        .current_dir(&a.repo_dir)
        .status()
        .map_err(|e| Error::Spawn {
            program: a.cargo.clone(),
            why: e.to_string(),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::BuildFailed {
            command,
            status: status.to_string(),
        })
    }
}

/// Checks the binary at `path`: `minos` within `floor`, and a `--version` of `version`.
fn check(
    a: &LegArgs,
    b: &Build<'_>,
    path: &Path,
    floor: &[u64],
    version: Version,
) -> Result<Vec<String>, Error> {
    let shown = path.display().to_string();
    let listing = capture(&a.otool, &["-l", &shown])?;
    let found = minos(&listing).ok_or_else(|| Error::NoMinos {
        bin: b.bin.to_owned(),
    })?;
    if dotted(found)?.as_slice() > floor {
        return Err(Error::MinosTooHigh {
            bin: b.bin.to_owned(),
            minos: found.to_owned(),
            floor: a.min_macos.clone(),
        });
    }
    let output = capture(&shown, &["--version"])?;
    let printed = output.lines().next().unwrap_or_default();
    if !printed.ends_with(&format!(" {version}")) {
        return Err(Error::WrongVersionLine {
            bin: b.bin.to_owned(),
            printed: printed.to_owned(),
            version: version.to_string(),
        });
    }
    Ok(vec![
        format!("{} minos {found}, not above {}", b.bin, a.min_macos),
        format!("{} prints {printed:?}", b.bin),
    ])
}

/// Runs `release macos-leg`: one line per check passed.
///
/// The entries, the agent among them, the floor and the workspace version are judged before
/// anything is built.
///
/// # Errors
/// [`Error::BadBuild`], [`Error::NoAgentBuild`], [`Error::BadMacosVersion`], [`Error::Io`] or
/// [`Error::Manifest`] for the inputs; [`Error::Spawn`] and [`Error::BuildFailed`] for the
/// build; whatever `release stage` and `release hostkeeper-package` fail with;
/// [`Error::Failed`] and [`Error::Spawn`] for `otool` and the binary; [`Error::NoMinos`],
/// [`Error::MinosTooHigh`], [`Error::WrongVersionLine`].
pub fn run(a: &LegArgs) -> Result<String, Error> {
    let builds = parse_builds(&a.builds)?;
    let agent = builds
        .iter()
        .find(|b| b.bin == AGENT)
        .ok_or(Error::NoAgentBuild)?;
    let floor = dotted(&a.min_macos)?;
    let version = workspace_version(&read(&a.repo_dir.join("Cargo.toml"))?)?;
    let binary_of = |b: &Build<'_>| {
        a.repo_dir
            .join("target")
            .join(&a.target)
            .join("release")
            .join(b.bin)
    };
    let mut lines = Vec::new();
    for b in &builds {
        build(a, b)?;
        lines.push(format!("{} built", b.bin));
        lines.push(stage::run(&StageArgs {
            bin: b.bin.to_owned(),
            target: a.target.clone(),
            version,
            out: a.out.join(format!("stage-{}", b.bin)),
            repo_dir: a.repo_dir.clone(),
        })?);
        lines.extend(check(a, b, &binary_of(b), &floor, version)?);
    }
    lines.push(hostkeeper_package::run(&PackageArgs {
        binary: binary_of(agent),
        target: a.target.clone(),
        version: version.to_string(),
        min_hostkeeper: MIN_HOSTKEEPER.to_owned(),
        out: a.out.clone(),
    })?);
    Ok(lines.join("\n"))
}
