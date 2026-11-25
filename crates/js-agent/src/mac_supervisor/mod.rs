//! `js-agent __supervise-native --spec <file>`: the start of one native Mac job, in a process of
//! its own, and the agent's side that starts it and reads how it went.
//!
//! A native job runs as its own slot user and under a Seatbelt profile, and it answers to the
//! privacy controls (TCC) for the agent, so it holds the agent's Full Disk Access: it is
//! started without the disclaim, and so is every process between the agent and the job; the
//! user and the profile are the fence (`docs/design/spawn-disclaim.md`). Applying a profile
//! allocates, and so does dropping to a user through the system's calls, and between `fork` and
//! `exec` a child of the agent, which has other threads, may not allocate. So the agent starts
//! this verb, a fresh `js-agent` process with one thread for its whole life (the verb is
//! answered before the agent's signal handlers and before any thread, and it starts no runtime
//! and no logger), and the verb does what that child could not. It is the job's parent until
//! the job ends.
//!
//! **The supervisor** ([`run::supervise`]), in this order; the first step that fails ends the
//! start, the job is never started after it, and the exit record says which step and why:
//!
//! 1. `spec`: reads the spec file ([`SuperviseSpec`], [`spec::read_spec`]): opened without
//!    following a link, a regular file of the caller's (root's) with no permission for its
//!    group or others, removed before it is parsed. A spec that is refused, or a command line
//!    that is wrong, exits [`status::USAGE`] and writes nothing;
//! 2. `records`: makes `ready.json` and `exit.json` in the spec's record directory, new files
//!    of mode 0600, still as root, and keeps them open ([`record::Records`]); every record is
//!    written through them, never by path, and the job does not get them (`O_CLOEXEC`). When
//!    they cannot be made, nothing can be recorded: the verb says why on its error stream and
//!    exits [`status::SETUP_FAILED`];
//! 3. `audit-session`: opens a new audit session whose audit user is the slot uid (skipped
//!    when the spec says `new_session: false`, as tests do: only root may);
//! 4. `drop`: drops to the slot uid, with the slot gid its only group, and checks that root
//!    cannot be regained (skipped when the verb does not run as root, as in tests);
//! 5. `limits`: sets the file-size and process limits, each soft and hard;
//! 6. `profile`: applies the Seatbelt profile with its parameters;
//! 7. `spawn`: starts the job's program in a process group of its own, keeping the
//!    responsibility, with the verb's own environment, working directory and standard streams;
//!    then writes the ready record ([`Ready`]: the job's pid, which is its group's id, and the
//!    audit session), waits for the job (`wait4`, which also says what the job and every
//!    descendant whose end was waited for used: [`JobRusage`]), writes the exit record
//!    ([`ExitRecord`], with that usage) and exits with the job's status.
//!
//! **Its exit status** ([`record::status_of`]): the job's code; 128 and the signal for a job a
//! signal ended; [`status::SETUP_FAILED`] (125) when a step failed; [`status::USAGE`] (2) for a
//! spec or command line it refused. A job can exit 125 itself: the record tells the two apart.
//! The records are JSON, one write each; the agent reads an empty or partial file as not
//! written yet.
//!
//! **The agent's side** ([`launch`]): a [`Launcher`] starts the supervisor for a
//! [`LaunchRequest`] (the spec, and the job's whole environment, working directory and three
//! streams, which the supervisor passes on unchanged). [`SystemLauncher`] writes the spec to
//! `<record_dir>/spec.json` (a new file, mode 0600) and starts this program with the verb,
//! keeping the responsibility, in a process group of its own. The [`Supervised`] it returns
//! waits for the ready record ([`Supervised::wait_ready`], which pauses between looks and
//! reads no clock) and answers how the run ended without waiting ([`Supervised::try_exit`]),
//! from the exit record or, with none, from the supervisor's own status
//! ([`record::from_status`]). [`FakeLauncher`] runs shell text with no supervisor and writes
//! the same records, for tests.
//!
//! The native runtime's start of a job (`crate::native`) calls [`SystemLauncher`], for every
//! native job and for its self-test. The decisions here are tested on every platform; the system calls of the
//! audit session, the drop, the limits and the profile are in `macos` and only macOS has them
//! (elsewhere each step fails, as `macOS only`).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub mod fake;
pub mod launch;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod record;
pub mod run;
pub mod spec;

pub use fake::FakeLauncher;
pub use launch::{
    LaunchRequest, Launcher, Supervised, Supervision, SupervisorChild, SystemLauncher,
};
pub use record::{Ended, ExitRecord, JobRusage, Ready, Step};
pub use spec::SuperviseSpec;

/// The verb `main` answers before anything else.
pub const VERB: &str = "__supervise-native";

/// The supervisor's own exit statuses; any other is the job's.
pub mod status {
    /// The spec or the command line was refused; nothing was written.
    pub const USAGE: u8 = 2;
    /// A step of the start failed: the exit record says which (or, when the records could
    /// not be made, only the error stream does).
    pub const SETUP_FAILED: u8 = 125;
}

/// How the command line reads.
const USAGE: &str = "usage: js-agent __supervise-native --spec <file>";

/// Runs the verb: `args` is the whole command line, the verb second. Diagnostics go to `err`;
/// `_out` is the job's, and the verb writes nothing to it.
pub fn run(args: &[OsString], _out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    ExitCode::from(run_over(&run::SystemHost, args, err))
}

/// [`run`] over `host`: the status.
fn run_over(host: &dyn run::SuperviseHost, args: &[OsString], err: &mut dyn Write) -> u8 {
    let read = spec_path(args.get(2..).unwrap_or_default())
        .and_then(|path| spec::read_spec(&path).map_err(|why| why.to_string()));
    match read {
        Ok(spec) => run::supervise(host, &spec, err),
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}\n{USAGE}");
            status::USAGE
        }
    }
}

/// The spec's path, from the command line after the verb: `--spec <file>`, and nothing else.
fn spec_path(args: &[OsString]) -> Result<PathBuf, String> {
    match args {
        [flag, path] if flag == "--spec" => Ok(PathBuf::from(path)),
        [] => Err("--spec is required".to_owned()),
        _ => Err(format!("unexpected arguments {args:?}")),
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
