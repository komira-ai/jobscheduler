//! `pkg build-macos --version <X.Y.Z>`: the js-agent installer package of one version, built in
//! the current directory, the checkout of a Mac, once `release stamp` has set the version.
//!
//! It is `.github/actions/js-agent-macos-pkg/action.yml`'s unsigned path after the stamp, as one
//! command, in the same order: the three programs are built with `cargo build --locked
//! --release`; each prints a first `--version` line that ends with ` <version>`; the cdhash of
//! `target/release/js-agent` is read from `codesign --display --verbose=4` (40 lower-case hex
//! digits); the three plists are written to `inputs/` (the session plist names that cdhash as
//! the requirement of the daemon it admits) and linted with `plutil`; the payload is staged in
//! `stage/` as `pkg stage-macos` stages it, and the staged `js-agent` must have the same cdhash;
//! `pkgbuild` and `productbuild` make `dist/js-agent-<version>-macos-arm64.pkg`; the package is
//! read back as `pkg check-macos` reads it; and the sums are written: `dist/SHA256SUMS` for the
//! package and `target/release/PROGRAMS.SHA256SUMS` for the three programs, each line
//! `<sha256>`, two spaces, the file name, as `shasum -a 256` prints it.
//!
//! Every program it starts is a flag with the default the action uses. The first failure ends
//! the run.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::{PREFIX, StageArgs, inspect_with, stage, verdict};
use crate::release::Version;

/// The programs the package carries, built in this order.
const PROGRAMS: [&str; 3] = ["js-agent", "js-mac-session", "js-mac-provision"];

/// Where the programs are built.
const BIN_DIR: &str = "target/release";

/// The arguments of `pkg build-macos`.
#[derive(clap::Args)]
pub struct BuildArgs {
    /// The package version, `MAJOR.MINOR.PATCH`, which the checkout is stamped with.
    #[arg(long)]
    pub version: Version,
    /// The program that builds (`cargo`).
    #[arg(long, default_value = "cargo")]
    pub cargo: String,
    /// The program that reads a code signature (`/usr/bin/codesign`).
    #[arg(long, default_value = "/usr/bin/codesign")]
    pub codesign: String,
    /// The program that lints the plists (`plutil`).
    #[arg(long, default_value = "plutil")]
    pub plutil: String,
    /// The program that builds the component package (`pkgbuild`).
    #[arg(long, default_value = "pkgbuild")]
    pub pkgbuild: String,
    /// The program that builds the product archive (`productbuild`).
    #[arg(long, default_value = "productbuild")]
    pub productbuild: String,
    /// The program that expands the package to read it back (`pkgutil`).
    #[arg(long, default_value = "pkgutil")]
    pub pkgutil: String,
    /// The program that lists the package's bill of materials (`lsbom`).
    #[arg(long, default_value = "lsbom")]
    pub lsbom: String,
}

/// Why `pkg build-macos` failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A program could not be started.
    #[error("{program}: {why}")]
    Spawn {
        /// The program.
        program: String,
        /// What the system said.
        why: String,
    },
    /// A program ended with a failure status.
    #[error("`{command}` ended with {status}: {stderr}")]
    Failed {
        /// The command line.
        command: String,
        /// How it ended.
        status: String,
        /// What it wrote to its standard error.
        stderr: String,
    },
    /// A file or directory could not be made, read or written.
    #[error("{path}: {why}")]
    Io {
        /// The path.
        path: String,
        /// What the system said.
        why: String,
    },
    /// A program's first `--version` line does not end with the version.
    #[error("{bin} prints '{printed}', not {version}")]
    WrongVersionLine {
        /// The program.
        bin: String,
        /// The line it printed.
        printed: String,
        /// The version it must print.
        version: String,
    },
    /// The built `js-agent` has no cdhash of 40 lower-case hex digits.
    #[error("the cdhash of js-agent is '{found}', not 40 hex digits")]
    BadCdhash {
        /// What `codesign` reported, empty when it reported none.
        found: String,
    },
    /// The staged `js-agent` has another cdhash than the built one.
    #[error("the staged js-agent has the cdhash '{staged}', not '{built}'")]
    StagedCdhash {
        /// The cdhash of the staged program.
        staged: String,
        /// The cdhash of the built program.
        built: String,
    },
    /// The payload could not be staged, or the package is not the package.
    #[error(transparent)]
    Package(#[from] super::Error),
}

/// The failure of an operation on `path`, for `map_err`.
fn failure(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |why| Error::Io {
        path: path.display().to_string(),
        why: why.to_string(),
    }
}

/// Runs `program` with `args` in the current directory: what it wrote, when it succeeded.
fn run_program(program: &str, args: &[&str]) -> Result<std::process::Output, Error> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Spawn {
            program: program.to_owned(),
            why: e.to_string(),
        })?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(Error::Failed {
            command: format!("{program} {}", args.join(" ")),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        })
    }
}

/// Builds one program, as the action does.
fn build_program(a: &BuildArgs, bin: &str) -> Result<(), Error> {
    run_program(
        &a.cargo,
        &["build", "--locked", "--release", "-p", bin, "--bin", bin],
    )
    .map(drop)
}

/// The line `bin --version` prints first, which must end with the version.
fn check_version_line(bin: &str, version: Version) -> Result<String, Error> {
    let path = format!("{BIN_DIR}/{bin}");
    let out = run_program(&path, &["--version"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let printed = text.lines().next().unwrap_or_default();
    if printed.ends_with(&format!(" {version}")) {
        Ok(format!("{bin} prints {printed:?}"))
    } else {
        Err(Error::WrongVersionLine {
            bin: bin.to_owned(),
            printed: printed.to_owned(),
            version: version.to_string(),
        })
    }
}

/// The cdhash `codesign` reports for `binary`: the `CDHash=` lines of what it writes, joined.
fn cdhash(a: &BuildArgs, binary: &str) -> Result<String, Error> {
    let out = run_program(&a.codesign, &["--display", "--verbose=4", binary])?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(text
        .lines()
        .filter_map(|line| line.strip_prefix("CDHash="))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The cdhash of `js-agent` at `binary`, which must be 40 lower-case hex digits.
fn agent_cdhash(a: &BuildArgs, binary: &str) -> Result<String, Error> {
    let found = cdhash(a, binary)?;
    if found.len() == 40
        && found
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(found)
    } else {
        Err(Error::BadCdhash { found })
    }
}

/// Writes what `js-mac-provision` prints for `args` to `file`.
fn write_plist(args: &[&str], file: &str) -> Result<(), Error> {
    let out = run_program(&format!("{BIN_DIR}/js-mac-provision"), args)?;
    std::fs::write(file, out.stdout).map_err(failure(Path::new(file)))
}

/// The line of `shasum -a 256` for the file `name` in `dir`.
fn sum_line(dir: &Path, name: &str) -> Result<String, Error> {
    let path = dir.join(name);
    let bytes = std::fs::read(&path).map_err(failure(&path))?;
    Ok(format!("{}  {name}\n", hex::encode(Sha256::digest(bytes))))
}

/// Writes the sums of `names`, in `dir`, to `file` in `dir`.
fn write_sums(dir: &Path, names: &[&str], file: &str) -> Result<(), Error> {
    let mut text = String::new();
    for name in names {
        text.push_str(&sum_line(dir, name)?);
    }
    let path = dir.join(file);
    std::fs::write(&path, text).map_err(failure(&path))
}

/// Builds the package in the current directory: one line for each step that passed.
///
/// # Errors
/// [`Error`]: a program does not start or fails, a program prints another version, the cdhash of
/// `js-agent` is not 40 hex digits or changes when it is staged, a directory it makes is
/// already there, or the package is not the package.
pub fn run(a: &BuildArgs) -> Result<String, Error> {
    let version = a.version.to_string();
    let mut lines = Vec::new();
    for bin in PROGRAMS {
        build_program(a, bin)?;
        lines.push(format!("{bin} built"));
    }
    for bin in PROGRAMS {
        lines.push(check_version_line(bin, a.version)?);
    }
    let agent = format!("{BIN_DIR}/js-agent");
    let built = agent_cdhash(a, &agent)?;
    std::fs::create_dir("inputs").map_err(failure(Path::new("inputs")))?;
    write_plist(&["agent-plist"], "inputs/agent.plist")?;
    write_plist(
        &[
            "session-plist",
            "--daemon-requirement",
            &format!("cdhash H\"{built}\""),
        ],
        "inputs/session.plist",
    )?;
    write_plist(&["pf-pool-plist"], "inputs/pf-pool.plist")?;
    run_program(
        &a.plutil,
        &[
            "-lint",
            "inputs/agent.plist",
            "inputs/session.plist",
            "inputs/pf-pool.plist",
        ],
    )?;
    lines.push(stage(&StageArgs {
        version: a.version,
        bin_dir: PathBuf::from(BIN_DIR),
        license: PathBuf::from("LICENSE"),
        agent_plist: PathBuf::from("inputs/agent.plist"),
        session_plist: PathBuf::from("inputs/session.plist"),
        pf_pool_plist: PathBuf::from("inputs/pf-pool.plist"),
        out: PathBuf::from("stage"),
    })?);
    let staged = cdhash(a, &format!("stage/root/{PREFIX}/bin/js-agent"))?;
    if staged != built {
        return Err(Error::StagedCdhash { staged, built });
    }
    // `dist` may exist already: a remote action's runner makes the parent directory of each
    // output it was told of (`mac-farm --output dist/...`) before the command starts.
    std::fs::create_dir_all("dist").map_err(failure(Path::new("dist")))?;
    let name = format!("js-agent-{version}-macos-arm64.pkg");
    let pkg = format!("dist/{name}");
    run_program(
        &a.pkgbuild,
        &[
            "--root",
            "stage/root",
            "--identifier",
            super::IDENTIFIER,
            "--version",
            &version,
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            "stage/component.pkg",
        ],
    )?;
    run_program(
        &a.productbuild,
        &[
            "--package",
            "stage/component.pkg",
            "--version",
            &version,
            &pkg,
        ],
    )?;
    let inspected = inspect_with(Path::new(&pkg), &a.pkgutil, &a.lsbom)?;
    lines.push(verdict(&inspected, &version, Path::new(&pkg))?);
    write_sums(Path::new("dist"), &[&name], "SHA256SUMS")?;
    write_sums(Path::new(BIN_DIR), &PROGRAMS, "PROGRAMS.SHA256SUMS")?;
    lines.push(format!("summed {name} and {}", PROGRAMS.join(", ")));
    Ok(lines.join("\n"))
}
