//! `release check-code-signature <binary> --program <name> [--codesign <program>]`: refuses a
//! macOS program unless it carries the signature the release gives it, as
//! [`super::mac_programs`] says.
//!
//! The release signs `js-agent`, `js-mac-session`, `js-mac-provision` and `js-vmm` with the team's
//! Developer ID Application identity, a hardened runtime and a secure timestamp, each under
//! its own identifier. This verb reads that back with `codesign` (behind the [`Codesign`]
//! port, so the judgement is tested with a fake and the real adapter is the system tool),
//! three reads in order:
//!
//! 1. `codesign --verify --strict --verbose=2 <binary>`: the signature is valid and its seal
//!    holds. A failure ends the check.
//! 2. `codesign --display --verbose=2 <binary>`: the identifier, the team, the signature
//!    kind, the code directory's flags, the authority chain and the timestamp.
//! 3. `codesign --display --requirements - <binary>`: the designated requirement.
//!
//! It refuses an ad hoc signature, another identifier or team, an authority chain that is not
//! exactly [`super::mac_programs::AUTHORITIES`], a signature with no timestamp, a code
//! directory without the `runtime` flag, and a designated requirement that is not the
//! program's [`Program::requirement`]. A program name that is not in the table is refused
//! before `codesign` runs.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::mac_programs::{AUTHORITIES, Program, TEAM_ID, program};

/// Why `release check-code-signature` refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `--program` names no program of the package.
    #[error("{0:?} is not a program of the js-agent package")]
    UnknownProgram(String),
    /// `codesign` could not be run, or failed.
    #[error("{command}: {why}")]
    Tool {
        /// The command line.
        command: String,
        /// Why it failed.
        why: String,
    },
    /// The signature is not the one the release gives the program.
    #[error("{binary} is not signed as {program}: {}", .problems.join("; "))]
    Refused {
        /// The binary that was read.
        binary: String,
        /// The program it must be.
        program: String,
        /// What is wrong, one line each.
        problems: Vec<String>,
    },
}

/// The reads of `codesign` the check needs.
pub trait Codesign {
    /// `codesign --verify --strict --verbose=2`: `Err` with the reason when the signature does
    /// not verify.
    ///
    /// # Errors
    /// [`Error::Tool`].
    fn verify(&self, binary: &Path) -> Result<(), Error>;

    /// `codesign --display --verbose=2`: what it prints (codesign prints it on stderr).
    ///
    /// # Errors
    /// [`Error::Tool`].
    fn display(&self, binary: &Path) -> Result<String, Error>;

    /// `codesign --display --requirements -`: what it prints.
    ///
    /// # Errors
    /// [`Error::Tool`].
    fn requirements(&self, binary: &Path) -> Result<String, Error>;
}

/// `codesign` the program, at a path (`/usr/bin/codesign` on a Mac).
pub struct SystemCodesign {
    /// The program to run.
    pub program: PathBuf,
}

impl SystemCodesign {
    /// Runs the program with `args` and the binary: its standard error and standard output
    /// together, when it started and succeeded.
    fn run(&self, args: &[&str], binary: &Path) -> Result<String, Error> {
        let command = format!(
            "{} {} {}",
            self.program.display(),
            args.join(" "),
            binary.display()
        );
        let out = Command::new(&self.program)
            .args(args)
            .arg(binary)
            .output()
            .map_err(|e| Error::Tool {
                command: command.clone(),
                why: e.to_string(),
            })?;
        let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stdout));
        if !out.status.success() {
            return Err(Error::Tool {
                command,
                why: format!("{}: {}", out.status, text.trim_end()),
            });
        }
        Ok(text)
    }
}

impl Codesign for SystemCodesign {
    fn verify(&self, binary: &Path) -> Result<(), Error> {
        self.run(&["--verify", "--strict", "--verbose=2"], binary)
            .map(|_| ())
    }

    fn display(&self, binary: &Path) -> Result<String, Error> {
        self.run(&["--display", "--verbose=2"], binary)
    }

    fn requirements(&self, binary: &Path) -> Result<String, Error> {
        self.run(&["--display", "--requirements", "-"], binary)
    }
}

/// The value of the first line of `text` that starts with `key=`.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// The flag names of the code directory line (`flags=0x10000(runtime)`).
fn flag_names(display: &str) -> Vec<&str> {
    display
        .lines()
        .filter(|l| l.starts_with("CodeDirectory "))
        .find_map(|l| l.split_once(" flags=").map(|(_, rest)| rest))
        .and_then(|rest| rest.split_once('('))
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or_else(Vec::new, |(names, _)| {
            names.split(',').map(str::trim).collect()
        })
}

/// What is wrong with a signature, one line each; empty when it is `program`'s.
#[must_use]
pub fn problems(program: &Program, display: &str, requirements: &str) -> Vec<String> {
    let mut found = Vec::new();
    if field(display, "Signature") == Some("adhoc") {
        found.push("the signature is ad hoc".to_owned());
    }
    match field(display, "Identifier") {
        Some(id) if id == program.identifier => {}
        other => found.push(format!(
            "the identifier is {}, not {}",
            other.unwrap_or("absent"),
            program.identifier
        )),
    }
    match field(display, "TeamIdentifier") {
        Some(team) if team == TEAM_ID => {}
        other => found.push(format!(
            "the team is {}, not {TEAM_ID}",
            other.unwrap_or("absent")
        )),
    }
    let authorities: Vec<&str> = display
        .lines()
        .filter_map(|l| l.strip_prefix("Authority="))
        .collect();
    if authorities != AUTHORITIES {
        found.push(format!(
            "the authorities are {authorities:?}, not {AUTHORITIES:?}"
        ));
    }
    if field(display, "Timestamp").is_none() {
        found.push("the signature has no secure timestamp".to_owned());
    }
    if !flag_names(display).contains(&"runtime") {
        found.push("the code directory has no hardened runtime flag".to_owned());
    }
    let want = program.requirement();
    let designated: Vec<&str> = requirements
        .lines()
        .filter_map(|l| l.strip_prefix("designated => "))
        .collect();
    if designated != [want.as_str()] {
        found.push(format!(
            "the designated requirement is {designated:?}, not {want:?}"
        ));
    }
    found
}

/// Reads `binary`'s signature through `tool` and holds it to the table.
///
/// The program name is judged before anything runs.
///
/// # Errors
/// [`Error::UnknownProgram`], [`Error::Tool`] when a read fails (including a signature that
/// does not verify), [`Error::Refused`] when the signature is not the program's.
pub fn check(tool: &dyn Codesign, binary: &Path, name: &str) -> Result<String, Error> {
    let program = program(name).ok_or_else(|| Error::UnknownProgram(name.to_owned()))?;
    tool.verify(binary)?;
    let display = tool.display(binary)?;
    let requirements = tool.requirements(binary)?;
    let found = problems(&program, &display, &requirements);
    if !found.is_empty() {
        return Err(Error::Refused {
            binary: binary.display().to_string(),
            program: name.to_owned(),
            problems: found,
        });
    }
    Ok(format!(
        "{}: signed as {} by team {TEAM_ID}, hardened runtime, timestamped",
        binary.display(),
        program.identifier
    ))
}

/// The arguments of `release check-code-signature`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// The binary to read.
    pub binary: PathBuf,
    /// The program it must be: `js-agent`, `js-mac-session`, `js-mac-provision` or `js-vmm`.
    #[arg(long)]
    pub program: String,
    /// The `codesign` program.
    #[arg(long, default_value = "/usr/bin/codesign")]
    pub codesign: PathBuf,
}

/// Runs `release check-code-signature`: the line to print.
///
/// # Errors
/// As [`check`].
pub fn run(args: &CheckArgs) -> Result<String, Error> {
    check(
        &SystemCodesign {
            program: args.codesign.clone(),
        },
        &args.binary,
        &args.program,
    )
}
