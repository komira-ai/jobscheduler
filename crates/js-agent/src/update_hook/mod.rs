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
//! Every step is a stub today: it exits 1 with the reason `not_implemented`.

mod args;
mod params;
mod report;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

pub use args::{ArgsError, Invocation, Step};
pub use params::{CORDON_LEASE_KEY, CORDON_MAX_KEY, PROMOTION_CEILING_KEY, Params, ParamsError};
pub use report::{Class, MAX_REASON_LEN, Reason, Report, is_reason_token};

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

/// A step with no body yet: exit 1, reason `not_implemented`.
fn not_implemented(invocation: &Invocation, _params: &Params) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(
            Reason::NotImplemented,
            format!("{} is not implemented", invocation.step),
        ),
    }
}

impl Host<'static> {
    /// This machine.
    #[must_use]
    pub fn system() -> Self {
        Self {
            step: &not_implemented,
        }
    }
}

/// Runs the verb on `args` (the program name and the verb first): the last line goes to
/// `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let _ = (args, out, err, host);
    unimplemented!()
}
