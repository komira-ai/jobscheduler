//! `cutover`: stop the server, wait for its writer lock to be let go, move the release link to
//! the staged release, and start the server. The stop is the commit: once it is begun every
//! ending says `reversible: false`. No host is cordoned and no job is waited for: the outage is
//! the time the unit takes to stop and start.
//!
//! In this order:
//! 1. the lock, the record and the params agree; there must be a record;
//! 2. the unit is read, so a read that fails leaves nothing begun; then the record gets
//!    `stop_begun`;
//! 3. unless the link is on the staged release already: the unit is stopped and waited for (at
//!    most the declared bound less the lock wait, the start wait and a margin), then the store's
//!    writer lock must be free within [`LOCK_WAIT_SECS`](crate::update_hook::LOCK_WAIT_SECS): a
//!    holder left after the stop is `lock_not_released` (class environment) with the link
//!    unmoved, since a second server would refuse to start beside it. Then the record gets
//!    `link_moved` and the link is moved;
//! 4. the record gets `start_issued`, and the unit is started and waited for until `active`.
//!
//! Each mark is written before its act. A rerun does not rely on the marks: it asks the unit
//! manager and the link, so a unit that is down is not stopped again, a link on the staged
//! release is not moved again, and a unit that is up is not started again.

use std::path::Path;

use js_update_hook::{CUTOVER_MARGIN_SECS, START_WAIT_SECS};

use crate::update_hook::{Done, LOCK_WAIT_SECS, Params, Reason, Record};

use super::{
    Env, Held, irreversible, life, mark, open_record, release_link, relink, start, stop, success,
    wait_free,
};

/// Runs `cutover`.
pub fn cutover(env: &Env<'_>, params: &Params) -> Done {
    let (held, mut record) = match open_record(env, params) {
        Ok(opened) => opened,
        Err(done) => return done,
    };
    let done = go(env, params, &held, &mut record).unwrap_or_else(|done| done);
    match record.stop_begun {
        true => irreversible(done),
        false => done,
    }
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    let link = release_link(env, params)?;
    let unit = record.unit.clone();
    if !record.stop_begun {
        life(env, &unit, Reason::StopFailed)?;
        mark(env, held, record, |record| record.stop_begun = true)?;
    }
    if !on(env, &link, &record.staged)? {
        let bound = env
            .declaration
            .timeouts
            .cutover
            .saturating_sub(LOCK_WAIT_SECS + START_WAIT_SECS + CUTOVER_MARGIN_SECS);
        stop(env, &unit, bound)?;
        wait_free(env)?;
        if !record.link_moved {
            mark(env, held, record, |record| record.link_moved = true)?;
        }
        relink(env, &link, &record.staged)?;
    }
    if !record.start_issued {
        mark(env, held, record, |record| record.start_issued = true)?;
    }
    start(env, &unit)?;
    Ok(success())
}

/// Whether `link` points at `release`.
pub(super) fn on(env: &Env<'_>, link: &Path, release: &Path) -> Result<bool, Done> {
    let target = env.ports.link.target(link).map_err(|why| {
        super::failure(
            Reason::LinkFailed,
            format!("cannot read {}: {why}", link.display()),
        )
    })?;
    Ok(target.as_deref() == Some(release))
}
