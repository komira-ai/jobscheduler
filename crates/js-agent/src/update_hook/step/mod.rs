//! The steps of the hook: `prepare`, `cutover`, `verify`, `rollback` and `cleanup`
//! (host-updates design, 4.2).
//!
//! A step is a function of an [`Env`] and the params table, and ends as a [`Done`]: the status
//! and the last line. It takes the state directory's lock first, loads and validates the
//! record, and only then looks at the machine. Every change to the record or through a port
//! passes a kill point first ([`Ports::point`](super::Ports::point)), so that a test, and later
//! the conformance harness, can end the step at any of them and run it again.

mod cleanup;
mod cutover;
mod prepare;
mod rollback;
mod verify;

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use crate::facts::CommandRunner;

use super::{
    Class, DirStat, Done, Held, LeaseError, Manager, Outcome, Params, Ports, Reason, Record,
    Report, StateError, Step, validate,
};

pub use cleanup::cleanup;
pub use cutover::cutover;
pub use prepare::prepare;
pub use rollback::rollback;
pub use verify::verify;

/// What the hook finds when it looks at the join token file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenFacts {
    /// Whether it is a regular file after following links.
    pub regular: bool,
    /// Its permission bits.
    pub mode: u32,
    /// Its owner.
    pub uid: u32,
}

/// How the token file is looked at (see [`TokenFacts`]).
pub type TokenFn = dyn Fn(&Path) -> io::Result<TokenFacts>;

/// Everything a step touches.
pub struct Env<'a> {
    /// The unit manager, the release link, the lease server, the agent's state root, time and
    /// the kill points.
    pub ports: Ports<'a>,
    /// Runs the staged agent's `--version`, and `systemctl` where the unit manager is systemd.
    pub runner: &'a dyn CommandRunner,
    /// Which unit manager runs the agent, and so which unit file a release carries.
    pub manager: Manager,
    /// The user the hook runs as: the owner the join token file must have.
    pub euid: u32,
    /// How the join token file is looked at.
    pub token: &'a TokenFn,
    /// How a directory of the state directory's path is looked at (see
    /// [`fd_stat`](super::fd_stat)).
    pub stat: &'a dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
}

/// Whether `step` on a host whose record is `record` is past its commit, so that every ending
/// of it says `reversible: false`: `cutover` and `rollback` once the first irreversible act is
/// begun (the stop, or a first install's move of the link; see `cutover`).
pub(crate) fn committed(step: Step, record: &Record) -> bool {
    matches!(step, Step::Cutover | Step::Rollback) && cutover::begun(record)
}

/// A step that ended without a body of its own to say more: success, nothing to tell.
fn success() -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report::default(),
    }
}

/// A failure of the step's own: exit 1, a `release` matter.
fn failure(reason: Reason, detail: impl Into<String>) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(reason, detail),
    }
}

/// The state directory, the lock or the record is not as the hook requires.
fn state_failure(error: StateError) -> Done {
    failure(error.reason(), error.to_string())
}

/// The lease server did not answer as asked: not the release's fault.
fn lease_failure(error: LeaseError) -> Done {
    let mut done = failure(Reason::LeaseUnreachable, error.to_string());
    done.report.class = Class::Environment;
    done
}

/// Takes the lock, loads the record and holds it to the rules; `None` if there is none.
fn open_state(env: &Env<'_>, params: &Params) -> Result<(Held, Option<Record>), Done> {
    let held = Held::open(&params.settings.state_dir, env.stat).map_err(state_failure)?;
    let record = held.load().map_err(state_failure)?;
    if let Some(record) = &record {
        check(params, record)?;
    }
    Ok((held, record))
}

/// Holds `record` to the rules of the params table.
fn check(params: &Params, record: &Record) -> Result<(), Done> {
    validate(record, params)
        .map_err(StateError::Invalid)
        .map_err(state_failure)
}

/// Makes `record` the record, after a kill point.
fn save(env: &Env<'_>, held: &Held, record: &Record) -> Result<(), Done> {
    env.ports.point();
    held.save(record).map_err(state_failure)
}
