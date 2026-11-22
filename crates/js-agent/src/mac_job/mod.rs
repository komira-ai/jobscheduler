//! `js-agent __mac-job`: one native macOS job, run by this program alone, start to end.
//!
//! ```text
//! js-agent __mac-job (--job <file> | --job-inline <json>) --user <name> --root <dir> --out <dir>
//!     [--profile confined-writes|deny-default] [--audit-session new|inherit]
//!     [--profile-add <file>]
//! ```
//!
//! A prototype of the one-program Mac runtime (`docs/design/mac-one-program.md`): no helper
//! daemon, no pool of accounts, no `sandbox-exec`. Run as root, it:
//!
//! 1. reads the job ([`spec::JobFile`]): the operating system it is for, the program, its
//!    arguments and environment, a folder of inputs, the outputs to keep, the network it may use
//!    and its wall limit. A job for another operating system or architecture is refused.
//! 2. checks the one ordinary user it runs jobs as: not root, not an administrator, and running
//!    no process (this prototype runs one job per user at a time).
//! 3. makes the job folder `<root>/<id>` ([`layout::Layout`]) owned by that user, copies the
//!    inputs into its `work` folder, and points `HOME`, `TMPDIR` and the caches inside it.
//! 4. starts the program through [`js_mac_session::disclaim::Program`]. In the child, before the
//!    program executes, it opens a new audit session, drops to the user, and applies the
//!    Seatbelt profile ([`profile::render`], of the kind `--profile` names) itself with
//!    `sandbox_init_with_parameters`.
//! 5. waits for the program or its wall limit, then ends every process the job started
//!    ([`sweep`]): every process of the job's audit session, and every process of the user,
//!    which may be killed because no other job runs as that user.
//! 6. copies the declared outputs to `--out`, removes the job folder, and checks that nothing is
//!    left: no process of the user, no file of the user's outside the folder that is new or
//!    changed, no job folder.
//!
//! It writes a JSON report ([`report::Report`]) to `<out>/report.json` and to standard output,
//! and the program's standard output and error to `<out>/stdout` and `<out>/stderr`.
//!
//! `--job-inline` takes the job's JSON as the argument itself, for a run that can pass arguments
//! to the program but no file; the job then fetches its own inputs. `--root` is made (root's,
//! mode 0755) when it is missing.
//!
//! Its statuses ([`status`]): 0 the job exited 0 and nothing is left; 1 the job failed and
//! nothing is left; 2 the command line or the job file is wrong; 3 something was left; 4 the job
//! could not be set up or started.
//!
//! Only macOS runs it; elsewhere it parses its command line and refuses.

pub mod layout;
pub mod profile;
pub mod report;
pub mod spec;
pub mod sweep;

#[cfg(target_os = "macos")]
mod macos;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

/// The mode word `main` answers before anything else.
pub const VERB: &str = "__mac-job";

/// The statuses of the mode.
pub mod status {
    /// The job exited 0 and nothing is left.
    pub const OK: u8 = 0;
    /// The job failed (a status other than 0, a signal, its wall limit) and nothing is left.
    pub const JOB_FAILED: u8 = 1;
    /// The command line or the job file is wrong.
    pub const USAGE: u8 = 2;
    /// The job ended and something it made is still there.
    pub const LEFT_BEHIND: u8 = 3;
    /// The job could not be set up or started.
    pub const SETUP: u8 = 4;
}

/// Whether the job gets an audit session of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuditSession {
    /// A new one, which the sweep finds the job's processes by.
    New,
    /// The caller's; the sweep then finds them by the user alone.
    Inherit,
}

/// The command line after the mode word.
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "js-agent __mac-job", no_binary_name = true)]
pub struct Args {
    /// The job file (JSON, [`spec::JobFile`]).
    #[arg(
        long,
        required_unless_present = "job_inline",
        conflicts_with = "job_inline"
    )]
    pub job: Option<PathBuf>,
    /// The job's JSON itself, in place of a file.
    #[arg(long)]
    pub job_inline: Option<String>,
    /// The ordinary user the job runs as.
    #[arg(long)]
    pub user: String,
    /// The folder the job folder is made in: absolute, owned by root.
    #[arg(long)]
    pub root: PathBuf,
    /// The folder the outputs, the program's streams and the report go to.
    #[arg(long)]
    pub out: PathBuf,
    /// The kind of Seatbelt profile.
    #[arg(long, value_enum, default_value = "confined-writes")]
    pub profile: profile::Kind,
    /// Whether the job gets an audit session of its own.
    #[arg(long, value_enum, default_value = "new")]
    pub audit_session: AuditSession,
    /// Seatbelt rules appended to the profile, for measuring what a build needs.
    #[arg(long)]
    pub profile_add: Option<PathBuf>,
}

/// Parses the command line after the mode word.
pub fn parse(args: &[OsString]) -> Result<Args, clap::Error> {
    Args::try_parse_from(args)
}

/// Runs the mode: `args` is the whole command line, the mode word second.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let args = match parse(args.get(2..).unwrap_or_default()) {
        Ok(args) => args,
        Err(why) => {
            let _ = write!(err, "{why}");
            return ExitCode::from(status::USAGE);
        }
    };
    ExitCode::from(run_parsed(&args, out, err))
}

/// Runs the parsed mode on this platform.
#[cfg(target_os = "macos")]
fn run_parsed(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    macos::run(args, out, err)
}

/// Refuses: only macOS runs native macOS jobs.
#[cfg(not(target_os = "macos"))]
fn run_parsed(_args: &Args, _out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let _ = writeln!(err, "js-agent: {VERB} runs on macOS only");
    status::USAGE
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
