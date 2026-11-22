//! `js-mac-probe`: records, on a hosted macOS runner, whether the things a native
//! (`sandbox-exec`) runtime relies on work there. It changes no product code.
//!
//! - [`report`]: the machine-readable report; only a probe with an expected outcome can
//!   fail, a probe whose answer is unknown reports what it saw.
//! - [`child`]: the programs the probe runs inside a profile and, as the control, outside,
//!   and the job and children of the launchd probe.
//! - [`launchd`]: the launchd probe's cases, plist, files and judgement.
//! - `macos`: the probes (macOS only): `sandbox-exec`, deny profiles, network under the
//!   off policy, `clonefile`, volumes, `proc_pid_rusage`, the toolchain under a
//!   profile built the design's way, each against a no-sandbox control, and which children
//!   of a launchd job outlive the job (in the user's own domain; `launchd-system` runs the
//!   same as root in the system domain).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub mod child;
pub mod launchd;
#[cfg(target_os = "macos")]
mod macos;
pub mod parse;
pub mod report;

use report::Report;

/// Command line.
#[derive(Debug, Parser)]
#[command(about = "Probes a macOS runner for what a native sandbox-exec runtime relies on")]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The modes.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run every probe and print the report as JSON; exit 1 if an asserted probe failed.
    Report {
        /// A scratch parent directory on the volume the runtime would use (the runner's temp).
        #[arg(long)]
        work: PathBuf,
        /// The user's home directory.
        #[arg(long)]
        home: PathBuf,
        /// Also write the report to this file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the sandbox trace of the toolchain steps to this file.
        #[arg(long)]
        trace_out: Option<PathBuf>,
    },
    /// As root: the launchd probe in the system domain, with a child that drops to `--drop`;
    /// print the report as JSON and exit 1 if an asserted probe failed.
    LaunchdSystem {
        /// A directory every uid may traverse, for the probe's own directory.
        #[arg(long)]
        work: PathBuf,
        /// The `<uid>:<gid>` the dropped child runs as.
        #[arg(long)]
        drop: launchd::Ids,
        /// Also write the report to this file.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// A program the probe runs inside a sandbox profile.
    #[command(subcommand)]
    Child(child::Child),
}

/// Runs the command line.
#[must_use]
pub fn run(cli: &Cli) -> ExitCode {
    match &cli.command {
        Command::Child(child) => child::run(child),
        Command::Report {
            work,
            home,
            out,
            trace_out,
        } => emit(&collect(work, home, trace_out.as_deref()), out.as_deref()),
        Command::LaunchdSystem { work, drop, out } => {
            emit(&collect_launchd_system(work, *drop), out.as_deref())
        }
    }
}

fn emit(report: &Report, out: Option<&std::path::Path>) -> ExitCode {
    let mut json = Vec::new();
    let written = report
        .write_json(&mut json)
        .and_then(|()| std::io::Write::write_all(&mut std::io::stdout().lock(), &json))
        .and_then(|()| out.map_or(Ok(()), |path| std::fs::write(path, &json)));
    let failed = report.failed();
    for name in &failed {
        eprintln!("FAILED: {name}");
    }
    if let Err(err) = written {
        eprintln!("cannot write the report: {err}");
        return ExitCode::FAILURE;
    }
    [ExitCode::SUCCESS, ExitCode::FAILURE][usize::from(!failed.is_empty())]
}

#[cfg(target_os = "macos")]
fn collect(
    work: &std::path::Path,
    home: &std::path::Path,
    trace_out: Option<&std::path::Path>,
) -> Report {
    macos::collect(work, home, trace_out)
}

#[cfg(target_os = "macos")]
fn collect_launchd_system(work: &std::path::Path, drop: launchd::Ids) -> Report {
    macos::collect_launchd_system(work, drop)
}

#[cfg(not(target_os = "macos"))]
fn collect(
    _work: &std::path::Path,
    _home: &std::path::Path,
    _trace_out: Option<&std::path::Path>,
) -> Report {
    not_macos()
}

#[cfg(not(target_os = "macos"))]
fn collect_launchd_system(_work: &std::path::Path, _drop: launchd::Ids) -> Report {
    not_macos()
}

#[cfg(not(target_os = "macos"))]
fn not_macos() -> Report {
    let mut report = Report::default();
    report.push(report::Probe::asserted("platform/macos", false).fact("os", std::env::consts::OS));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("js-mac-probe").chain(args.iter().copied())).unwrap()
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_the_report_fails_the_platform_probe() {
        let out =
            std::env::temp_dir().join(format!("js-mac-probe-{}-report.json", std::process::id()));
        let cli = parse(&[
            "report",
            "--work",
            "/w",
            "--home",
            "/h",
            "--out",
            out.to_str().unwrap(),
        ]);
        assert_eq!(run(&cli), ExitCode::FAILURE);
        let json = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert!(json.contains("\"platform/macos\""), "{json}");
        assert!(json.contains("\"verdict\": \"fail\""), "{json}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_report_to_an_unwritable_file_fails() {
        let out = std::env::temp_dir()
            .join("js-mac-probe-no-such-dir")
            .join("report.json");
        let cli = parse(&[
            "report",
            "--work",
            "/w",
            "--home",
            "/h",
            "--out",
            out.to_str().unwrap(),
        ]);
        assert_eq!(run(&cli), ExitCode::FAILURE);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_report_without_a_file_goes_to_stdout_only() {
        let cli = parse(&["report", "--work", "/w", "--home", "/h"]);
        assert_eq!(run(&cli), ExitCode::FAILURE);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_the_launchd_system_probe_fails_the_platform_probe() {
        let out = std::env::temp_dir().join(format!(
            "js-mac-probe-{}-launchd-system.json",
            std::process::id()
        ));
        let cli = parse(&[
            "launchd-system",
            "--work",
            "/w",
            "--drop",
            "4294967294:4294967294",
            "--out",
            out.to_str().unwrap(),
        ]);
        assert_eq!(run(&cli), ExitCode::FAILURE);
        let json = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert!(json.contains("\"platform/macos\""), "{json}");
    }

    #[test]
    fn launchd_system_refuses_ids_that_are_not_uid_colon_gid() {
        let parsed = Cli::try_parse_from([
            "js-mac-probe",
            "launchd-system",
            "--work",
            "/w",
            "--drop",
            "nobody",
        ]);
        let err = parsed.unwrap_err().to_string();
        assert!(
            err.contains("expected <uid>:<gid> (two numbers), got \"nobody\""),
            "{err}"
        );
    }

    #[test]
    fn the_child_modes_are_reachable_from_the_command_line() {
        let cli = parse(&["child", "resolve", "localhost"]);
        assert_eq!(run(&cli), ExitCode::SUCCESS);
    }
}
