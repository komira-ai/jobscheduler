//! `cargo xtask <verb>`: the repository's CI logic, in Rust with tests, so each workflow
//! step is one command (the alias is in `.cargo/config.toml`).
//!
//! - `fetch --url <url> --sha256 <hex> --out <path> [--extract tar.gz --member <path>]`
//!   ([`fetch`]): a pinned tool, downloaded over HTTPS and refused unless its SHA-256
//!   matches, before anything is unpacked or written.
//!
//! - `release plan --head <sha>`, `release stamp --to <version>` and `release stage --bin <name>
//!   --target <triple> --version <version> --out <dir>` ([`release`]): whether a commit of main
//!   is released and as which version, setting that version in the CI checkout before the build,
//!   and laying down the files of each release archive (docs/release.md).
//!
//! - `deploy --service NAME... --version X.Y.Z --url <coordinator>` ([`deploy`]): the gamma
//!   step of a release, setting the version of each service on the update coordinator in
//!   order and waiting for it to converge (docs/release.md).
//! - `crun-tests [--sudo <path>] [--root <dir>] [--job-user <name>]` ([`crun_tests`]): the
//!   real-`crun` tests of `js-contain` (every one `#[ignore]`d under a plain `cargo test`),
//!   each test target built as this user and run in a `Delegate=yes` system unit of its own;
//!   with `--job-user`, the targets of a root daemon, run as root, which start every
//!   container as that user.
//! - `unit-result --unit <name> --count-file <path> [--sudo <path>]` ([`crun_tests::check_unit`]): fails unless a
//!   system unit a step ran with `systemd-run --wait` exited 0, by the unit's own result, and
//!   ran at least one test, by the `--count-file` its `count-run` wrote.
//!   Each unit runs its binary through `count-run --count-file <path> -- <binary> <args>...`
//!   ([`count_run`]), which writes the number of tests the binary ran; a target fails unless
//!   that count is at least one.
//! - `pkg check-deb --deb F --version X.Y.Z --binary B --packaging DIR --license F` ([`pkg`]):
//!   refuses a js-agent deb that is not exactly the package `packaging/js-agent` describes, or
//!   whose binary is not a static x86-64 Linux executable.
//! - `pkg stage-macos` and `pkg check-macos` ([`macpkg`]): the macOS installer package's
//!   payload and scripts, laid down for `pkgbuild`, and the built package read back with
//!   `pkgutil` and `lsbom` and refused unless it is exactly that.
//! - `site-demo [--out <path>] [--bin-dir <dir>] [--port <n>] [--ready-timeout <s>]` ([`site_demo`]): records the demonstration on
//!   the landing page of jobscheduler.dev, `site/test/demo.transcript`: builds the lease server and the
//!   client, leases a host from a static host file, lists the lease and releases it.
//! - `site-changed --event <name> --base <ref> --paths <file> --github-output <file>`
//!   ([`site_changed`]): whether the site steps of the `lint` job run, from the event and the
//!   files a pull request changes against the entries of `site/ci-paths`.
//! - `gen-job-surface --out <dir> [--descriptor <path>] [--check]` ([`job_surface`]): the job
//!   commands of `js` and the methods of `js_client::jobs::JobClient`, rendered from the job
//!   service's descriptor set into the two checked-in files; `--check` fails naming a file that
//!   is not current (docs/job-surface.md).
//! - `determinism [--root <dir>]` ([`determinism`]): no clock or random source is read in the
//!   crates a `js-sim-cell` run links, but at the sites its allow-list names, each with a
//!   reason (docs/simulation.md). The test `cargo test -p xtask --test determinism` runs it on
//!   this tree.
//! - `image-recipe --out <dir> [--root <dir>] [--pins <file>]` ([`image`]): the build
//!   description of the machine images of `linux-x86_64` and `linux-arm64` (js-agent, crun and
//!   hostkeeper baked in, each pinned by URL and SHA-256), and the cloud-init seed that builds
//!   each one from its base image (docs/runbooks/machine-images.md).
//! - [`layering`]: the workspace's layer rules (`layers.toml`), used by the `layering`
//!   test rather than a verb: `cargo test -p xtask --test layering`.
//! - [`spawn_sites`]: every program the crates `js-agent` and `js-mac-session` are built
//!   from (themselves and their path dependencies built for macOS) start goes through
//!   `js_mac_session::disclaim`, used by a test rather than a verb: `cargo test -p xtask
//!   --test spawn_sites`.
//!
//! Exit status: 0 when the verb passed, 1 when it failed (one reason on stderr), 2 on a
//! usage error.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub mod count_run;
pub mod crun_tests;
pub mod deploy;
pub mod determinism;
pub mod fetch;
pub mod image;
pub mod job_surface;
pub mod layering;
pub mod macpkg;
pub mod pkg;
pub mod release;
pub mod site_changed;
pub mod site_demo;
pub mod spawn_sites;
pub mod test_build;

/// Why a verb failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A release verb failed.
    #[error(transparent)]
    Release(#[from] release::Error),
    /// The deploy verb failed.
    #[error(transparent)]
    Deploy(#[from] deploy::Error),
    /// The determinism lint failed, or could not read the tree.
    #[error(transparent)]
    Determinism(#[from] determinism::Error),
    /// The fetch verb failed.
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    /// A package check failed.
    #[error(transparent)]
    Pkg(#[from] pkg::Error),
    /// The macOS package could not be staged or checked.
    #[error(transparent)]
    MacPkg(#[from] macpkg::Error),
    /// A program `count-run` ran ended with a failure status, which is also this one's.
    #[error("the program ended with status {0}")]
    Child(u8),
    /// The site demonstration could not be recorded.
    #[error("site-demo: {0}")]
    SiteDemo(#[from] site_demo::Error),
    /// The site decision could not be written.
    #[error(transparent)]
    SiteChanged(#[from] site_changed::Error),
    /// The real-crun tests, or the build of their targets, failed.
    #[error(transparent)]
    CrunTests(#[from] test_build::Error),
    /// The machine image recipes could not be made.
    #[error("image-recipe: {0}")]
    Image(#[from] image::Error),
    /// The job surface could not be generated, or the checked-in files are not current.
    #[error(transparent)]
    JobSurface(#[from] job_surface::GenError),
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
    /// Fail unless every clock and random-source site in the crates the simulation cell
    /// links is on the lint's allow-list, and every entry of the list matches a site.
    Determinism {
        /// The repository root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
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
    /// Fail unless the system unit a step ran with `systemd-run --wait` (without
    /// `--collect`) exited 0, reading the unit's own result rather than `systemd-run`'s
    /// exit status, and clear the unit's failed state.
    UnitResult {
        /// The sudo program; it is run with `-n`, so it never prompts.
        #[arg(long, default_value = "sudo")]
        sudo: PathBuf,
        /// The unit's name (`--unit` of the `systemd-run`).
        #[arg(long)]
        unit: String,
        /// The file the unit's `count-run` wrote the number of tests it ran to: the unit
        /// fails unless it holds a count of at least one.
        #[arg(long)]
        count_file: PathBuf,
    },
    /// Run a test binary and write the number of tests it ran to a file (what a `crun-tests`
    /// unit runs; the exit status is the binary's).
    CountRun {
        /// Where to write the count.
        #[arg(long)]
        count_file: PathBuf,
        /// The test binary and its arguments.
        #[arg(last = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Record the demonstration on the landing page of jobscheduler.dev (a build of the lease
    /// server and the client, a server on 127.0.0.1, a lease acquired, listed and released).
    SiteDemo {
        /// The repository root, where cargo builds.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Where to write the transcript (default: `site/test/demo.transcript` under `--root`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Use the `js-lease-server` and `js` in this directory instead of building them.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// The server's port on 127.0.0.1; the recording shows the default, 7300.
        #[arg(long, default_value_t = site_demo::DEFAULT_PORT)]
        port: u16,
        /// How many seconds to wait for the server to answer.
        #[arg(long, default_value_t = site_demo::READY_TIMEOUT.as_secs())]
        ready_timeout: u64,
        /// The directory the run's scratch directory is made in, and removed from after (default:
        /// `target` under `--root`, cargo's own directory of the workspace).
        #[arg(long)]
        scratch: Option<PathBuf>,
    },
    /// Whether the site steps of the `lint` job run: always off a pull request, and on one when a
    /// changed file matches `site/ci-paths` (writes `run=true` or `run=false`).
    SiteChanged(site_changed::Args),
    /// Render the job commands of js and the job client's methods from the job service's
    /// descriptor set, or with `--check` fail when the checked-in files differ.
    GenJobSurface(job_surface::Args),
    /// Write the build descriptions of the linux-x86_64 and linux-arm64 machine images, and the
    /// cloud-init seed that builds each one (no network: every archive is pinned).
    ImageRecipe(image::Args),
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
    /// Lay the macOS package's payload and scripts down, ready for `pkgbuild`.
    StageMacos(macpkg::StageArgs),
    /// Refuse a macOS package that is not exactly the js-agent package (a Mac only).
    CheckMacos(macpkg::CheckArgs),
}

/// The `release` verbs.
#[derive(Subcommand)]
enum ReleaseVerb {
    /// Whether a commit of main is released, and as which version.
    Plan(release::plan::PlanArgs),
    /// Set the release version in this checkout (not committed).
    Stamp(release::stamp::StampArgs),
    /// Lay down the files of one release archive.
    Stage(release::stage::StageArgs),
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
        Verb::Determinism { root } => Ok(determinism::run(&root)?),
        Verb::CrunTests {
            sudo,
            root,
            job_user,
        } => {
            // `cargo xtask` runs this program under cargo, which sets `CARGO`.
            let cargo = std::env::var_os("CARGO").unwrap_or_default();
            Ok(crun_tests::run(&crun_tests::CrunTests {
                cargo: &cargo,
                sudo: &crun_tests::SudoProgram(&sudo),
                root: &root,
                job_user: job_user.as_deref(),
            })?)
        }
        Verb::UnitResult {
            sudo,
            unit,
            count_file,
        } => Ok(crun_tests::check_unit(
            &crun_tests::SudoProgram(&sudo),
            &unit,
            &count_file,
        )?),
        Verb::CountRun {
            count_file,
            command,
        } => count_run::run(&count_file, &command),
        Verb::SiteDemo {
            root,
            out,
            bin_dir,
            port,
            ready_timeout,
            scratch,
        } => {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let out = out.unwrap_or_else(|| root.join(site_demo::DEFAULT_OUT));
            let scratch = scratch.unwrap_or_else(|| root.join("target"));
            Ok(site_demo::run(&site_demo::SiteDemo {
                cargo: &cargo,
                root: &root,
                bin_dir: bin_dir.as_deref(),
                out: &out,
                scratch: &scratch,
                port,
                ready_timeout: std::time::Duration::from_secs(ready_timeout),
            })?)
        }
        Verb::SiteChanged(args) => Ok(site_changed::run(&args)?),
        Verb::GenJobSurface(args) => Ok(job_surface::run(&args)?),
        Verb::ImageRecipe(args) => Ok(image::run(&args)?),
        Verb::Pkg { verb } => Ok(match verb {
            PkgVerb::CheckDeb(args) => pkg::check(&args)?,
            PkgVerb::StageMacos(args) => macpkg::stage(&args)?,
            PkgVerb::CheckMacos(args) => macpkg::check(&args)?,
        }),
        Verb::Release { verb } => Ok(match verb {
            ReleaseVerb::Plan(args) => release::plan::run(&args)?,
            ReleaseVerb::Stamp(args) => release::stamp::run(&args)?,
            ReleaseVerb::Stage(args) => release::stage::run(&args)?,
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
        Err(Error::Child(code)) => ExitCode::from(code),
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
