//! The two records the supervisor writes in its record directory, `ready.json` once the job
//! has started and `exit.json` once it has ended (or the start failed), and how the agent reads
//! them and the supervisor's exit status.
//!
//! Each record is one JSON object written with one `write(2)` on a file the supervisor opened
//! before it dropped to the slot user ([`Records`]). The agent reads an empty or unparseable
//! file as "not written yet" ([`parse_ready`], [`parse_exit`]), so it never takes half a record
//! for an end.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};

use super::status;

/// The file the agent writes the spec to, in the record directory.
pub const SPEC: &str = "spec.json";

/// The ready record's file, in the record directory.
pub const READY: &str = "ready.json";

/// The exit record's file, in the record directory.
pub const EXIT: &str = "exit.json";

/// A step of the supervisor's start, as an exit record names the one that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Reading the spec (only the agent's reading of a supervisor that exited
    /// [`status::USAGE`] with no record names it: a refused spec writes nothing).
    Spec,
    /// Opening or writing the records.
    Records,
    /// Opening a new audit session.
    AuditSession,
    /// Dropping to the slot user.
    Drop,
    /// Setting the file-size and process limits.
    Limits,
    /// Applying the Seatbelt profile.
    Profile,
    /// Starting the job's program, or waiting for it.
    Spawn,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Spec => "spec",
            Self::Records => "records",
            Self::AuditSession => "audit-session",
            Self::Drop => "drop",
            Self::Limits => "limits",
            Self::Profile => "profile",
            Self::Spawn => "spawn",
        })
    }
}

/// The ready record: the job has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    /// The job's pid, which is also its process group's id.
    pub job_pid: i32,
    /// The audit session the job runs in (the one the supervisor opened), or `None` when the
    /// spec asked for no new session.
    pub asid: Option<i32>,
}

/// How a supervised start ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "how", content = "value")]
pub enum Ended {
    /// The job exited with this status.
    Exited(i32),
    /// A signal ended the job (or, read from the supervisor's status when it wrote no record,
    /// the supervisor).
    Signalled(i32),
    /// A step of the start failed, and the job was never started (or, when the ready record
    /// could not be written, was killed).
    SetupFailed {
        /// The step.
        step: Step,
        /// Why, in words.
        why: String,
    },
}

/// What the job and the processes it waited for used, as the supervisor's `wait4` of the job
/// reported it: the job's own CPU and that of every descendant whose end was waited for (by
/// the job or by a descendant of it), and the largest resident set any one of them reached. A
/// process the job left to run on after it, or that ended unwaited for, is not in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRusage {
    /// The CPU time, user and system together, in microseconds.
    pub cpu_usec: u64,
    /// The largest resident set one of the processes reached, in bytes.
    pub max_rss_bytes: u64,
}

/// The exit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    /// How the start ended.
    pub ended: Ended,
    /// The job's pid, when it was started.
    pub job_pid: Option<i32>,
    /// The job's audit session, when it was started in a new one.
    pub asid: Option<i32>,
    /// What the job used, when it was started and waited for. Left out of the record when
    /// `None`; a record that lacks it (a setup that failed, a supervisor from before it) reads
    /// as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<JobRusage>,
}

/// The ready record in `text`, or `None` when it is empty or not (yet) a whole record.
#[must_use]
pub fn parse_ready(text: &str) -> Option<Ready> {
    serde_json::from_str(text).ok()
}

/// The exit record in `text`, or `None` when it is empty or not (yet) a whole record.
#[must_use]
pub fn parse_exit(text: &str) -> Option<ExitRecord> {
    serde_json::from_str(text).ok()
}

/// How the job's exit status reads: its code, or the signal that ended it.
#[must_use]
pub fn ended_of(status: ExitStatus) -> Ended {
    match status.code() {
        Some(code) => Ended::Exited(code),
        None => Ended::Signalled(status.signal().unwrap_or(0)),
    }
}

/// The supervisor's exit status for `record`: the job's code, 128 and the signal for a job a
/// signal ended, [`status::SETUP_FAILED`] for a failed start. A job may exit 125 itself: the
/// record, not the status, tells the two apart.
#[must_use]
pub fn status_of(record: &ExitRecord) -> u8 {
    match &record.ended {
        Ended::Exited(code) => u8::try_from(code & 0xff).unwrap_or(u8::MAX),
        Ended::Signalled(signal) => u8::try_from(128 + signal).unwrap_or(u8::MAX),
        Ended::SetupFailed { .. } => status::SETUP_FAILED,
    }
}

/// How a supervisor that wrote no exit record ended, by its own exit `status`, for the agent:
/// [`status::SETUP_FAILED`] means its records could not be opened, [`status::USAGE`] that its
/// spec was refused, a signal that something killed it (the job can: after the drop both run
/// as the slot user); any other code is the job's, whose record could not be written.
#[must_use]
pub fn from_status(status: ExitStatus) -> Ended {
    let failed = |step, code| Ended::SetupFailed {
        step,
        why: format!("the supervisor exited {code} and wrote no record"),
    };
    match status.code() {
        Some(code) if code == i32::from(status::SETUP_FAILED) => failed(Step::Records, code),
        Some(code) if code == i32::from(status::USAGE) => failed(Step::Spec, code),
        Some(code) => Ended::Exited(code),
        None => Ended::Signalled(status.signal().unwrap_or(0)),
    }
}

/// The two record files, opened as root before the supervisor drops to the slot user, and
/// written through these descriptors, never by path, so the profile and the user the
/// supervisor then holds need not allow a write to the record directory.
///
/// Rust opens every file with `O_CLOEXEC`, so the job, started after, gets neither.
#[derive(Debug)]
pub struct Records {
    /// `ready.json`.
    ready: File,
    /// `exit.json`.
    exit: File,
}

impl Records {
    /// Makes `ready.json` and `exit.json` in `dir`: new files (an existing file, or a link,
    /// is refused), mode 0600, empty.
    ///
    /// # Errors
    /// A file could not be made.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let make = |name: &str| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join(name))
        };
        Ok(Self {
            ready: make(READY)?,
            exit: make(EXIT)?,
        })
    }

    /// Writes `ready` to `ready.json`, in one write.
    ///
    /// # Errors
    /// The write failed or wrote less than the whole record.
    pub fn write_ready(&mut self, ready: &Ready) -> io::Result<()> {
        write_once(&mut self.ready, &serde_json::to_vec(ready)?)
    }

    /// Writes `record` to `exit.json`, in one write.
    ///
    /// # Errors
    /// The write failed or wrote less than the whole record.
    pub fn write_exit(&mut self, record: &ExitRecord) -> io::Result<()> {
        write_once(&mut self.exit, &serde_json::to_vec(record)?)
    }
}

/// Writes `bytes` to `file` with one `write(2)`.
fn write_once(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    whole(file.write(bytes)?, bytes.len())
}

/// Whether a write of `len` bytes that wrote `written` wrote them all.
fn whole(written: usize, len: usize) -> io::Result<()> {
    if written == len {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::WriteZero,
        format!("wrote {written} of the record's {len} bytes"),
    ))
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
