//! `js-agent update-hook`: the js-agent's side of the host updater's hook contract (host-updates
//! design, 4.2).
//!
//! ```text
//! js-agent update-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! ```
//!
//! The updater runs one step per process and reads two things: the exit status and the last
//! line of standard output, a JSON object ([`Report`]). Exit 0 is success, 1 failure, 75
//! deferred (nothing irreversible happened, try later) and 76 halt (no version can repair
//! this). The verb is answered before the agent's flags are parsed and before any signal
//! handler is installed: a hook is a short-lived root process of the updater, not an agent.
//!
//! The five steps have their bodies in [`step`], over the ports. [`Host::system`] runs them over
//! the machine's: the unit manager, the release link, the lease server's own-host routes and the
//! agent's state root (see [`machine`](Machine)). Where the lease server is and where the state
//! root is come from the params table ([`ConnectParams`]); the declaration is read from
//! [`DECLARATION_PATH`].
//!
//! The hook builds on Linux and macOS and differs between them in two places only: the unit
//! manager ([`PlatformUnits`]: `systemctl` on Linux, `launchctl` on a Mac) and the runtime the
//! staged agent must list on `--version` ([`Declaration::runtime`], which the declaration names
//! or the platform's [`DEFAULT_RUNTIME`] stands for).

mod args;
mod declaration;
mod launch_units;
mod machine;
mod params;
mod plain;
mod ports;
mod record;
mod report;
pub mod step;
mod validate;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub use args::{ArgsError, Invocation, Step};
pub use declaration::{DECLARATION_PATH, DEFAULT_RUNTIME, Declaration, DeclarationError, Timeouts};
pub use launch_units::{BOOTOUT_MARGIN, LAUNCHCTL, LaunchUnits, SETTLE_READS, StopRunner};
pub use machine::{Machine, MachineLease, StatFn, run_step};
pub use params::{
    ARTIFACT_DIGEST_KEY, CA_CERT_KEY, CORDON_LEASE_KEY, CORDON_MAX_KEY, ConnectParams,
    DEFER_RETRY_KEY, DRAIN_WAIT_KEY, HOST_ID_KEY, MIN_CORDON_LEASE_SECS, POLL_SECS,
    PREVIOUS_RELEASE_KEY, PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, Params, ParamsError,
    RELEASE_LINK_KEY, SERVER_KEY, STAGED_RELEASE_KEY, STATE_ROOT_KEY, TARGET_VERSION_KEY,
    TOKEN_FILE_KEY,
};
pub use ports::{
    Clock, CordonView, Cordoned, Gate, HostReading, Lease, LeaseError, Link, MAX_CORDON_SECS,
    NoGate, PortError, Ports, SYSTEMCTL, StateView, SystemClock, SystemLink, SystemUnits,
    Uncordoned, UnitState, Units, read_link, swap_link,
};
pub use record::{
    Cordon, DirProblem, DirStat, Held, LOCK_FILE, RECORD_FILE, RECORD_MAX, Record, Rollback,
    SCHEMA, StateError, TEMP_FILE, UpdateInfo, check_ancestor, check_dir, fd_stat,
};
pub use report::{Class, MAX_REASON_LEN, Reason, Report, is_reason_token};
pub use validate::{
    CUTOVER_MARGIN_SECS, EXPIRY_SLACK_MS, Invalid, MAX_VERSION_LEN, PREPARE_MARGIN_SECS,
    ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, ROLLBACK_STOP_MIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS, validate,
};

/// The unit port of the platform the hook is built for.
#[cfg(target_os = "macos")]
pub type PlatformUnits<'a> = LaunchUnits<'a>;

/// The unit port of the platform the hook is built for.
#[cfg(not(target_os = "macos"))]
pub type PlatformUnits<'a> = SystemUnits<'a>;

/// The first argument that selects the verb.
pub const VERB: &str = "update-hook";

// The updater's exit codes of a step. They are NOT `readiness.rs`'s `EXIT_NOISE_OVER_BOUND`
// (75) and `EXIT_NOT_QUIET` (76), which are the statuses of a consumer's readiness hook run
// by the agent; here 75 and 76 are the updater's "deferred" and "halt". The two sets share
// numbers and nothing else: do not import one for the other.

/// How a step ended, as the updater reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Success,
    /// Exit 1.
    Failure,
    /// Exit 75: the host is busy or not ready, nothing irreversible happened, try later.
    Deferred,
    /// Exit 76: no version can repair this; acting further would make it worse.
    Halt,
}

impl Outcome {
    /// The status a step exits with.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Outcome::Success => 0,
            Outcome::Failure => 1,
            Outcome::Deferred => 75,
            Outcome::Halt => 76,
        }
    }
}

impl From<Outcome> for ExitCode {
    fn from(outcome: Outcome) -> ExitCode {
        ExitCode::from(outcome.code())
    }
}

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
    let params = Params::read(&invocation.params).map_err(Refusal::Params)?;
    // `prepare` is given the staged tree twice, by argument and by the table; they must agree.
    if let Some(given) = invocation
        .artifact
        .as_ref()
        .filter(|given| given.as_os_str() != params.staged_release.as_os_str())
    {
        return Err(Refusal::Artifact {
            given: given.clone(),
            staged: params.staged_release,
        });
    }
    Ok((host.step)(&invocation, &params))
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
        let _ = writeln!(err, "js-agent update-hook: {refusal}");
        refusal.done()
    });
    // A closed standard output cannot be reported anywhere; the status still is.
    let _ = done.report.write_to(out);
    done.outcome.into()
}
