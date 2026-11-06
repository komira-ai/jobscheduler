//! `js-lease-server update-hook`: the lease server's side of the host updater's hook contract.
//!
//! ```text
//! js-lease-server update-hook [--name=value ...] <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! ```
//!
//! The updater runs one step per process and reads two things: the exit status and the last
//! line of standard output, a JSON object ([`Report`]). Exit 0 is success, 1 failure, 75
//! deferred (nothing irreversible happened, try later) and 76 halt (no version can repair
//! this). The verb is answered before the server's flags are parsed and before any listener or
//! signal handler: a hook is a short-lived root process of the updater, not a server.
//!
//! **An update restarts the server in place.** There is no cordon of any host, no drain and no
//! standby: `prepare` reads, `cutover` stops the unit, waits for the store's writer lock to be
//! let go, moves the release link and starts the unit, and `verify` waits for a new process to
//! hold the writer lock and answer `/healthz`. The outage is the time the unit takes to stop
//! and start. What a restart keeps is what the server keeps in its store and its files; what
//! it holds only in memory is lost with the process.
//!
//! The steps' bodies are in [`step`], over the ports ([`Ports`]). [`Host::system`] runs them over
//! the machine's ([`Machine`]). What every service's hook shares (the exit status, the last
//! line, the step names, the locked state directory, the unit manager, the release link, time
//! and the kill points) is js-update-hook's.

mod args;
mod declaration;
mod machine;
mod params;
mod ports;
mod record;
mod report;
pub mod step;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub use args::{
    ArgsError, DEFAULT_UNIT, Flags, HEALTHZ_URL_FLAG, Invocation, RELEASE_LINK_FLAG, STORE_FLAG,
    UNIT_FLAG, healthz_address,
};
pub use declaration::{
    CLEANUP_SECS, DECLARATION_PATH, Declaration, DeclarationError, LOCK_WAIT_SECS, MIN_TIMEOUTS,
    PREPARE_WORK_SECS, SERVE_WAIT_SECS, STOP_WAIT_SECS, Timeouts, VERIFY_WAIT_SECS,
};
pub use js_update_hook::{Outcome, Step};
pub use js_update_hook::{PortError, UnitState};
pub use machine::{
    HEALTHZ_LIMIT, HttpHealth, Machine, StoreProbe, SystemRunner, judge, run_step, store_target,
};
pub use params::{
    DEFER_RETRY_KEY, PREVIOUS_RELEASE_KEY, PREVIOUS_VERSION_KEY, Params, ParamsError,
    STAGED_RELEASE_KEY, TARGET_VERSION_KEY,
};
pub use ports::{
    Clock, Gate, Health, Link, Ports, Schema, StoreUnreachable, StoreView, Units, Writer,
};
pub use record::{Held, Invalid, Record, Rollback, SCHEMA, StateError, validate};
pub use report::{Reason, Report};

/// The first argument that selects the verb.
pub const VERB: &str = "update-hook";

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
    pub step: &'a dyn Fn(&Invocation, &Params) -> Done,
}

/// This machine's step: the real ports, over the declaration the updater installed.
fn machine_step(invocation: &Invocation, params: &Params) -> Done {
    run_step(invocation, params, &Machine::system())
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
    Args(ArgsError),
    #[error("{0}")]
    Params(ParamsError),
    #[error(
        "the artifact {given:?} is not the staged release {STAGED_RELEASE_KEY} names ({staged:?})"
    )]
    Artifact { given: PathBuf, staged: PathBuf },
}

impl Refusal {
    /// The failure the updater is told of: nothing was done, so nothing is irreversible.
    fn done(&self) -> Done {
        let reason = match self {
            Refusal::Args(_) | Refusal::Artifact { .. } => Reason::BadArgs,
            Refusal::Params(_) => Reason::BadParams,
        };
        Done {
            outcome: Outcome::Failure,
            report: Report::failure(reason, self.to_string()),
        }
    }
}

/// Reads the command line and the params file and runs the step they name.
fn step_of(args: &[OsString], host: &Host<'_>) -> Result<Done, Refusal> {
    let invocation = Invocation::parse(args.iter().skip(2).cloned()).map_err(Refusal::Args)?;
    let params = Params::read(&invocation.params)
        .map_err(Refusal::Params)?
        .with_flags(&invocation.flags);
    // `prepare` is given the staged tree twice, by argument and by the table; they must agree.
    if let Some(given) = invocation
        .artifact
        .as_ref()
        .filter(|given| **given != params.staged_release)
    {
        return Err(Refusal::Artifact {
            given: given.clone(),
            staged: params.staged_release,
        });
    }
    Ok((host.step)(&invocation, &params))
}

/// Whether `args` (the program name first) select the verb.
#[must_use]
pub fn is_verb(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|verb| verb == VERB)
}

/// Runs the verb on `args` (the program name and the verb first): the last line goes to
/// `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let done = step_of(args, host).unwrap_or_else(|refusal| {
        let _ = writeln!(err, "js-lease-server update-hook: {refusal}");
        refusal.done()
    });
    // A closed standard output cannot be reported anywhere; the status still is.
    let _ = done.report.write_to(out);
    done.outcome.into()
}
