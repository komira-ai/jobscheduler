//! `cutover` (host-updates design, 4.2): look once more at the cordon and the leases, then stop
//! the unit, move the release link from the previous release to the staged one, and start the
//! unit. The stop is the commit: once it is begun every ending says `reversible: false`.
//!
//! In this order:
//! 1. the lock, the record and the params agree; there must be a record, and the release link
//!    the declaration names (`release_link`) must lie inside a declared directory, never be one,
//!    and be in neither release (`bad_params` otherwise, nothing done);
//! 2. unless the stop is begun already: a taint or an `owed-*` record halts (76), and the host
//!    must be cordoned and hold no lease. `prepare` can end with no cordon held (it was killed
//!    between the placing and the confirming), so a cordon of ours that is gone is put back
//!    within the cap, and a cap that is spent or a lease that came in defers (75, class
//!    environment) with nothing stopped. A cordon that is not ours (an operator's) is neither
//!    renewed nor lifted. A cordon in force under a mark that was never confirmed may be ours or
//!    not, and nothing would renew it through the stop: that defers too, the mark left as it is;
//! 3. the unit and the link are read, so that a read that fails leaves nothing begun; then the
//!    record gets `stop_begun` together with `pre_stop_session_ms`, the session the lease server
//!    showed for the host in the reading of step 2 (one save, so that the stop is never marked
//!    without it and it is never written without the stop; a rerun that finds the stop begun
//!    reads no host and keeps it), and the unit is stopped and waited for (renewing the cordon if
//!    it is ours), for at most the declared bound less the start wait and a margin;
//! 4. the record gets `link_moved`, then the link is moved;
//! 5. the record gets `start_issued`, then the unit is started and waited for. A start that
//!    fails is the release's fault only if the unit ran and died (`failed`, or `auto-restart`
//!    after a crash, at any look); a unit still waiting in `start` is not.
//!
//! Each mark is written before its act, so a run that is killed leaves the marks of every act
//! it may have made. A rerun does not rely on them: it asks the unit manager and the link, and
//! does what is not yet done, so every act is a no-op when its effect is already there. The
//! state of the unit is read with `show` and not `is-active`, which cannot tell a failed run of
//! `systemctl` from an inactive unit.
//!
//! A first install has no agent to drain, no host the lease server knows and no unit to stop:
//! it skips steps 2 and 3, writes no `stop_begun`, and is irreversible from `link_moved` on.

use std::path::Path;
use std::time::Duration;

use crate::update_hook::plain::is_plain;
use crate::update_hook::{
    CUTOVER_MARGIN_SECS, Class, Cordon, Done, Held, HostReading, Outcome, POLL_SECS, Params,
    Reason, Record, Report, START_WAIT_SECS, UnitState,
};

use super::prepare::{ask_secs, place, refuse_host, renew};
use super::{Env, failure, lease_failure, open_state, save, success};

/// Runs `cutover`.
pub fn cutover(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let Some(mut record) = loaded else {
        return Err(failure(
            Reason::StateInvalid,
            "there is no record: prepare has not run",
        ));
    };
    let done = go(env, params, &held, &mut record).unwrap_or_else(|done| done);
    // The stop is the commit, on success and on failure; a first install has no stop, and its
    // first act is the move of the link.
    Ok(if begun(&record) {
        irreversible(done)
    } else {
        done
    })
}

/// Whether the first irreversible act is begun: the stop, or for a first install (nothing to
/// stop) the move of the link.
fn begun(record: &Record) -> bool {
    if record.update.previous.is_none() {
        record.link_moved
    } else {
        record.stop_begun
    }
}

fn irreversible(mut done: Done) -> Done {
    done.report.reversible = false;
    done
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    let link = release_link(env, params)?;
    let entered_begun = begun(record);
    // The session the server showed in the last reading before the stop (see `hold`). A rerun
    // that finds the stop begun reads nothing and keeps what the record holds.
    let mut session = None;
    if !entered_begun {
        refuse_host(env)?;
        session = hold(env, held, record, params)?;
    }
    // A first install has no unit to stop.
    if record.update.previous.is_some() {
        // What the machine shows is read before the stop is marked: a read that fails then
        // leaves nothing begun.
        let unit = record.update.unit.clone();
        let state = show(env, &unit, Reason::StopFailed)?;
        let on_staged = linked(env, &link, &record.update.staged)?;
        if !entered_begun {
            // One save: the stop is never marked without the session it is to be judged against
            // (`mark` saves a copy of the record as it stands, with the session in it).
            record.pre_stop_session_ms = session;
            mark(env, held, record, |record| record.stop_begun = true)?;
        }
        stop(env, held, record, params, (state, on_staged), entered_begun)?;
    }
    move_link(env, held, record, &link)?;
    start(env, held, record, params)?;
    Ok(success())
}

/// A failure of something remote or shared: not the release's doing.
fn environment(reason: Reason, detail: impl Into<String>) -> Done {
    let mut done = failure(reason, detail);
    done.report.class = Class::Environment;
    done
}

/// Nothing was stopped, and the host or the cordon is why: try again later.
fn deferred(params: &Params, reason: Reason, detail: impl Into<String>) -> Done {
    Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(reason),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(detail.into()),
            ..Report::default()
        },
    }
}

/// The release link the declaration names, held to the rules the record's paths are held to:
/// plain, inside a declared directory other than the state directory (never one of them
/// itself: the swap writes the link's parent), and in neither release.
fn release_link(env: &Env<'_>, params: &Params) -> Result<std::path::PathBuf, Done> {
    let declaration = env.declaration;
    let link = params.release_link.clone().ok_or_else(|| {
        failure(
            Reason::BadParams,
            "the hook params name no release link (release_link)",
        )
    })?;
    let declared = declaration
        .write_paths
        .iter()
        .any(|dir| *dir != declaration.state_dir && link != *dir && link.starts_with(dir));
    // A link inside a release would be replaced with the release, or hide in it.
    let inside_a_release = link.starts_with(&params.staged_release)
        || params
            .previous_release
            .as_ref()
            .is_some_and(|previous| link.starts_with(previous));
    if is_plain(&link) && declared && !inside_a_release {
        Ok(link)
    } else {
        Err(failure(
            Reason::BadParams,
            format!(
                "the release link {} is not inside a declared directory",
                link.display()
            ),
        ))
    }
}

/// Whether the cordon is one this update placed and confirmed, and has not lapsed.
fn ours(record: &Record, now_ms: u64) -> bool {
    record.cordon.placed && !record.cordon.provisional && now_ms < record.cordon.expires_ms
}

/// Sets a mark in the record, durably, before the act it names.
fn mark(env: &Env<'_>, held: &Held, record: &mut Record, set: fn(&mut Record)) -> Result<(), Done> {
    let mut next = record.clone();
    set(&mut next);
    save(env, held, &next)?;
    *record = next;
    Ok(())
}

/// The host is cordoned and holds no lease, or this ends deferred (or failed) with nothing
/// stopped. Returns the `session_started_ms` of the reading that showed it (`None` on a first
/// install, which reads nothing, and when the server showed no session): the last reading of
/// the host before the stop.
fn hold(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
) -> Result<Option<u64>, Done> {
    if record.update.previous.is_none() {
        return Ok(None);
    }
    let looked = env.ports.lease.host().map_err(lease_failure)?;
    let now = env.ports.clock.now_ms();
    let host = if looked.cordon.is_some() {
        if record.cordon.placed && record.cordon.provisional {
            // Placed by a `prepare` that was killed before it confirmed the mark: it may be
            // ours or an operator's. Nothing renews it, and it could lapse during the stop.
            return Err(deferred(
                params,
                Reason::CordonUnconfirmed,
                "a cordon is in force but the record never confirmed it as ours",
            ));
        }
        if ours(record, now) {
            renew(env, held, record, params)?;
        }
        looked
    } else {
        replace(env, held, record, params, now)?;
        env.ports.lease.host().map_err(lease_failure)?
    };
    if host.leases == 0 && !host.releasing {
        return Ok(host.session_started_ms);
    }
    Err(busy(env, held, record, params, &host).unwrap_or_else(|done| done))
}

/// Puts a cordon back where there is none: the one that was lost, or the one `prepare` never
/// confirmed. If the cap leaves nothing to ask for, the mark is cleared and the step defers.
fn replace(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    now_ms: u64,
) -> Result<(), Done> {
    if ask_secs(&record.cordon, params, now_ms) == 0 {
        // Only a cordon placed before can have used the cap up: forget it.
        record.cordon = Cordon::default();
        save(env, held, record)?;
        return Err(deferred(
            params,
            Reason::CordonCapReached,
            "the cordon is gone and its cap leaves no time to put it back",
        ));
    }
    place(env, held, record, params, false)
}

/// The host holds a lease under the cordon (or in its absence): lift the cordon if it is ours
/// and defer. Nothing was stopped.
fn busy(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    host: &HostReading,
) -> Result<Done, Done> {
    if ours(record, env.ports.clock.now_ms()) {
        env.ports.point();
        env.ports.lease.uncordon().map_err(lease_failure)?;
        record.cordon = Cordon::default();
        save(env, held, record)?;
    }
    Ok(deferred(
        params,
        Reason::DrainBound,
        format!(
            "the host is not idle: {} lease(s), releasing: {}",
            host.leases, host.releasing
        ),
    ))
}

/// What the unit manager says of a unit, in the three ways that matter here.
enum Life {
    /// `active`.
    Up,
    /// `activating`, `reloading` and the like: on its way up, or waiting to restart.
    Starting,
    /// `deactivating`.
    Stopping,
    /// `inactive` or `failed`.
    Down,
}

fn life(state: &UnitState) -> Life {
    match state.active.as_str() {
        "active" => Life::Up,
        "inactive" | "failed" => Life::Down,
        "deactivating" => Life::Stopping,
        _ => Life::Starting,
    }
}

fn show(env: &Env<'_>, unit: &str, reason: Reason) -> Result<UnitState, Done> {
    env.ports
        .units
        .show(unit)
        .map_err(|why| environment(reason, format!("cannot read the state of {unit}: {why}")))
}

/// Whether the link points at the staged release.
fn linked(env: &Env<'_>, link: &Path, staged: &Path) -> Result<bool, Done> {
    let target = env.ports.link.target(link).map_err(|why| {
        environment(
            Reason::LinkFailed,
            format!("cannot read {}: {why}", link.display()),
        )
    })?;
    Ok(target.as_deref() == Some(staged))
}

/// Waits one poll and renews the cordon if it is ours.
fn pause(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<(), Done> {
    env.ports.clock.pause(Duration::from_secs(POLL_SECS));
    if ours(record, env.ports.clock.now_ms()) {
        renew(env, held, record, params)?;
    }
    Ok(())
}

/// Stops the unit and waits for it to be down. A unit that is up on the staged release after
/// the stop was begun is the new agent, started by an earlier run: it is left alone.
fn stop(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    (state, on_staged): (UnitState, bool),
    entered_begun: bool,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    match life(&state) {
        Life::Down => return Ok(()),
        Life::Stopping => {}
        Life::Up | Life::Starting => {
            if entered_begun && on_staged {
                return Ok(());
            }
            env.ports.point();
            env.ports.units.stop_no_block(&unit).map_err(|why| {
                environment(Reason::StopFailed, format!("cannot stop {unit}: {why}"))
            })?;
        }
    }
    // The step is killed at the declared bound; ending a little before it says why.
    let bound_ms = env
        .declaration
        .timeouts
        .cutover
        .saturating_sub(START_WAIT_SECS + CUTOVER_MARGIN_SECS)
        .saturating_mul(1000);
    let began = env.ports.clock.now_ms();
    loop {
        let state = show(env, &unit, Reason::StopFailed)?;
        if matches!(life(&state), Life::Down) {
            return Ok(());
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(environment(
                Reason::StopFailed,
                format!(
                    "{unit} is still {} after {} s",
                    state.active,
                    bound_ms / 1000
                ),
            ));
        }
        pause(env, held, record, params)?;
    }
}

/// Points the link at the staged release.
fn move_link(env: &Env<'_>, held: &Held, record: &mut Record, link: &Path) -> Result<(), Done> {
    let staged = record.update.staged.clone();
    // Read before the mark: on a first install the mark is the commit.
    let on_staged = linked(env, link, &staged)?;
    if !record.link_moved {
        mark(env, held, record, |record| record.link_moved = true)?;
    }
    if on_staged {
        return Ok(());
    }
    env.ports.point();
    env.ports.link.relink(link, &staged).map_err(|why| {
        environment(
            Reason::LinkFailed,
            format!(
                "cannot move {} to {}: {why}",
                link.display(),
                staged.display()
            ),
        )
    })
}

/// A start that failed: the release's fault only if the unit ran and died, now or at an earlier
/// look (`seen_crash`); a unit that is anything else says nothing of the binary.
fn start_failure(env: &Env<'_>, unit: &str, detail: String, seen_crash: bool) -> Done {
    let ran = seen_crash
        || env
            .ports
            .units
            .show(unit)
            .is_ok_and(|state| state.crashed());
    if ran {
        failure(Reason::StartFailed, detail)
    } else {
        environment(Reason::StartFailed, detail)
    }
}

/// Starts the unit and waits for it to be active.
fn start(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    if !record.start_issued {
        mark(env, held, record, |record| record.start_issued = true)?;
    }
    let state = show(env, &unit, Reason::StartFailed)?;
    if matches!(life(&state), Life::Down | Life::Stopping) {
        env.ports.point();
        env.ports.units.start(&unit).map_err(|why| {
            start_failure(env, &unit, format!("cannot start {unit}: {why}"), false)
        })?;
    }
    let bound_ms = START_WAIT_SECS * 1000;
    let began = env.ports.clock.now_ms();
    let mut seen_crash = false;
    loop {
        let state = show(env, &unit, Reason::StartFailed)?;
        seen_crash |= state.crashed();
        match life(&state) {
            Life::Up => return Ok(()),
            Life::Down | Life::Stopping => {
                return Err(start_failure(
                    env,
                    &unit,
                    format!("{unit} is {} after it was started", state.active),
                    seen_crash,
                ));
            }
            Life::Starting => {}
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(start_failure(
                env,
                &unit,
                format!("{unit} is still {} after {START_WAIT_SECS} s", state.active),
                seen_crash,
            ));
        }
        pause(env, held, record, params)?;
    }
}
