//! `rollback`: put the previous release back. It decides from the record and from what the
//! machine shows (the machine is believed over the marks), and a rerun after a kill at any
//! point goes on from there and makes no act twice.
//!
//! The first row that matches:
//! 1. no record: nothing to undo (it has finished before, or `prepare` never wrote one);
//! 2. the server was never stopped (no `stop_begun`): nothing to repair; the record is deleted
//!    and the step ends `reversible: true`;
//! 3. a first install (no previous release): if the link was never moved, nothing ran through
//!    it and the record is deleted; if it was, there is nothing to restore, so the step touches
//!    nothing, keeps the record and fails `no_previous_release`;
//! 4. otherwise the unit and the link are read:
//!    - `active` on the previous release: no act;
//!    - not down (`active`, on its way somewhere, or crashed and waiting to restart) on the
//!      staged release, or not `active` on the previous one: stop it, wait for the writer lock to
//!      be free, point the link at the previous release if it is not there, start it. A unit
//!      that is still stopping on the previous release is not taken as serving: the lock its
//!      dying process holds would read as the previous release's;
//!    - not down on any other release, or with no link: halt (76, `state_invalid`), nothing
//!      done;
//!    - down: wait for the writer lock to be free (a stopped server's session can outlive it
//!      for a moment), point the link at the previous release if it is not there, and start it.
//!
//! Then the step waits up to [`SERVE_WAIT_SECS`](crate::update_hook::SERVE_WAIT_SECS) for the
//! previous release to hold the store's writer lock as a holder other than the one the step
//! probed before its first act (recorded with the mark of that act, `rb_holder`, and compared
//! by equality); a step that made no act takes any holder. At the bound it fails
//! `rollback_not_serving`, or `store_unreachable` (class environment) when the store did not
//! answer the last probe; a unit that is down or crashed fails `rollback_not_serving` at once.
//! Then the record is deleted (no `cleanup` follows a rollback).
//!
//! Exit codes: 0; 1 (every ending after the stop says `reversible: false`); 76 for a release it
//! cannot name. Never 75.

use std::path::Path;

use js_update_hook::{ROLLBACK_MARGIN_SECS, START_WAIT_SECS};

use crate::update_hook::ports::Writer;
use crate::update_hook::{Done, LOCK_WAIT_SECS, Params, Reason, Record, SERVE_WAIT_SECS};

use super::{
    Bound, Env, Held, Life, failure, halt, irreversible, life, mark, open_state, pause,
    release_link, relink, start, state_failure, stop, store_down, success, success_with, wait_free,
};

/// Runs `rollback`.
pub fn rollback(env: &Env<'_>, params: &Params) -> Done {
    let (held, loaded) = match open_state(env, params) {
        Ok(opened) => opened,
        Err(done) => return done,
    };
    let Some(mut record) = loaded else {
        return success_with(
            "there is no record: the rollback has finished, or prepare never wrote one",
        );
    };
    let done = go(env, params, &held, &mut record).unwrap_or_else(|done| done);
    match record.stop_begun {
        true => irreversible(done),
        false => done,
    }
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    let first_install_unmoved = record.previous_target.is_none() && !record.link_moved;
    if !record.stop_begun || first_install_unmoved {
        return finish(env, held);
    }
    let Some(previous) = record.previous_target.clone() else {
        return Err(failure(
            Reason::NoPreviousRelease,
            "a first install has no previous release to restore: nothing was changed",
        ));
    };
    if !record.rb.serving_ok {
        let link = release_link(env, params)?;
        restore(env, held, record, &link, &previous)?;
        serving(env, record)?;
        mark(env, held, record, |record| record.rb.serving_ok = true)?;
    }
    finish(env, held)
}

/// Deletes the record: the update is undone.
fn finish(env: &Env<'_>, held: &Held) -> Result<Done, Done> {
    env.ports.point();
    held.delete().map_err(state_failure)?;
    Ok(success())
}

/// Puts the unit on the previous release, from what the unit and the link show.
fn restore(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    link: &Path,
    previous: &Path,
) -> Result<(), Done> {
    let unit = record.unit.clone();
    let state = life(env, &unit, Reason::StopFailed)?;
    let target = env.ports.link.target(link).map_err(|why| {
        failure(
            Reason::LinkFailed,
            format!("cannot read {}: {why}", link.display()),
        )
    })?;
    let at_previous = target.as_deref() == Some(previous);
    if state == Life::Up && at_previous {
        return Ok(());
    }
    if state != Life::Down {
        if !at_previous && target.as_deref() != Some(record.staged.as_path()) {
            return Err(halt(
                Reason::StateInvalid,
                format!(
                    "{unit} is not down, and {} is on neither release of the update",
                    link.display()
                ),
            ));
        }
        act(env, held, record, |record| record.rb.stop_begun = true)?;
        let bound = env.declaration.timeouts.rollback.saturating_sub(
            LOCK_WAIT_SECS + START_WAIT_SECS + SERVE_WAIT_SECS + ROLLBACK_MARGIN_SECS,
        );
        stop(env, &unit, bound)?;
    }
    // Whatever stopped the unit (this step, or a cutover that failed), its process's session may
    // still hold the lock: the previous release would refuse to start beside it.
    wait_free(env)?;
    if !at_previous {
        act(env, held, record, |record| record.rb.link_restored = true)?;
        relink(env, link, previous)?;
    }
    act(env, held, record, |record| record.rb.start_issued = true)?;
    start(env, &unit)
}

/// Sets a mark of rollback's before its act. The first one is saved with `rb_holder`, the
/// holder of the writer lock before any act, probed now.
fn act(env: &Env<'_>, held: &Held, record: &mut Record, set: fn(&mut Record)) -> Result<(), Done> {
    let begun = record.rb.stop_begun || record.rb.link_restored || record.rb.start_issued;
    if !begun {
        record.rb_holder = match env.ports.store.writer().map_err(store_down)? {
            Writer::Free => None,
            Writer::Held { holder } => Some(holder),
        };
    }
    mark(env, held, record, set)
}

/// Waits for the previous release to hold the writer lock as another holder than `rb_holder`.
fn serving(env: &Env<'_>, record: &Record) -> Result<(), Done> {
    let unit = record.unit.as_str();
    let bound = Bound::secs(env, SERVE_WAIT_SECS);
    let not_serving = || {
        failure(
            Reason::RollbackNotServing,
            format!(
                "the previous release did not take the store's writer lock in {SERVE_WAIT_SECS} s"
            ),
        )
    };
    loop {
        let last = match life(env, unit, Reason::RollbackNotServing)? {
            Life::Down | Life::Crashed => {
                return Err(failure(
                    Reason::RollbackNotServing,
                    format!("{unit} is not running on the previous release"),
                ));
            }
            _ => match env.ports.store.writer() {
                Ok(Writer::Held { holder }) if record.rb_holder.as_ref() != Some(&holder) => {
                    return Ok(());
                }
                Ok(_) => not_serving(),
                Err(why) => store_down(why),
            },
        };
        if bound.passed(env) {
            return Err(last);
        }
        pause(env);
    }
}
