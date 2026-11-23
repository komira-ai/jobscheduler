//! `farm-build --buck2 <path> --target <label> --patch <path> --summary <path> --repo <owner/name>
//! --run-id <id>`: the farm lane's build (`.github/workflows/farm.yml`, docs/farm.md), one command.
//! In this order:
//!
//! 1. `buck2 audit config buildrules_re.linux_x86_64_properties --style json` must give the key a
//!    value that is not blank: the worker property set farm-connect wrote, which `.buckconfig`
//!    hands to buildrules. Without one buck2 has no execution platform, and this says so first.
//! 2. The generated build table must be fresh ([`table`]): `buildrules check`, with the
//!    `buildrules` command line built from the pinned buildrules cell on the farm. When it calls the
//!    table stale, the job runs `buildrules sync`, writes what sync changed as a patch to `--patch`
//!    (the workflow uploads it as the artifact [`table::PATCH_ARTIFACT`]) and the stale inputs, the
//!    changed files and the one command that applies the patch to `--summary` (the job summary),
//!    and fails without building.
//! 3. `buck2 build <label>`. What buck2 prints on its standard error (its progress, the output of
//!    a failed action, its summary) is written to the log line by line as buck2 prints it, so a
//!    long build shows its progress in the job's log; a failed build is a failure, but for one
//!    case: when buck2 ended with a status of its own and every failure in what it printed is a
//!    cut connection to the farm ([`cuts`]), the same build runs once more, at once, and a line in
//!    the log and in the job summary says so. The second run is final. Cut again, it fails with
//!    [`Error::CutTwice`]: one cut is a moment's break in the way to the farm, but a second in a
//!    row means the farm or the network to it is down, and a third try would only spend the job's
//!    time and hide an outage behind a slow green.
//! 4. The summary line of the last run, `Commands: N (cached: N, remote: N, local: N)`, must be
//!    there and say `local: 0`: every action ran on the farm or came from its cache, none on this
//!    machine. The line is what the verb prints.
//!
//! Whatever happened, it then runs `buck2 kill`, so no daemon outlives the job; that command's own
//! outcome changes nothing.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

mod cuts;
pub mod table;

pub use table::Handoff;

/// The key of the worker property set buildrules reads.
pub const PROPERTIES_KEY: &str = "buildrules_re.linux_x86_64_properties";

/// Why a farm build failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// buck2 could not be started, or waited for.
    #[error("cannot run {program:?}: {why}")]
    Start {
        /// The program that was to run.
        program: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// `buck2 audit config` ended with a failure status.
    #[error("`buck2 audit config` failed ({status}): {stderr}")]
    Audit {
        /// How it ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// The audit gave the worker property set no value, or a blank one.
    #[error(
        "buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no [komira_re] \
         linux_x86_64_properties, so buck2 has no farm to build on"
    )]
    NoWorkers,
    /// The generated build table is not fresh, or its freshness could not be settled.
    #[error(transparent)]
    Table(#[from] table::Error),
    /// The build ended with a failure status.
    #[error("`buck2 build {target}` failed ({status})")]
    Build {
        /// The label that was built.
        target: String,
        /// How the build ended.
        status: String,
    },
    /// The build was run again after a cut and failed again with every failure a cut.
    #[error(
        "`buck2 build {target}` failed ({status}) on its second run too, every failure again a \
         cut connection to the farm: the farm or the network to it is down; run the job again \
         once the farm answers"
    )]
    CutTwice {
        /// The label that was built.
        target: String,
        /// How the second run ended.
        status: String,
    },
    /// The build passed and printed no readable summary line.
    #[error("the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)")]
    NoSummary,
    /// The summary line says an action ran on this machine; the line is the payload.
    #[error("`{0}`: actions ran on this machine, not on the farm (local must be 0)")]
    Local(String),
}

/// Whether `json`, what `buck2 audit config <key> --style json` printed, gives
/// [`PROPERTIES_KEY`] a value that is not blank.
pub fn has_value(json: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json)
        .ok()
        .and_then(|config| config.get(PROPERTIES_KEY).cloned())
        .and_then(|value| value.as_str().map(|v| !v.trim().is_empty()))
        .unwrap_or(false)
}

/// The last summary line of a build's output, from `Commands: `, and the number after its
/// `local: `; `None` when there is none.
pub fn summary(log: &str) -> Option<(&str, u64)> {
    log.lines().rev().find_map(|line| {
        let line = &line[line.find("Commands: ")?..];
        let (_, after) = line.split_once("local: ")?;
        let digits = after
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or_default();
        Some((line, digits.parse().ok()?))
    })
}

/// The line the log and the job summary get when a build cut `cuts` times runs once more.
pub fn rerun_line(target: &str, cuts: usize) -> String {
    format!(
        "xtask: every failure of `buck2 build {target}` was a cut connection to the farm ({cuts} \
         cut messages); running it once more\n"
    )
}

/// Builds `target` with the buck2 at `buck2`, writing buck2's standard error to `log` as it
/// comes, once the generated build table is fresh, and stops buck2's daemon after: the summary
/// line, or why the build is not a farm build. A stale table leaves its fix where `handoff` says;
/// a build run again after a cut says so in `handoff`'s job summary.
///
/// # Errors
/// buck2 cannot be started, names no worker property set, the table is stale or its check
/// fails, buck2 fails the build, prints no summary line, or ran an action on this machine.
pub fn build(
    buck2: &Path,
    target: &str,
    handoff: &Handoff,
    log: &mut dyn Write,
) -> Result<String, Error> {
    let result = build_and_check(buck2, target, handoff, log);
    // The daemon outlives the command: stop it however the build ended.
    let _ = Command::new(buck2)
        .arg("kill")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    result
}

/// The error for a program that could not be started or waited for.
fn start_error(program: &Path) -> impl FnOnce(std::io::Error) -> Error {
    move |e| Error::Start {
        program: program.to_owned(),
        why: e.to_string(),
    }
}

/// `program <args>` run to its end, its standard input closed.
fn output(program: &Path, args: &[&str]) -> Result<Output, Error> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(start_error(program))
}

/// Every line `from` gives, written to `log` as it comes, and all of them.
fn tee(from: impl std::io::Read, log: &mut dyn Write) -> Vec<u8> {
    let mut lines = BufReader::new(from);
    let (mut line, mut text) = (Vec::new(), Vec::new());
    // A read that fails ends the log; the status of the build is still buck2's.
    while lines.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        // A log that cannot be written loses buck2's words, not the verdict.
        let _ = log.write_all(&line);
        text.append(&mut line);
    }
    text
}

/// One `buck2 build <target>`: how it ended and what it printed on its standard error, which is
/// written to `log` as it comes. What it prints on its standard output goes to this program's
/// standard error, so the verb's own standard output stays the summary line.
fn build_once(
    buck2: &Path,
    target: &str,
    log: &mut dyn Write,
) -> Result<(ExitStatus, String), Error> {
    let mut child = Command::new(buck2)
        .args(["build", target])
        .stdin(Stdio::null())
        .stdout(std::io::stderr())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(start_error(buck2))?;
    let text = child
        .stderr
        .take()
        .map(|stderr| tee(stderr, log))
        .unwrap_or_default();
    let status = child.wait().map_err(start_error(buck2))?;
    Ok((status, String::from_utf8_lossy(&text).into_owned()))
}

/// How many cuts a failed run reports when it may run again: it ended with a status of its own
/// (a buck2 ended by a signal is never run again) and every failure it printed is a cut.
fn cut(status: ExitStatus, text: &str) -> Option<usize> {
    status.code().and(cuts::only_cuts(text))
}

/// The checks and the build, without the daemon's stop.
fn build_and_check(
    buck2: &Path,
    target: &str,
    handoff: &Handoff,
    log: &mut dyn Write,
) -> Result<String, Error> {
    let audit = output(
        buck2,
        &["audit", "config", PROPERTIES_KEY, "--style", "json"],
    )?;
    if !audit.status.success() {
        return Err(Error::Audit {
            status: audit.status.to_string(),
            stderr: String::from_utf8_lossy(&audit.stderr).trim().to_owned(),
        });
    }
    if !has_value(&String::from_utf8_lossy(&audit.stdout)) {
        return Err(Error::NoWorkers);
    }
    table::ensure_fresh(buck2, handoff, log)?;
    let (mut status, mut text) = build_once(buck2, target, log)?;
    if !status.success()
        && let Some(cuts) = cut(status, &text)
    {
        let line = rerun_line(target, cuts);
        // The job summary says so first: a green build that needed a second run is not hidden.
        table::write(&handoff.summary, line.as_bytes(), true)?;
        let _ = log.write_all(line.as_bytes());
        (status, text) = build_once(buck2, target, log)?;
        if !status.success() && cut(status, &text).is_some() {
            return Err(Error::CutTwice {
                target: target.to_owned(),
                status: status.to_string(),
            });
        }
    }
    if !status.success() {
        return Err(Error::Build {
            target: target.to_owned(),
            status: status.to_string(),
        });
    }
    let (line, local) = summary(&text).ok_or(Error::NoSummary)?;
    if local != 0 {
        return Err(Error::Local(line.to_owned()));
    }
    Ok(line.to_owned())
}

#[cfg(test)]
mod tests;
