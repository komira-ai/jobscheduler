//! `verify`: look, again and again until a bound, for proof that the new server runs and
//! serves. It reads and never changes the machine; the one write is the record's `verify_ok`,
//! after the observation it names.
//!
//! The proof, at one look:
//! - the unit is `active`;
//! - the store's writer lock is held, by a holder other than the one `prepare` recorded (none
//!   recorded means any holder is another). The holders are opaque identifiers compared by
//!   equality, and no order is read into them. A server that has not taken the lock has not
//!   started: it takes the lock before it reads the store or binds a listener;
//! - `GET /healthz` answers 200.
//!
//! Before it starts: a record with `start_issued` and the release link on the staged release
//! (`state_invalid` otherwise). At each look a unit that is `inactive`, `failed` or waiting to
//! restart after a crash ends the step at once as `start_failed`: the release ran and died.
//!
//! The looks end at the declared `verify` timeout less
//! [`VERIFY_MARGIN_SECS`](js_update_hook::VERIFY_MARGIN_SECS), one every
//! [`POLL_SECS`](js_update_hook::POLL_SECS). Then the failure follows the last look: `no_lock`
//! (no new holder of the lock), `not_serving` (a new holder, and `/healthz` not 200), or
//! `store_unreachable` (class environment: the store did not answer, which says nothing of the
//! release). A rerun with `verify_ok` set succeeds without a look.

use js_update_hook::VERIFY_MARGIN_SECS;

use crate::update_hook::ports::Writer;
use crate::update_hook::{Done, Params, Reason, Record};

use super::cutover::on;
use super::{
    Bound, Env, Life, failure, life, open_record, pause, release_link, save, store_down, success,
};

/// Runs `verify`.
pub fn verify(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, mut record) = open_record(env, params)?;
    if record.verify_ok {
        return Ok(success());
    }
    if !record.start_issued {
        return Err(failure(
            Reason::StateInvalid,
            "the record does not say that cutover started the unit",
        ));
    }
    let link = release_link(env, params)?;
    if !on(env, &link, &record.staged)? {
        return Err(failure(
            Reason::StateInvalid,
            "the release link is not on the staged release",
        ));
    }
    wait(env, &record)?;
    record.verify_ok = true;
    save(env, &held, &record)?;
    Ok(success())
}

/// Looks until the new server is proved, or the bound passes.
fn wait(env: &Env<'_>, record: &Record) -> Result<(), Done> {
    let unit = record.unit.as_str();
    let secs = env
        .declaration
        .timeouts
        .verify
        .saturating_sub(VERIFY_MARGIN_SECS);
    let bound = Bound::secs(env, secs);
    let mut last = no_lock(secs);
    loop {
        match life(env, unit, Reason::StartFailed)? {
            Life::Down | Life::Crashed => {
                return Err(failure(
                    Reason::StartFailed,
                    format!("{unit} is not running during verify"),
                ));
            }
            Life::Up => match look(env, record, secs) {
                Ok(()) => return Ok(()),
                Err(done) => last = done,
            },
            Life::Moving => {}
        }
        if bound.passed(env) {
            return Err(last);
        }
        pause(env);
    }
}

/// One look at the store and `/healthz`: `Ok` is proof, and the error is the failure the step
/// ends with if this is the last look.
fn look(env: &Env<'_>, record: &Record, secs: u64) -> Result<(), Done> {
    let holder = match env.ports.store.writer().map_err(store_down)? {
        Writer::Held { holder } => holder,
        Writer::Free => return Err(no_lock(secs)),
    };
    if record.holder.as_ref() == Some(&holder) {
        return Err(no_lock(secs));
    }
    match env.ports.health.status() {
        Ok(200) => Ok(()),
        Ok(status) => Err(not_serving(&format!("/healthz answered {status}"))),
        Err(why) => Err(not_serving(&why)),
    }
}

fn no_lock(secs: u64) -> Done {
    failure(
        Reason::NoLock,
        format!("no new process took the store's writer lock in {secs} s"),
    )
}

fn not_serving(why: &str) -> Done {
    failure(
        Reason::NotServing,
        format!("a new process holds the store's writer lock and is not serving: {why}"),
    )
}
