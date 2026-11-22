//! `js-agent update-hook`: the js-agent's side of the host updater's hook contract, as
//! hostkeeper runs it.
//!
//! ```text
//! js-agent update-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! js-agent <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! js-agent update-hook <drain|resume> --params <file>
//! js-agent <drain|resume> --params <file>
//! ```
//!
//! hostkeeper runs the update steps in the second form and the steps it adds in the first, so
//! the hook takes both, for every step ([`is_hook_word`] routes the program to it). `uninstall`
//! is refused: the package declares no uninstall bound.
//!
//! An update of a crun host under systemd whose running agent can hand its runs over does not
//! drain it: `prepare` records the handover and neither cordons nor waits, and `cutover` and
//! `rollback` write the handover file in the agent's state root before they stop the unit, so the
//! stopping agent leaves its runs to the one started next (see `prepare` for when, and
//! [`crate::lifecycle::handover`]); on a host with an egress ceiling, only when both agents keep
//! the runs' egress wiring. Every other host is cordoned and drained first, and the detail of a
//! successful `prepare` says why.
//!
//! `drain` and `resume` are the steps of a maintenance run (hostkeeper's maintenance-updates
//! design, 5.2): the package manifest names them (`hook.drain`, `hook.resume`), and hostkeeper
//! runs `drain` before work on the host that is not an update (an OS update, a reboot) and
//! `resume` after it. `drain` writes the hold file of the agent's state root, cordons the host
//! and returns once it is idle; `resume` restarts the agent if the host facts changed, ends the
//! hold and lifts the cordon (see [`step::drain`] and [`step::resume`]). Their params file is
//! the manifest's table with the updater's maintenance keys ([`DrainParams`]), and they keep no
//! record.
//!
//! The updater reads two things: the exit status and the last line of standard output, a JSON
//! object ([`Report`]). Exit 0 is success, 1 failure, 75 deferred (nothing irreversible
//! happened, try later) and 76 halt (no version can repair this). hostkeeper's daemon logs only
//! the reason of that line, and the hook's standard error, which it keeps as the hook's log; so
//! a step that does not succeed also writes one line there, its step, how it ended, its reason
//! and its detail (`js-agent update-hook: prepare failed: state_invalid: ...`), a success with a
//! detail writes its step and that detail (`js-agent update-hook: prepare succeeded: ...`), and
//! a command line or params file that is refused writes why. The verb is answered before
//! the agent's flags are parsed and before any signal handler is installed: a hook is a
//! short-lived root process of the updater, not an agent.
//!
//! The steps have their bodies in [`step`], over the ports. [`Host::system`] runs them over
//! the machine's: the unit manager, the release link (read, never written: hostkeeper points it
//! at the staged release before `cutover` and back at the previous one before `rollback`), the
//! lease server's own-host routes and the agent's state root (see [`machine`](Machine)). What the
//! hook is told comes from the package manifest's `[params]` table ([`KEYS`]); where the lease
//! server is comes from the host's flags file (`flags_file`), the file `js-agent --flags-file`
//! reads, so the server and pool of a host stay flags.
//!
//! What every service's hook shares (the exit status, the last line's shape, the step names,
//! the locked state directory, the unit managers, the time and the kill points) is
//! js-update-hook's; this module re-exports it under the names the agent's steps use, with the
//! agent's [`Reason`]s and [`Record`].
//!
//! The hook of a config package (`js-agent config-hook`, the module [`config`]) shares this
//! contract and its dispatch ([`is_hook_word`], [`run`]) and has steps of its own.
//!
//! The hook builds on Linux and macOS and differs between them in two places only: the unit
//! manager ([`PlatformUnits`]: `systemctl` on Linux, `launchctl` on a Mac) and the runtime the
//! staged agent must list on `--version` ([`Settings::runtime`]).

mod args;
pub mod config;
mod drain_params;
mod launch_units;
mod machine;
pub mod params;
mod ports;
mod record;
mod report;
mod settings;
pub mod step;
mod unit_files;
mod validate;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub use js_update_hook::Outcome;

pub use args::{
    ArgsError, Invocation, MaintainInvocation, MaintenanceStep, Request, Step, VERB, is_hook_word,
};
pub use drain_params::{DRAIN_DEADLINE_KEY, DrainParams};
pub use launch_units::{BOOTOUT_MARGIN, LAUNCHCTL, LaunchUnits, SETTLE_READS, StopRunner};
pub use machine::{
    Machine, MachineLease, StatFn, SystemFacts, host_facts, own_host_id, run_maintain, run_step,
};
pub use params::{
    ARTIFACT_DIGEST_KEY, CLEANUP_TIMEOUT_KEY, CORDON_LEASE_KEY, CORDON_MAX_KEY,
    CUTOVER_TIMEOUT_KEY, ConnectParams, DEFER_RETRY_KEY, DRAIN_WAIT_KEY, FLAGS_FILE_KEY,
    HELPER_UNIT_KEY, KEYS, MIN_CORDON_LEASE_SECS, PF_UNIT_KEY, POLL_SECS, PREPARE_TIMEOUT_KEY,
    PREVIOUS_RELEASE_KEY, PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, Params, ParamsError,
    RELEASE_LINK_KEY, ROLLBACK_TIMEOUT_KEY, RUNTIME_KEY, SCRATCH_ROOT_KEY, SOCKET_GROUP_KEY,
    STAGED_RELEASE_KEY, STATE_DIR_KEY, STATE_ROOT_KEY, TARGET_VERSION_KEY, TOKEN_FILE_KEY,
    UNIT_KEY, VERIFY_DEADLINE_KEY, VERIFY_TIMEOUT_KEY,
};
pub use ports::{
    Clock, CordonView, Cordoned, Facts, Gate, HoldState, Holds, HostReading, Lease, LeaseError,
    Link, MAX_CORDON_SECS, NoGate, PortError, Ports, SYSTEMCTL, StateView, SystemClock, SystemLink,
    SystemUnits, Uncordoned, UnitState, Units, read_link,
};
pub use record::{
    Cordon, DirProblem, DirStat, Held, LOCK_FILE, RECORD_FILE, RECORD_MAX, Record, Rollback,
    SCHEMA, StateError, TEMP_FILE, UnitRestart, UpdateInfo, check_ancestor, check_dir, fd_stat,
};
pub use report::{Class, MAX_REASON_LEN, Reason, Report, is_reason_token};
pub use settings::{Settings, Timeouts};
pub use unit_files::{INSTALLED_MISSING, Manager, PlanError, agent_in, plan};
pub use validate::{
    CUTOVER_MARGIN_SECS, EXPIRY_SLACK_MS, Invalid, MAX_CHANGE_LEN, MAX_CHANGES, MAX_VERSION_LEN,
    PREPARE_MARGIN_SECS, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    VERIFY_MARGIN_SECS, validate,
};

/// The unit port of the platform the hook is built for.
#[cfg(target_os = "macos")]
pub type PlatformUnits<'a> = LaunchUnits<'a>;

/// The unit port of the platform the hook is built for.
#[cfg(not(target_os = "macos"))]
pub type PlatformUnits<'a> = SystemUnits<'a>;

// The updater's exit codes of a step ([`Outcome`]) are NOT `readiness.rs`'s
// `EXIT_NOISE_OVER_BOUND` (75) and `EXIT_NOT_QUIET` (76), which are the statuses of a consumer's
// readiness hook run by the agent; there 75 and 76 are the updater's "deferred" and "halt". The
// two sets share numbers and nothing else: do not import one for the other.

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
    /// Runs the maintenance step `invocation` names, with its params already read.
    pub maintain: &'a dyn Fn(&MaintainInvocation, &DrainParams) -> Done,
}

/// This machine's step: the real ports.
fn machine_step(invocation: &Invocation, params: &Params) -> Done {
    run_step(invocation, params, &Machine::system())
}

/// This machine's maintenance step: the real ports.
fn machine_maintain(invocation: &MaintainInvocation, params: &DrainParams) -> Done {
    run_maintain(invocation, params, &Machine::system())
}

impl Host<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            step: &machine_step,
            maintain: &machine_maintain,
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

/// Reads the command line and the params file and runs the step they name: the step's word,
/// and how it ended.
fn step_of(args: &[OsString], host: &Host<'_>) -> Result<(&'static str, Done), Refusal> {
    let invocation = match Request::parse(args.iter().skip(1).cloned()).map_err(Refusal::Args)? {
        Request::Update(invocation) => invocation,
        Request::Maintain(invocation) => {
            let params = DrainParams::read(&invocation.params).map_err(Refusal::Params)?;
            return Ok((
                invocation.step.name(),
                (host.maintain)(&invocation, &params),
            ));
        }
    };
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
    Ok((invocation.step.name(), (host.step)(&invocation, &params)))
}

/// Writes to `err` how `step` ended and why: the reason and the detail of its last line, in the
/// same words. A success says nothing unless its last line has a detail (a record set aside, a
/// unit that changes), and then says the detail.
fn tell(err: &mut dyn Write, step: &str, done: &Done) {
    let ending = match done.outcome {
        Outcome::Success => {
            if let Some(detail) = &done.report.detail {
                let _ = writeln!(err, "js-agent update-hook: {step} succeeded: {detail}");
            }
            return;
        }
        Outcome::Failure => "failed",
        Outcome::Deferred => "deferred",
        Outcome::Halt => "halted",
    };
    let reason = done.report.reason.map_or("", Reason::as_str);
    let detail = done.report.detail.as_deref().unwrap_or("");
    let _ = writeln!(
        err,
        "js-agent update-hook: {step} {ending}: {reason}: {detail}"
    );
}

/// Runs the hook on `args` (the program name first, then the verb or the step): the last line goes to
/// `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    // The config package's hook shares the contract and the dispatch, and has its own steps.
    if args.get(1).is_some_and(|verb| verb == config::VERB) {
        return config::run(args, out, err, &config::Host::system());
    }
    let done = match step_of(args, host) {
        Ok((step, done)) => {
            tell(err, step, &done);
            done
        }
        Err(refusal) => {
            let _ = writeln!(err, "js-agent update-hook: {refusal}");
            refusal.done()
        }
    };
    // A closed standard output cannot be reported anywhere; the status still is.
    let _ = done.report.write_to(out);
    done.outcome.into()
}
