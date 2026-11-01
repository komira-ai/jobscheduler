//! The steps of the hook that have a body: `prepare`, `cutover`, `verify` and `cleanup`
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
    Class, Declaration, DirStat, Done, Held, LeaseError, Outcome, Params, Ports, Reason, Record,
    Report, StateError, validate,
};

pub use cleanup::cleanup;
pub use cutover::cutover;
pub use prepare::prepare;
pub use rollback::rollback;
pub use verify::verify;

/// Everything a step touches.
pub struct Env<'a> {
    /// The unit manager, the release link, the lease server, the agent's state root, time and
    /// the kill points.
    pub ports: Ports<'a>,
    /// Runs the staged agent's `--version`.
    pub runner: &'a dyn CommandRunner,
    /// The hook's declaration.
    pub declaration: &'a Declaration,
    /// How a directory of the state directory's path is looked at (see
    /// [`fd_stat`](super::fd_stat)).
    pub stat: &'a dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
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
    let held = Held::open(&env.declaration.state_dir, env.stat).map_err(state_failure)?;
    let record = held.load().map_err(state_failure)?;
    if let Some(record) = &record {
        check(env, params, record)?;
    }
    Ok((held, record))
}

/// Holds `record` to the rules of the declaration and the params table.
fn check(env: &Env<'_>, params: &Params, record: &Record) -> Result<(), Done> {
    validate(record, env.declaration, params)
        .map_err(StateError::from)
        .map_err(state_failure)
}

/// Makes `record` the record, after a kill point.
fn save(env: &Env<'_>, held: &Held, record: &Record) -> Result<(), Done> {
    env.ports.point();
    held.save(record).map_err(state_failure)
}
