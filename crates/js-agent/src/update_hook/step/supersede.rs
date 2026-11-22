//! A record of another target (host-updates design, 4.2, "The hook record"): an update that
//! was ordered and then dropped for a newer one, or one that failed and was rolled back only in
//! part, leaves its record, and the next order names a version the record does not.
//!
//! Such a record is one this hook wrote when it keeps every rule of [`validate`] once the
//! params are the ones it was written under (its own target, previous version, digest and
//! releases: [`ordered`]); a record of the same target that disagrees in anything else, and a
//! record that breaks a rule of its own, are refused (`state_invalid`) by every step.
//!
//! `prepare` moves such a record aside, never deletes it, and goes on, in two cases:
//!
//! - *superseded* (`record.superseded-<target>-<ms>.json`): it never reached its commit (see
//!   [`begun`]: an update's stop, a first install's move of the link), or it is a first
//!   install whose unit is down (`inactive` or `failed`, which is also what the unit manager
//!   says of a unit that is not installed). Its `rollback` would make no act on the agent;
//! - *rolled back* (`record.rolled-back-<target>-<ms>.json`): an update past its stop whose
//!   rollback is complete in effect, though the record was never deleted (whatever its `rb`
//!   marks say): it returns the host to the previous release this order names, the release link
//!   is on that release, the unit is active, the record does not say the agent that runs is the
//!   one the update started, an in-place hand-back has no request left (`in_place`), and the
//!   lease server shows the previous agent's live session (as `rollback` judges its hello). Its
//!   `rollback` would only wait for that hello and delete the record.
//!
//! The step's last line names the file. A cordon the record marked as its own is left to lapse
//! within one lease: the new update finds it in force and, not knowing it as its own, neither
//! renews nor lifts it.
//!
//! Every other record of another target still guards the host (`state_invalid`), and the detail
//! says why it is kept: a first install whose agent's unit is up, starting or stopping, or
//! cannot be read; an update past its stop whose rollback is not complete, naming the first
//! thing left and saying that the `rollback` step finishes it; and an update that returns the
//! host to another previous release than this order's, whose rollback only an operator can run.
//!
//! `rollback` rolls back the update on record (see [`for_rollback`]): given a record of another
//! target that this hook wrote, of an update (not a first install) that returns the host to the
//! previous version and release the params name, it runs with the params that record was
//! written under, and its last line says so. So the updater's rollback of an order that a
//! `prepare` refused finishes the earlier update, and one that never took the host is deleted as
//! its own rollback would. A first install's record of another target is refused, kept: its
//! rollback stops the agent it started, which no other order may do.

use std::path::Path;

use crate::update_hook::{Done, Held, Params, Record, StateError, validate};

use super::cutover::{Life, begun, life, linked};
use super::rollback::{Look, hello};
use super::{Env, in_place, state_failure};

/// How a record of another target that `prepare` moves aside is done with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Aside {
    /// It never took the host.
    Superseded,
    /// It took the host, and its rollback is complete in effect.
    RolledBack,
}

/// Holds a loaded record to the rules: `Ok(None)` for a record of this update, `Ok(Some(_))` for
/// a record of another target that is moved aside (later, by [`set_aside`]), and the step's
/// `state_invalid` failure for every other record.
pub(super) fn judge(
    env: &Env<'_>,
    params: &Params,
    record: &Record,
) -> Result<Option<Aside>, Done> {
    let Err(invalid) = validate(record, params) else {
        return Ok(None);
    };
    let refuse = |why: Option<String>| refusal(invalid.clone(), why);
    if !written_here(record, params) {
        return Err(refuse(None));
    }
    if !begun(record) {
        return Ok(Some(Aside::Superseded));
    }
    let Some(previous) = record.update.previous_target.as_deref() else {
        return match first_install_up(env, record) {
            None => Ok(Some(Aside::Superseded)),
            Some(why) => Err(refuse(Some(why))),
        };
    };
    match left(env, params, record, previous) {
        None => Ok(Some(Aside::RolledBack)),
        Some(why) => Err(refuse(Some(why))),
    }
}

/// The `state_invalid` failure of `invalid`, and why the record is kept when there is more to
/// say.
fn refusal(invalid: crate::update_hook::Invalid, why: Option<String>) -> Done {
    let done = state_failure(StateError::Invalid(invalid));
    match why {
        Some(why) => noted(done, None, Some(format!("the record is kept: {why}"))),
        None => done,
    }
}

/// A record of another target that keeps every rule under the params it was written under.
fn written_here(record: &Record, params: &Params) -> bool {
    record.update.target != params.target_version
        && validate(record, &ordered(record, params)).is_ok()
}

/// Why a first install of another target that moved the link still holds the host: its unit is
/// up, starting or stopping, or cannot be read; `None` when it is down.
fn first_install_up(env: &Env<'_>, record: &Record) -> Option<String> {
    let target = &record.update.target;
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

/// What is left of the rollback of `record`, an update of another target past its stop that
/// returns the host to `previous`, and how it is finished; `None` when it is complete in
/// effect.
fn left(env: &Env<'_>, params: &Params, record: &Record, previous: &Path) -> Option<String> {
    let target = &record.update.target;
    if !returns_to_order(record, params) {
        return Some(format!(
            "the update to {target} returns the host to {} at {}, not to the previous release the params table names: run its rollback with the params it was written under",
            record.update.previous.as_deref().unwrap_or_default(),
            previous.display()
        ));
    }
    let what = unfinished(env, params, record, previous)?;
    Some(format!(
        "the update to {target} has stopped the previous agent and its rollback is not complete: {what}; the rollback step finishes it"
    ))
}

/// Whether the record returns the host to the previous version and release the params name.
fn returns_to_order(record: &Record, params: &Params) -> bool {
    record.update.previous == params.previous_version
        && record.update.previous_target == params.previous_release
}

/// The first thing the host shows that the rollback of `record` has not done; `None` when the
/// previous agent runs on `previous` and the lease server shows its session.
fn unfinished(env: &Env<'_>, params: &Params, record: &Record, previous: &Path) -> Option<String> {
    let unit = &record.update.unit;
    let link = &params.release_link;
    match linked(env, link, previous) {
        Ok(true) => {}
        Ok(false) => {
            return Some(format!(
                "the release link {} is not on the previous release {}",
                link.display(),
                previous.display()
            ));
        }
        Err(done) => return done.report.detail,
    }
    let state = match env.ports.units.show(unit) {
        Ok(state) => state,
        Err(why) => return Some(format!("the state of {unit} cannot be read: {why}")),
    };
    if !matches!(life(&state), Life::Up) {
        return Some(format!(
            "{unit} is {} ({}): no agent runs",
            state.active, state.sub
        ));
    }
    if record.start_issued && !record.rb.start_issued {
        return Some(format!(
            "{unit} runs the agent the update to {} started",
            record.update.target
        ));
    }
    if record.in_place {
        match in_place::pending(env) {
            Ok(false) => {}
            Ok(true) => {
                return Some(
                    "the request to hand the runs over in place is still in the agent's state root"
                        .to_owned(),
                );
            }
            Err(done) => return done.report.detail,
        }
    }
    let version = record.update.previous.as_deref().unwrap_or_default();
    match env.ports.lease.host() {
        Err(why) => Some(format!("the lease server cannot be read: {why}")),
        Ok(host) => match hello(&host, record, version, params.settings.hello_runtime()) {
            Ok(()) => None,
            Err(Look::Wrong(detail)) => Some(detail),
            Err(_) => Some(format!(
                "the lease server does not show the live session of the previous agent ({version}) the rollback waits for"
            )),
        },
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

/// The params `rollback` rolls `record` back with, and what its last line adds: `params` for a
/// record of this update; for a record of an update to another target this hook wrote that
/// returns the host to the previous version and release the params name, the params it was written under and a note that
/// says so. Every other record is refused, `state_invalid`.
pub(super) fn for_rollback(
    params: &Params,
    record: &Record,
) -> Result<(Params, Option<String>), Done> {
    let Err(invalid) = validate(record, params) else {
        return Ok((params.clone(), None));
    };
    if !written_here(record, params) {
        return Err(refusal(invalid, None));
    }
    let target = &record.update.target;
    let Some(previous) = record.update.previous.as_deref() else {
        return Err(refusal(
            invalid,
            Some(format!(
                "the first install of {target} is rolled back only with the params it was written under"
            )),
        ));
    };
    if !returns_to_order(record, params) {
        return Err(refusal(
            invalid,
            Some(format!(
                "the update to {target} returns the host to {previous}, not to the previous release the params table names: run its rollback with the params it was written under"
            )),
        ));
    }
    let note = format!(
        "the record is of the update to {target}, not {}: rolled back with its own target and releases",
        params.target_version
    );
    Ok((ordered(record, params), Some(note)))
}

/// Moves `record` aside as `how` says, after a kill point, and says what was done.
pub(super) fn set_aside(
    env: &Env<'_>,
    held: &Held,
    record: &Record,
    how: Aside,
) -> Result<String, Done> {
    let target = &record.update.target;
    let (kind, what) = match how {
        Aside::Superseded => (
            "superseded",
            format!("superseded the record of {target}, which never took the host"),
        ),
        Aside::RolledBack => (
            "rolled-back",
            format!(
                "finished the record of {target}, whose rollback is complete in effect (the previous agent runs on the previous release)"
            ),
        ),
    };
    let name = format!("record.{kind}-{target}-{}.json", env.ports.clock.now_ms());
    env.ports.point();
    let path = held.set_aside(&name).map_err(state_failure)?;
    Ok(format!("{what}: moved to {}", path.display()))
}

/// `done` with `before` put ahead of its detail and `after` behind it, each joined by `; `.
pub(super) fn noted(mut done: Done, before: Option<String>, after: Option<String>) -> Done {
    let parts: Vec<String> = [before, done.report.detail.take(), after]
        .into_iter()
        .flatten()
        .collect();
    done.report.detail = (!parts.is_empty()).then(|| parts.join("; "));
    done
}
