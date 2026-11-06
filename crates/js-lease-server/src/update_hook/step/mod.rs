//! The steps of the hook: `prepare`, `cutover`, `verify`, `rollback` and `cleanup`.
//!
//! A step is a function of an [`Env`] and the params table, and ends as a [`Done`]: the status
//! and the last line. It takes the state directory's lock first, loads the record and holds it
//! to the rules, and only then looks at the machine. Every change to the record or through a
//! port passes a kill point first ([`Ports::point`](super::Ports::point)), so a test can end
//! the step at any of them and run it again.
//!
//! The waits look every [`POLL_SECS`] until a bound counted on the [`Clock`](super::Clock) the
//! ports give, so a test steps time and nothing sleeps.

mod cleanup;
mod cutover;
mod prepare;
mod rollback;
mod verify;

use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_update_hook::{Class, DirStat, Outcome, POLL_SECS, START_WAIT_SECS, Step, is_plain};

use super::declaration::{Declaration, LOCK_WAIT_SECS};
use super::ports::{Ports, StoreUnreachable, Writer};
use super::record::{Held, Record, StateError, validate};
use super::{Done, Params, Reason, Report};

pub use cleanup::cleanup;
pub use cutover::cutover;
pub use prepare::prepare;
pub use rollback::rollback;
pub use verify::verify;

/// How a directory of the state directory's path is looked at (see
/// [`fd_stat`](js_update_hook::fd_stat)).
pub type StatFn = dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>;

/// Everything a step touches.
pub struct Env<'a> {
    /// The unit manager, the release link, the store, `/healthz`, time and the kill points.
    pub ports: Ports<'a>,
    /// The hook's declaration.
    pub declaration: &'a Declaration,
    /// How a directory of the state directory's path is looked at.
    pub stat: &'a StatFn,
}

/// Runs `step`.
#[must_use]
pub fn run(step: Step, env: &Env<'_>, params: &Params) -> Done {
    let body: fn(&Env<'_>, &Params) -> Done = match step {
        Step::Prepare => prepare,
        Step::Cutover => cutover,
        Step::Verify => verify,
        Step::Rollback => rollback,
        Step::Cleanup => cleanup,
    };
    body(env, params)
}

/// A step that ended with nothing to tell.
fn success() -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report::default(),
    }
}

/// A success that says why nothing was done.
fn success_with(detail: &str) -> Done {
    let mut done = success();
    done.report.detail = Some(detail.to_owned());
    done
}

/// A failure of the step's own: exit 1, a `release` matter.
fn failure(reason: Reason, detail: impl Into<String>) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report::failure(reason, detail),
    }
}

/// A failure of something shared (the store, the host): exit 1, not the release's doing.
fn environment(reason: Reason, detail: impl Into<String>) -> Done {
    let mut done = failure(reason, detail);
    done.report.class = Class::Environment;
    done
}

/// No version can repair this: exit 76.
fn halt(reason: Reason, detail: impl Into<String>) -> Done {
    Done {
        outcome: Outcome::Halt,
        report: Report::failure(reason, detail),
    }
}

/// The store did not answer: exit 1, class environment.
fn store_down(why: StoreUnreachable) -> Done {
    environment(Reason::StoreUnreachable, why.to_string())
}

/// The state directory, the lock or the record is not as the hook requires.
fn state_failure(error: StateError) -> Done {
    failure(error.reason(), error.to_string())
}

/// Every ending of a step past its commit says so.
fn irreversible(mut done: Done) -> Done {
    done.report.reversible = false;
    done
}

/// Takes the lock, loads the record and holds it to the rules; `None` if there is none.
fn open_state(env: &Env<'_>, params: &Params) -> Result<(Held, Option<Record>), Done> {
    let held = Held::open(&env.declaration.state_dir, env.stat).map_err(state_failure)?;
    let record = held.load().map_err(state_failure)?;
    if let Some(record) = &record {
        validate(record, env.declaration, params)
            .map_err(StateError::Invalid)
            .map_err(state_failure)?;
    }
    Ok((held, record))
}

/// [`open_state`] for a step that needs a record.
fn open_record(env: &Env<'_>, params: &Params) -> Result<(Held, Record), Done> {
    match open_state(env, params)? {
        (held, Some(record)) => Ok((held, record)),
        (_, None) => Err(failure(
            Reason::StateInvalid,
            "there is no record: prepare has not run",
        )),
    }
}

/// Makes `record` the record, after a kill point.
fn save(env: &Env<'_>, held: &Held, record: &Record) -> Result<(), Done> {
    env.ports.point();
    held.save(record).map_err(state_failure)
}

/// Sets a mark in the record, durably, before the act it names.
fn mark(env: &Env<'_>, held: &Held, record: &mut Record, set: fn(&mut Record)) -> Result<(), Done> {
    let mut next = record.clone();
    set(&mut next);
    save(env, held, &next)?;
    *record = next;
    Ok(())
}

/// The release link `--release-link` names, held to its rules: plain, directly inside a
/// declared directory other than the state directory (never one itself: the swap writes the
/// link's parent), and in neither release.
fn release_link(env: &Env<'_>, params: &Params) -> Result<PathBuf, Done> {
    let declaration = env.declaration;
    let link = params
        .release_link
        .clone()
        .ok_or_else(|| failure(Reason::BadParams, "--release-link is required"))?;
    let declared = link.parent().is_some_and(|dir| {
        dir != declaration.state_dir && declaration.write_paths.iter().any(|path| path == dir)
    });
    let in_release = link.starts_with(&params.staged_release)
        || params
            .previous_release
            .as_ref()
            .is_some_and(|previous| link.starts_with(previous));
    match is_plain(&link) && declared && !in_release {
        true => Ok(link),
        false => Err(failure(
            Reason::BadParams,
            format!(
                "the release link {} is not directly inside a declared directory other than the state directory",
                link.display()
            ),
        )),
    }
}

/// What the unit manager says of a unit, in the ways that matter here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    /// `active`.
    Up,
    /// `inactive`, or `failed`: no process.
    Down,
    /// Waiting to be started again after a crash (`auto-restart`): it ran and died.
    Crashed,
    /// `activating`, `reloading`, `deactivating` and the like: on its way somewhere.
    Moving,
}

/// The unit's [`Life`]; a failure to read it ends the step with `reason`, class environment.
fn life(env: &Env<'_>, unit: &str, reason: Reason) -> Result<Life, Done> {
    let state =
        env.ports.units.show(unit).map_err(|why| {
            environment(reason, format!("cannot read the state of {unit}: {why}"))
        })?;
    Ok(if state.sub == "auto-restart" {
        Life::Crashed
    } else {
        match state.active.as_str() {
            "active" => Life::Up,
            "inactive" | "failed" => Life::Down,
            _ => Life::Moving,
        }
    })
}

/// A bound on a wait, counted on the ports' clock from when it is made.
struct Bound {
    began_ms: u64,
    bound_ms: u64,
}

impl Bound {
    fn secs(env: &Env<'_>, secs: u64) -> Bound {
        Bound {
            began_ms: env.ports.clock.now_ms(),
            bound_ms: secs.saturating_mul(1000),
        }
    }

    /// Whether the bound has passed: the wait looks no more.
    fn passed(&self, env: &Env<'_>) -> bool {
        env.ports.clock.now_ms().saturating_sub(self.began_ms) >= self.bound_ms
    }
}

/// The pause between two looks.
fn pause(env: &Env<'_>) {
    env.ports.clock.pause(Duration::from_secs(POLL_SECS));
}

/// Stops `unit` unless it is down, and waits up to `secs` for it to be: `inactive` or
/// `failed`.
fn stop(env: &Env<'_>, unit: &str, secs: u64) -> Result<(), Done> {
    if life(env, unit, Reason::StopFailed)? == Life::Down {
        return Ok(());
    }
    env.ports.point();
    env.ports
        .units
        .stop_no_block(unit)
        .map_err(|why| failure(Reason::StopFailed, format!("cannot stop {unit}: {why}")))?;
    let bound = Bound::secs(env, secs);
    loop {
        if life(env, unit, Reason::StopFailed)? == Life::Down {
            return Ok(());
        }
        if bound.passed(env) {
            return Err(failure(
                Reason::StopFailed,
                format!("{unit} did not stop in {secs} s"),
            ));
        }
        pause(env);
    }
}

/// Waits up to [`LOCK_WAIT_SECS`] for nobody to hold the store's writer lock: the stopped
/// server's session lets it go when its connection closes. A holder at the bound is
/// `lock_not_released`, a store that did not answer the last probe `store_unreachable`, both
/// class environment.
fn wait_free(env: &Env<'_>) -> Result<(), Done> {
    let bound = Bound::secs(env, LOCK_WAIT_SECS);
    loop {
        let last = match env.ports.store.writer() {
            Ok(Writer::Free) => return Ok(()),
            Ok(Writer::Held { holder }) => environment(
                Reason::LockNotReleased,
                format!(
                    "the unit is down and {holder} still holds the store's writer lock after {LOCK_WAIT_SECS} s"
                ),
            ),
            Err(why) => store_down(why),
        };
        if bound.passed(env) {
            return Err(last);
        }
        pause(env);
    }
}

/// Starts `unit` unless it is up, and waits up to [`START_WAIT_SECS`] for it to be `active`.
/// A unit that is down or crashed after the start ran and died: `start_failed`, as is one still
/// on its way at the bound.
fn start(env: &Env<'_>, unit: &str) -> Result<(), Done> {
    if life(env, unit, Reason::StartFailed)? == Life::Up {
        return Ok(());
    }
    env.ports.point();
    env.ports
        .units
        .start(unit)
        .map_err(|why| failure(Reason::StartFailed, format!("cannot start {unit}: {why}")))?;
    let bound = Bound::secs(env, START_WAIT_SECS);
    loop {
        match life(env, unit, Reason::StartFailed)? {
            Life::Up => return Ok(()),
            Life::Down | Life::Crashed => {
                return Err(failure(
                    Reason::StartFailed,
                    format!("{unit} ran and stopped after its start"),
                ));
            }
            Life::Moving => {}
        }
        if bound.passed(env) {
            return Err(failure(
                Reason::StartFailed,
                format!("{unit} was not active {START_WAIT_SECS} s after its start"),
            ));
        }
        pause(env);
    }
}

/// Points the release link at `target`, unless it points there already.
fn relink(env: &Env<'_>, link: &Path, target: &Path) -> Result<(), Done> {
    let now = env.ports.link.target(link).map_err(|why| {
        failure(
            Reason::LinkFailed,
            format!("cannot read {}: {why}", link.display()),
        )
    })?;
    if now.as_deref() == Some(target) {
        return Ok(());
    }
    env.ports.point();
    env.ports.link.relink(link, target).map_err(|why| {
        failure(
            Reason::LinkFailed,
            format!(
                "cannot point {} at {}: {why}",
                link.display(),
                target.display()
            ),
        )
    })
}
