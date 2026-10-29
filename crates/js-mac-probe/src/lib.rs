//! `js-mac-probe`: records, on a hosted macOS runner, whether the things a native
//! (`sandbox-exec`) runtime relies on work there. It changes no product code.
//!
//! - [`report`]: the machine-readable report; only a probe with an expected outcome can
//!   fail, a probe whose answer is unknown reports what it saw.
//! - [`child`]: the programs the probe runs inside a profile and, as the control, outside.
//! - `macos`: the probes (macOS only): `sandbox-exec`, deny profiles, network under the
//!   off policy, `clonefile`, volumes, `proc_pid_rusage`, and the toolchain under a
//!   profile built the design's way, each against a no-sandbox control.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub mod child;
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
        Command::Report { work, home, out } => report_command(work, home, out.as_deref()),
    }
}

fn report_command(
    work: &std::path::Path,
    home: &std::path::Path,
    out: Option<&std::path::Path>,
) -> ExitCode {
    let report = collect(work, home);
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
fn collect(work: &std::path::Path, home: &std::path::Path) -> Report {
    macos::collect(work, home)
}

#[cfg(not(target_os = "macos"))]
fn collect(_work: &std::path::Path, _home: &std::path::Path) -> Report {
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

    #[test]
    fn the_child_modes_are_reachable_from_the_command_line() {
        let cli = parse(&["child", "resolve", "localhost"]);
        assert_eq!(run(&cli), ExitCode::SUCCESS);
    }
}
