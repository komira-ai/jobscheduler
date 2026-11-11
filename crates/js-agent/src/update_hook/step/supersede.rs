//! `prepare`'s answer to a record of another target (host-updates design, 4.2, "The hook
//! record"): an update that was ordered, prepared and then dropped for a newer one leaves its
//! record, and the next order names a version the record does not. Such a record is
//! *superseded*, not refused, when it never took the host:
//!
//! - it is a record this hook wrote: it keeps every rule of [`validate`] once the params are
//!   the ones it was written under (its own target, previous version, digest and releases);
//! - its target is not the one the params name (a record of the same target that disagrees
//!   in anything else is refused as before);
//! - it never reached its commit (see [`begun`]: an update's stop, a first install's move of
//!   the link), or it is a first install whose unit is down (`inactive` or `failed`, which is
//!   also what the unit manager says of a unit that is not installed). Those are the records
//!   whose `rollback` would make no act on the agent; it would only lift a cordon of the
//!   record's own and delete the record.
//!
//! A superseded record is moved aside in the state directory, never deleted, as
//! `record.superseded-<target>-<ms>.json`, and the step's last line says so. A cordon it marked
//! as its own is left to lapse within one lease: the new update finds it in force and, not
//! knowing it as its own, neither renews nor lifts it.
//!
//! Every other record of another target still guards the host as before (`state_invalid`):
//! an update past its stop is rolled back first, and the detail says so; so is a first install
//! whose agent's unit is up, starting or stopping, or cannot be read.

use crate::update_hook::{Done, Held, Params, Record, StateError, validate};

use super::cutover::{Life, begun, life};
use super::{Env, state_failure};

/// Holds a loaded record to the rules: `Ok(false)` for a record of this update, `Ok(true)` for
/// a record of another target that is superseded (moved aside later, by [`set_aside`]), and
/// the step's `state_invalid` failure for every other record.
pub(super) fn judge(env: &Env<'_>, params: &Params, record: &Record) -> Result<bool, Done> {
    let Err(invalid) = validate(record, params) else {
        return Ok(false);
    };
    let refuse = |why: Option<String>| {
        let mut done = state_failure(StateError::Invalid(invalid.clone()));
        if let (Some(why), Some(detail)) = (why, done.report.detail.as_mut()) {
            detail.push_str(&format!("; the record is kept: {why}"));
        }
        done
    };
    let other_target = record.update.target != params.target_version;
    if !other_target || validate(record, &ordered(record, params)).is_err() {
        return Err(refuse(None));
    }
    match holds(env, record) {
        None => Ok(true),
        Some(why) => Err(refuse(Some(why))),
    }
}

/// Why a record of another target still holds the host; `None` when it never took it.
fn holds(env: &Env<'_>, record: &Record) -> Option<String> {
    if !begun(record) {
        return None;
    }
    let target = &record.update.target;
    if record.update.previous.is_some() {
        return Some(format!(
            "the update to {target} has stopped the previous agent: roll it back first"
        ));
    }
    let unit = &record.update.unit;
    match env.ports.units.show(unit) {
        Ok(state) if matches!(life(&state), Life::Down) => None,
        Ok(state) => Some(format!(
            "the first install of {target} moved the link and {unit} is {}",
            state.active
        )),
        Err(why) => Some(format!(
            "the first install of {target} moved the link and the state of {unit} cannot be read: {why}"
        )),
    }
}

/// The params `record` was written under: these, with its own target, previous version,
/// digest and releases.
fn ordered(record: &Record, params: &Params) -> Params {
    let update = &record.update;
    Params {
        target_version: update.target.clone(),
        previous_version: update.previous.clone(),
        artifact_digest: update.artifact_digest.clone(),
        staged_release: update.staged.clone(),
        previous_release: update.previous_target.clone(),
        ..params.clone()
    }
}

/// Moves the superseded `record` aside, after a kill point, and says what was done.
pub(super) fn set_aside(env: &Env<'_>, held: &Held, record: &Record) -> Result<String, Done> {
    let target = &record.update.target;
    let name = format!(
        "record.superseded-{target}-{}.json",
        env.ports.clock.now_ms()
    );
    env.ports.point();
    let path = held.set_aside(&name).map_err(state_failure)?;
    Ok(format!(
        "superseded the record of {target}, which never took the host: moved to {}",
        path.display()
    ))
}
