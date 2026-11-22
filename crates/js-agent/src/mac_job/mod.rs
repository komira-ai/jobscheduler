//! `js-agent __mac-job`: one native macOS job, run by this program alone, start to end.
//!
//! ```text
//! js-agent __mac-job (--job <file> | --job-inline <json>)
//!     (--accounts <dir> --account-range <first>-<last> | --user <name>)
//!     --root <dir> --out <dir> [--ready <n>] [--spawn ordinary|disclaimed]
//!     [--profile confined-writes|deny-default] [--audit-session new|inherit]
//!     [--profile-add <file>]
//!     [--mount-provider <socket> --mount-root <dir> [--mount-provider-user <user>] [--agent-id <id>]]
//!     [--put-ingest <dir> [--put-user <user>]]
//! ```
//!
//! A prototype of the one-program Mac runtime (`docs/design/mac-one-program.md`): no helper
//! daemon, no pool of accounts, no `sandbox-exec`. Run as root, it:
//!
//! 1. reads the job ([`spec::JobFile`]): the operating system it is for, the program, its
//!    arguments and environment, a folder of inputs, the outputs to keep, the network it may use
//!    and its wall limit. A job for another operating system or architecture is refused.
//! 2. takes the job's user: with `--accounts`, an account lent by the account pool
//!    ([`crate::mac_accounts::Pool`], its ledger in that folder, its uids those of
//!    `--account-range`, a setting with no default), which has never run a job;
//!    with `--user`, one existing account, for measurement. Either way it checks the user is not
//!    root, not an administrator, and runs no process.
//! 3. makes the job folder `<root>/<id>` ([`layout::Layout`]) owned by that user, copies the
//!    inputs into its `work` folder, and points `HOME`, `TMPDIR` and the caches inside it.
//! 4. starts the job through [`js_mac_session::disclaim::Program`]: this program again, in the
//!    start mode ([`start`]), a new process of one thread, which opens a new audit session, drops
//!    to the user, applies the Seatbelt profile ([`profile::render`], of the kind `--profile`
//!    names) itself with `sandbox_init_with_parameters`, and executes the job's program in its
//!    place. Nothing runs between `fork` and `exec` in the agent, so the agent may have threads.
//! 5. waits for the program or its wall limit, then ends every process the job started
//!    ([`sweep`]): every process of the job's audit session, and every process of the user,
//!    which may be killed because no other job runs as that user.
//! 6. copies the declared outputs to `--out`, removes the job folder, and checks that nothing is
//!    left: no process of the user, no file of the user's outside the folder that is new or
//!    changed, no job folder. A lent account is then retired: the pool ends its uid, deletes it,
//!    shows it gone, and refills.
//!
//! `--spawn ordinary` (the default) starts the job the usual way, so it answers to the privacy
//! controls as the agent and holds the agent's Full Disk Access: the one way measured to let a
//! job read tierstore's network file system mount. `--spawn disclaimed` disclaims it, as the
//! agent's other starts do. The owner decided the default: a job is an ordinary child of the
//! agent, and the profile keeps it on the mount to its own trees (see [`profile::mount_rules`]).
//!
//! It writes a JSON report ([`report::Report`]) to `<out>/report.json` and to standard output,
//! and the program's standard output and error to `<out>/stdout` and `<out>/stderr`.
//!
//! A job's `trees` come from the mount provider at `--mount-provider`: each is prepared read-only
//! and mounted (run id the job's id, source index its place in the list), the path it is mounted
//! at must lie beneath `--mount-root`, and `work/<at>` is a link to it. The confined-writes
//! profile lets the job read it and write nothing there; the deny-default one has no rule for it,
//! so a job with trees is refused under it. Each tree is released after the sweep.
//!
//! A job's `put_dirs` are tierstore's put directories, the way the build service sends a Mac job
//! its output directory (a read-write mount `<put root>/cas/b/<run>/<nonce>`): each must lie
//! beneath `--put-ingest` (the put ingest's root). The agent makes it, the key's folders given to
//! `--put-user` (the node's user) and the put directory itself the job's with the node's group, as
//! the node needs them; links `work/<at>` to it; and the profile lets the job write there. The job
//! commits its objects itself (`pack.part`, renamed to `pack.sha256-<hex>.part`, renamed to
//! `pack`), and the node stores and removes them. The report lists what is left in each.
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
pub mod start;
pub mod sweep;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use crate::mac_accounts::Slots;

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

/// How the job's program is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Spawn {
    /// The usual way: it holds the agent's privacy grants (the owner's decided default).
    Ordinary,
    /// With its privacy responsibility disclaimed.
    Disclaimed,
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
    /// The account pool's state folder: the job runs as an account it lends.
    #[arg(
        long,
        required_unless_present = "user",
        conflicts_with = "user",
        requires = "account_range"
    )]
    pub accounts: Option<PathBuf>,
    /// The uids the lent accounts take, `<first>-<last>` both included: required with
    /// `--accounts`, with no default, so a Mac's earlier accounts are never touched by chance.
    #[arg(long, value_name = "FIRST-LAST", conflicts_with = "user")]
    pub account_range: Option<Slots>,
    /// One existing ordinary user the job runs as, in place of a lent account.
    #[arg(long)]
    pub user: Option<String>,
    /// How many ready accounts the pool keeps.
    #[arg(long, default_value_t = 1)]
    pub ready: usize,
    /// How the job is started.
    #[arg(long, value_enum, default_value = "ordinary")]
    pub spawn: Spawn,
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
    /// The mount provider's socket, for the job's trees.
    #[arg(long, requires = "mount_root")]
    pub mount_provider: Option<PathBuf>,
    /// The user the mount provider runs as (a name or a uid), whose socket it is.
    #[arg(long, default_value = "0")]
    pub mount_provider_user: String,
    /// The folder every tree the provider mounts must lie beneath.
    #[arg(long, requires = "mount_provider")]
    pub mount_root: Option<PathBuf>,
    /// The id the provider calls carry as the agent's.
    #[arg(long, default_value = "mac-job")]
    pub agent_id: String,
    /// The put ingest's root, beneath which the job's put directories lie.
    #[arg(long)]
    pub put_ingest: Option<PathBuf>,
    /// The user the put ingest's node runs as: a name, or `uid:gid`.
    #[arg(long, default_value = "_tierstore")]
    pub put_user: String,
}

/// Parses the command line after the mode word.
pub fn parse(args: &[OsString]) -> Result<Args, clap::Error> {
    Args::try_parse_from(args)
}

/// Runs the mode, or the start of its job ([`start`]), when `args` (the whole command line)
/// names it second: its status, or `None` for any other command line.
pub fn dispatch(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> Option<ExitCode> {
    match args.get(1).and_then(|word| word.to_str()) {
        Some(VERB) => Some(run(args, out, err)),
        Some(start::VERB) => Some(run_start(args, err)),
        _ => None,
    }
}

/// Runs the start of a job: `js-agent __mac-job-start --start <file>`. On success it does not
/// return: the job's program runs in this process.
pub fn run_start(args: &[OsString], err: &mut dyn Write) -> ExitCode {
    let Some([flag, file]) = args.get(2..) else {
        let _ = writeln!(err, "usage: js-agent {} --start <file>", start::VERB);
        return ExitCode::from(status::USAGE);
    };
    if flag != "--start" {
        let _ = writeln!(err, "usage: js-agent {} --start <file>", start::VERB);
        return ExitCode::from(status::USAGE);
    }
    ExitCode::from(start_parsed(std::path::Path::new(file), err))
}

/// Starts the job of the start file `file`, on this platform.
#[cfg(target_os = "macos")]
fn start_parsed(file: &std::path::Path, err: &mut dyn Write) -> u8 {
    macos::start_job(file, err)
}

/// Refuses: only macOS starts native macOS jobs.
#[cfg(not(target_os = "macos"))]
fn start_parsed(_file: &std::path::Path, err: &mut dyn Write) -> u8 {
    let _ = writeln!(err, "js-agent: {} runs on macOS only", start::VERB);
    status::USAGE
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
