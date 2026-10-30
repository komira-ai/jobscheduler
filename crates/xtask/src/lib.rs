//! `cargo xtask <verb>`: the repository's CI logic, in Rust with tests, so each workflow
//! step is one command (the alias is in `.cargo/config.toml`).
//!
//! - `fetch --url <url> --sha256 <hex> --out <path> [--extract tar.gz --member <path>]`
//!   ([`fetch`]): a pinned tool, downloaded over HTTPS and refused unless its SHA-256
//!   matches, before anything is unpacked or written.
//!
//! - `release plan --head <sha>` and `release stamp --to <version>`
//!   ([`release`]): whether a commit of main is released and as which version, and setting
//!   that version in the CI checkout before the build (docs/release.md).
//!
//! - `deploy --service NAME... --version X.Y.Z --url <coordinator>` ([`deploy`]): the gamma
//!   step of a release, setting the version of each service on the update coordinator in
//!   order and waiting for it to converge (docs/release.md).
//! - `crun-tests [--sudo <path>] [--root <dir>] [--job-user <name>]` ([`crun_tests`]): the
//!   real-`crun` tests of `js-contain` (every one `#[ignore]`d under a plain `cargo test`),
//!   each test target built as this user and run in a `Delegate=yes` system unit of its own;
//!   with `--job-user`, the targets of a root daemon, run as root, which start every
//!   container as that user.
//! - `pkg check-deb --deb F --version X.Y.Z --binary B --packaging DIR --license F` ([`pkg`]):
//!   refuses a js-agent deb that is not exactly the package `packaging/js-agent` describes, or
//!   whose binary is not a static x86-64 Linux executable.
//! - [`layering`]: the workspace's layer rules (`layers.toml`), used by the `layering`
//!   test rather than a verb: `cargo test -p xtask --test layering`.
//!
//! Exit status: 0 when the verb passed, 1 when it failed (one reason on stderr), 2 on a
//! usage error.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub mod crun_tests;
pub mod deploy;
pub mod fetch;
pub mod layering;
pub mod pkg;
pub mod release;
pub mod test_build;

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A release verb failed.
    #[error(transparent)]
    Release(#[from] release::Error),
    /// The deploy verb failed.
    #[error(transparent)]
    Deploy(#[from] deploy::Error),
    /// The fetch verb failed.
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    /// A package check failed.
    #[error(transparent)]
    Pkg(#[from] pkg::Error),
    /// The real-crun tests, or the build of their targets, failed.
    #[error(transparent)]
    CrunTests(#[from] test_build::Error),
    /// Flags that do not fit together (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
}

/// The command line: one verb.
#[derive(Parser)]
#[command(name = "cargo xtask", about = "The repository's CI steps")]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

/// The verbs.
#[derive(Subcommand)]
enum Verb {
    /// Download a pinned file, check its SHA-256 and install it (mode 0755).
    Fetch {
        /// Where to download it from (HTTPS; redirects must stay on HTTPS).
        #[arg(long)]
        url: String,
        /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
        #[arg(long)]
        sha256: fetch::Sha256,
        /// The file to install; its directory is created when missing.
        #[arg(long)]
        out: PathBuf,
        /// How the download is packed (default: it is the file).
        #[arg(long, value_enum)]
        extract: Option<fetch::Format>,
        /// The file to take out of a tar (`--extract tar.gz`): its path inside the archive.
        #[arg(long)]
        member: Option<PathBuf>,
    },
    /// Set the release's version for services on the update coordinator and wait for them.
    Deploy(deploy::Args),
    /// Run the real-crun tests of js-contain, each test target in its own `Delegate=yes`
    /// system unit as this user (a Linux host with crun, uidmap and passwordless sudo).
    CrunTests {
        /// The sudo program; it is run with `-n`, so it never prompts.
        #[arg(long, default_value = "sudo")]
        sudo: PathBuf,
        /// The repository root, where cargo builds the test targets.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Run the tests of a root daemon instead: each target in a unit as root, starting
        /// every container as this job user (an existing account with a subordinate id
        /// range).
        #[arg(long)]
        job_user: Option<String>,
    },
    /// The js-agent Linux package.
    Pkg {
        /// Which step.
        #[command(subcommand)]
        verb: PkgVerb,
    },
    /// The continuous release's decisions.
    Release {
        /// Which one.
        #[command(subcommand)]
        verb: ReleaseVerb,
    },
}

/// The `pkg` verbs.
#[derive(Subcommand)]
enum PkgVerb {
    /// Refuse a deb that is not exactly the js-agent package.
    CheckDeb(pkg::CheckArgs),
}

/// The `release` verbs.
#[derive(Subcommand)]
enum ReleaseVerb {
    /// Whether a commit of main is released, and as which version.
    Plan(release::plan::PlanArgs),
    /// Set the release version in this checkout (not committed).
    Stamp(release::stamp::StampArgs),
}

/// Runs the verb `cli` names: the line to print when it passed.
fn run(cli: Cli) -> Result<String, Error> {
    match cli.verb {
        Verb::Fetch {
            url,
            sha256,
            out,
            extract,
            member,
        } => {
            let unpack = fetch::Unpack::new(extract, member).map_err(Error::Usage)?;
            let roots = fetch::host_roots();
            Ok(fetch::fetch(&url, &sha256, &unpack, &out, &roots)?)
        }
        Verb::Deploy(args) => Ok(deploy::run(&args)?),
        Verb::CrunTests {
            sudo,
            root,
            job_user,
        } => {
            // `cargo xtask` runs this program under cargo, which sets `CARGO`.
            let cargo = std::env::var_os("CARGO").unwrap_or_default();
            Ok(crun_tests::run(&crun_tests::CrunTests {
                cargo: &cargo,
                sudo: &sudo,
                root: &root,
                job_user: job_user.as_deref(),
            })?)
        }
        Verb::Pkg { verb } => Ok(match verb {
            PkgVerb::CheckDeb(args) => pkg::check(&args)?,
        }),
        Verb::Release { verb } => Ok(match verb {
            ReleaseVerb::Plan(args) => release::plan::run(&args)?,
            ReleaseVerb::Stamp(args) => release::stamp::run(&args)?,
        }),
    }
}

/// Prints `result` (the line on `out`, the reason on `err`) and picks the exit status.
pub fn report(result: Result<String, Error>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(line) => {
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(why)) => {
            let _ = writeln!(err, "xtask: {why}");
            ExitCode::from(2)
        }
        Err(e) => {
            let _ = writeln!(err, "xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The program: parses the arguments, runs the verb and prints its result.
pub fn main() -> ExitCode {
    let result = run(Cli::parse());
    report(result, &mut std::io::stdout(), &mut std::io::stderr())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outcome of [`report`] as text: the exit status, stdout and stderr.
    fn reported(result: Result<String, Error>) -> (String, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = report(result, &mut out, &mut err);
        (
            format!("{code:?}"),
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    }

    /// Asserts that a passed verb prints its line on stdout and exits 0, and that a usage
    /// error exits 2 and any other failure exits 1, each with its reason on stderr only.
    #[test]
    fn report_maps_each_result_to_a_status_and_a_stream() {
        let (code, out, err) = reported(Ok("done".to_owned()));
        assert_eq!(code, format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(out, "done\n");
        assert_eq!(err, "");

        let (code, out, err) = reported(Err(Error::Usage("bad flags".to_owned())));
        assert_eq!(code, format!("{:?}", ExitCode::from(2)));
        assert_eq!(out, "");
        assert_eq!(err, "xtask: bad flags\n");

        let failed = fetch::Error::NoMember(PathBuf::from("a/b"));
        let (code, out, err) = reported(Err(Error::Fetch(failed)));
        assert_eq!(code, format!("{:?}", ExitCode::FAILURE));
        assert_eq!(out, "");
        assert_eq!(err, "xtask: the archive has no \"a/b\"\n");
    }
}
