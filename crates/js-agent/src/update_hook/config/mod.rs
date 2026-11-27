//! `js-agent config-hook`: the hook of a js-agent config package (`js-agent-config-<group>`),
//! which installs the host's flags file (`/etc/js-agent/flags`) and the certificate it names.
//!
//! ```text
//! js-agent config-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! js-agent config-hook uninstall [--params <file>]
//! ```
//!
//! The package manifest names `hook.command = ["/usr/bin/js-agent", "config-hook"]` (on a Mac,
//! `/Library/Application Support/js-agent/bin/js-agent`), and hostkeeper runs it as the update hook
//! is run: the same exit statuses and last line ([`Report`]), the same flock on a state
//! directory for the whole step, every step safe to run twice, nothing from the environment.
//! The agent reads its flags once, at its start, so the hand-over of new flags is a restart:
//!
//! - `prepare` reads and checks the params, and keeps in its state directory what is at each
//!   path the staged package places and at the flags file (see [`kept`]), before the installer
//!   replaces it;
//! - `cutover` checks that the flags file now in place is one the agent can use (it parses as
//!   the agent's flags and names a host id, a facts file read included), else it ends
//!   `bad_config` with nothing restarted; when every file the package placed is as it was before
//!   the update (this update's kept copy says so), restarts nothing; else notes the session the
//!   lease server shows for the host and restarts the unit;
//! - `verify` waits for the unit to be active and the host to be `live` in another session (in
//!   any session, when `cutover` found the files unchanged), and then removes the kept copy;
//! - `rollback` puts the kept files back, restarts the unit onto them and, with the join token in
//!   place, waits for the hello without failing on it;
//! - `cleanup` deletes the record (and a kept copy `verify` could not remove), once `verify` has
//!   succeeded;
//! - `uninstall` does nothing to the service: the files go with the package, and the agent keeps
//!   running on what it read.
//!
//! The steps are in [`steps`] over the ports of [`Env`]; [`run_step`] runs them on this machine.
//! The verb is answered by [`update_hook::run`](super::run), before the agent's flags are
//! parsed and before any signal handler is installed.

mod args;
mod kept;
mod machine;
mod params;
mod reason;
mod record;
mod steps;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

pub use js_update_hook::Outcome;

pub use args::{Invocation, VERB};
pub use machine::{run_on, run_step};
pub use params::{
    CUTOVER_MIN_SECS, HELLO_WAIT_SECS, KEYS, Params, ParamsError, ROLLBACK_MIN_SECS,
    VERIFY_MIN_SECS,
};
pub use reason::{Reason, Report};
pub use record::{Held, Invalid, Record, SCHEMA, StateError};
pub use steps::{CUTOVER_RETRY_SECS, Env, LeaseOf, cleanup, cutover, prepare, rollback, verify};

/// How a step ended and what it tells the updater.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    /// The exit status.
    pub outcome: Outcome,
    /// The last line.
    pub report: Report,
}

/// What the verb takes from the machine it runs on: the body of the steps.
pub struct Host<'a> {
    /// Runs the step `invocation` names, with the params already read.
    pub step: &'a dyn Fn(&crate::update_hook::Invocation, &Params) -> Done,
}

/// This machine's step: the real ports.
fn machine_step(invocation: &crate::update_hook::Invocation, params: &Params) -> Done {
    run_step(
        invocation.step,
        invocation.artifact.as_deref(),
        params,
        &crate::update_hook::Machine::system(),
    )
}

impl Host<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            step: &machine_step,
        }
    }
}

/// Why no step ran.
#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error("{0}")]
    Args(crate::update_hook::ArgsError),
    #[error("{0}")]
    Params(ParamsError),
}

impl Refusal {
    /// The failure the updater is told of: nothing was done, so nothing is irreversible.
    fn done(&self) -> Done {
        let reason = match self {
            Refusal::Args(_) => Reason::BadArgs,
            Refusal::Params(_) => Reason::BadParams,
        };
        Done {
            outcome: Outcome::Failure,
            report: Report::failure(reason, self.to_string()),
        }
    }
}

/// The step the arguments name (`uninstall` among them) and how it ended.
fn step_of(args: &[OsString], host: &Host<'_>) -> Result<(&'static str, Done), Refusal> {
    match Invocation::parse(args.iter().skip(2).cloned().collect()).map_err(Refusal::Args)? {
        Invocation::Uninstall => Ok((
            "uninstall",
            Done {
                outcome: Outcome::Success,
                report: Report::default(),
            },
        )),
        Invocation::Step(invocation) => {
            let params = Params::read(&invocation.params).map_err(Refusal::Params)?;
            Ok((invocation.step.name(), (host.step)(&invocation, &params)))
        }
    }
}

/// Writes to `err` how `step` ended and why, in the words of the last line. A success says
/// nothing unless its last line has a detail.
fn tell(err: &mut dyn Write, step: &str, done: &Done) {
    if done.outcome == Outcome::Success {
        if let Some(detail) = &done.report.detail {
            let _ = writeln!(err, "js-agent config-hook: {step} succeeded: {detail}");
        }
        return;
    }
    // The config hook halts nothing: a step that did not succeed failed, or was deferred
    // (`cutover` while the lease server cannot be asked).
    let ending = if done.outcome == Outcome::Deferred {
        "deferred"
    } else {
        "failed"
    };
    let reason = done.report.reason.map_or("", Reason::as_str);
    let detail = done.report.detail.as_deref().unwrap_or("");
    let _ = writeln!(
        err,
        "js-agent config-hook: {step} {ending}: {reason}: {detail}"
    );
}

/// Runs the hook on `args` (the program name, the verb, then the step): the last line goes to
/// `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let done = match step_of(args, host) {
        Ok((step, done)) => {
            tell(err, step, &done);
            done
        }
        Err(refusal) => {
            let _ = writeln!(err, "js-agent config-hook: {refusal}");
            refusal.done()
        }
    };
    // A closed standard output cannot be reported anywhere; the status still is.
    let _ = done.report.write_to(out);
    done.outcome.into()
}
