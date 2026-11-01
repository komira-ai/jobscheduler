//! `rollback` (host-updates design, 4.2): put the previous release back. It is run after a
//! failed, killed or deferred `prepare` or `cutover`, and after a `verify` that failed; it
//! decides from the record and from what the machine shows, and a rerun after a kill at any
//! point goes on from there and makes no act twice.
//!
//! In this order, each before anything is changed:
//! 1. the lock, the record and the params agree; a taint or an `owed-*` record halts (76), here
//!    and again before each phase below (a tainted agent refuses every start, the old release
//!    included, so nothing is gained by going on). A rollback with no record has nothing to
//!    undo: it has finished before, or `prepare` never wrote one;
//! 2. the agent was never signalled (no `stop_begun`; for a first install, no `link_moved`):
//!    there is nothing to repair beyond the cordon. It is lifted if this update placed it, the
//!    record is deleted, and the step ends `reversible: true`;
//! 3. a first install that has moved the link (`link_moved`) has no previous release to
//!    restore. The step touches nothing, keeps the record and fails with `no_previous_release`:
//!    the record is what makes every later rollback say the same, and what lets the next order
//!    of the same release go forward from where it stopped. It stops no agent: that agent may
//!    hold a job, no cordon was placed to drain it under, and a stop inside a job's owed-checks
//!    window taints the host for good;
//! 4. otherwise the unit and the release link are read, and the first row that matches is taken
//!    (the system is believed over the marks): a unit that is `deactivating` is waited for and
//!    then taken as stopped; a unit that is up on the staged release is the new agent and is
//!    drained and stopped; a unit that is up on the previous release needs no act; a unit that
//!    is up on any other release is `state_invalid`, nothing done; a stopped unit needs the
//!    link and the start;
//! 5. drain (new agent only; the cordon is kept up, or put back if it lapsed, within the cap):
//!    wait up to the drain bound for the host to hold no lease, then mark `rb.drained`. A lease
//!    after the bound fails the step (`drain_bound`, class environment) with nothing stopped
//!    and the cordon left to lapse: the SIGTERM would land on a job of unknown phase;
//! 6. stop (`rb.stop_begun` first, then the stop and a wait for the unit to be down), the link
//!    back to the previous release (`rb.link_restored` first), the start (`rb.start_issued`
//!    first) and the wait for the previous agent's hello, and then `rb.hello_ok`, after it;
//! 7. the cordon is lifted if this update placed it and it has not lapsed or been left
//!    unconfirmed, and the record is deleted. A lease server that does not answer the lift
//!    does not hold the step: the cordon is ours and nothing renews it any more, so it lapses
//!    within one lease; the step ends in success and says `cordon left to lapse`.
//!
//! The hello that proves the previous agent is a live session other than the one the server
//! showed in the last reading before this step's first act (recorded with the mark of that act
//! as `rb_pre_stop_session_ms`, and compared by equality: both are the server's stamps), of the
//! previous version, on the execution mode the declared runtime names
//! (`Declaration::hello_runtime`). A step that makes no act (the previous agent already runs on
//! the previous release) asks for no other session.
//!
//! A hello that names no runtime is accepted by version alone. Only a previous agent from before
//! the hello carried the runtime can send one, and neither the record (which holds the previous
//! version as a string) nor the declaration says which versions report it; requiring it would
//! refuse exactly the old agents a rollback exists to bring back. A hello that names one must
//! name the declared mode.
//!
//! Whose cordon it lifts is `cutover`'s rule: only one marked as placed by this update, not
//! left unconfirmed, and not past its expiry; an operator's, and one that may be anyone's, are
//! never touched, renewed or placed again.
//!
//! The ending: the record is deleted (no `cleanup` follows a rollback), so a rerun finds none
//! and says so (`reversible: true`: it knows nothing, and the updater does not decide with the
//! flag on a success). A tombstone would have to be cleared by the next `prepare`, which
//! cannot tell it from the record of an update that was abandoned.
//!
//! Exit codes: 0; 1 (every ending after the stop says `reversible: false`); 76 on a taint or an
//! owed check. Never 75: the declared contract gives a rollback no deferral. A lease server that
//! does not answer before the first act is waited for inside the step, up to the declared bound
//! less the waits still to come, and then the step fails with `lease_unreachable` (class
//! environment) having changed nothing.

use std::path::Path;
use std::time::Duration;

use crate::update_hook::{
    Cordon, Done, Held, HostReading, LeaseError, POLL_SECS, Params, ROLLBACK_HELLO_SECS,
    ROLLBACK_MARGIN_SECS, Reason, Record, Report, START_WAIT_SECS,
};

use super::cutover::{
    Life, begun, bring_up, environment, irreversible, life, linked, mark, ours, pause,
    release_link, show,
};
use super::prepare::{place, refuse_host, renew};
use super::{Env, failure, lease_failure, open_state, save, state_failure, success};

/// Runs `rollback`.
pub fn rollback(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let Some(mut record) = loaded else {
        refuse_host(env)?;
        return Ok(Done {
            report: Report {
                detail: Some(
                    "there is no record: the rollback has finished, or prepare never wrote one"
                        .to_owned(),
                ),
                ..Report::default()
            },
            ..success()
        });
    };
    let done = go(env, params, &held, &mut record).unwrap_or_else(|done| done);
    // Once the agent was stopped (or a first install moved its link) nothing is reversible.
    Ok(if begun(&record) {
        irreversible(done)
    } else {
        done
    })
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    refuse_host(env)?;
    if !begun(record) {
        return finish(env, held, record);
    }
    let Some(previous) = record.update.previous_target.clone() else {
        return Err(failure(
            Reason::NoPreviousRelease,
            "a first install has no previous release to restore: nothing was stopped or moved",
        ));
    };
    if !record.rb.hello_ok {
        let link = release_link(env, params)?;
        restore(env, params, held, record, &link, &previous)?;
    }
    finish(env, held, record)
}

/// Where the release link points, among the two releases of the update.
enum At {
    Previous,
    Staged,
    Other,
}

fn link_at(env: &Env<'_>, link: &Path, record: &Record, previous: &Path) -> Result<At, Done> {
    if linked(env, link, previous)? {
        Ok(At::Previous)
    } else if linked(env, link, &record.update.staged)? {
        Ok(At::Staged)
    } else {
        Ok(At::Other)
    }
}

/// Everything from the unit's state to the previous agent's hello.
fn restore(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    link: &Path,
    previous: &Path,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    let state = show(env, &unit, Reason::StopFailed)?;
    let at = link_at(env, link, record, previous)?;
    let mut down = matches!(life(&state), Life::Down);
    if matches!(life(&state), Life::Stopping) {
        wait_down(env, params, held, record, &unit)?;
        down = true;
    }
    if !down {
        match at {
            At::Previous => return await_hello(env, params, held, record),
            At::Other => {
                return Err(failure(
                    Reason::StateInvalid,
                    format!(
                        "{unit} runs from a release link that is neither the staged nor the previous release"
                    ),
                ));
            }
            At::Staged => {
                drain(env, params, held, record)?;
                stop(env, params, held, record, &unit)?;
            }
        }
    }
    refuse_host(env)?;
    relink(env, held, record, link, previous)?;
    refuse_host(env)?;
    if !record.rb.start_issued {
        mark_act(env, held, record, set_start_issued)?;
    }
    bring_up(env, held, record, params)?;
    await_hello(env, params, held, record)
}

fn set_start_issued(record: &mut Record) {
    record.rb.start_issued = true;
}

fn set_stop_begun(record: &mut Record) {
    record.rb.stop_begun = true;
}

fn set_link_restored(record: &mut Record) {
    record.rb.link_restored = true;
}

fn set_drained(record: &mut Record) {
    record.rb.drained = true;
}

fn set_hello_ok(record: &mut Record) {
    record.rb.hello_ok = true;
}

/// The host as the lease server shows it. A server that does not answer is waited for, up to the
/// declared bound less the waits still to come, and then the step fails as an environment matter
/// (`lease_unreachable`): asked only before this step's first act, so nothing is half-done.
fn read_host(env: &Env<'_>) -> Result<HostReading, Done> {
    // What is left of the declared bound for the other waits: the start, the hello and a margin.
    let bound_ms = env
        .declaration
        .timeouts
        .rollback
        .saturating_sub(START_WAIT_SECS)
        .saturating_sub(ROLLBACK_HELLO_SECS)
        .saturating_sub(ROLLBACK_MARGIN_SECS)
        .saturating_mul(1000);
    let began = env.ports.clock.now_ms();
    loop {
        match env.ports.lease.host() {
            Ok(host) => return Ok(host),
            Err(why) => {
                if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
                    return Err(lease_failure(why));
                }
            }
        }
        env.ports.clock.pause(Duration::from_secs(POLL_SECS));
    }
}

/// Sets a mark of an act in the record, durably, before the act it names. The first of them is
/// saved with the session the lease server shows now: the one the previous agent's must differ
/// from. A server that does not answer ends the step here, deferred, with nothing begun.
fn mark_act(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    set: fn(&mut Record),
) -> Result<(), Done> {
    let rb = &record.rb;
    let acted = rb.stop_begun || rb.link_restored || rb.start_issued;
    let mut next = record.clone();
    if !acted {
        let host = read_host(env)?;
        next.rb_pre_stop_session_ms = host.session_started_ms;
    }
    set(&mut next);
    save(env, held, &next)?;
    *record = next;
    Ok(())
}

/// Keeps the host out of placement: a cordon of ours is renewed, one that is not ours is left
/// alone, and a cordon that is gone is put back within the cap (a cap that is spent fails the
/// step: the stop must not land on a job that came in).
fn keep_out(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    host: &HostReading,
) -> Result<(), Done> {
    if host.cordon.is_some() {
        if ours(record, env.ports.clock.now_ms()) {
            renew(env, held, record, params)?;
        }
        return Ok(());
    }
    place(env, held, record, params, false)
}

/// Waits up to the drain bound for the host to hold no lease, keeping the cordon up, and marks
/// it. Nothing is stopped if a lease is still there.
fn drain(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<(), Done> {
    // Once seen it is not waited for again: a rerun goes on to the stop.
    if record.rb.drained {
        return Ok(());
    }
    let bound_ms = params.drain_wait_secs.saturating_mul(1000);
    let began = env.ports.clock.now_ms();
    loop {
        refuse_host(env)?;
        let host = read_host(env)?;
        keep_out(env, params, held, record, &host)?;
        if host.leases == 0 && !host.releasing {
            break;
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(environment(
                Reason::DrainBound,
                format!(
                    "the host still held {} lease(s) after {} s under the cordon: the agent is not stopped and the cordon is left to lapse",
                    host.leases, params.drain_wait_secs
                ),
            ));
        }
        pause(env, held, record, params)?;
    }
    // After the observation, so that a mark means it was made.
    mark(env, held, record, set_drained)
}

/// The longest the wait for the unit to stop may be: what the declared bound leaves after the
/// drain, the start, the hello and a margin.
fn stop_bound_ms(env: &Env<'_>, params: &Params) -> u64 {
    env.declaration
        .timeouts
        .rollback
        .saturating_sub(params.drain_wait_secs)
        .saturating_sub(START_WAIT_SECS)
        .saturating_sub(ROLLBACK_HELLO_SECS)
        .saturating_sub(ROLLBACK_MARGIN_SECS)
        .saturating_mul(1000)
}

/// Marks the stop, signals the unit and waits for it to be down.
fn stop(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    unit: &str,
) -> Result<(), Done> {
    refuse_host(env)?;
    if !record.rb.stop_begun {
        mark_act(env, held, record, set_stop_begun)?;
    }
    env.ports.point();
    env.ports
        .units
        .stop_no_block(unit)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {unit}: {why}")))?;
    wait_down(env, params, held, record, unit)
}

fn wait_down(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    unit: &str,
) -> Result<(), Done> {
    let bound_ms = stop_bound_ms(env, params);
    let began = env.ports.clock.now_ms();
    loop {
        refuse_host(env)?;
        let state = show(env, unit, Reason::StopFailed)?;
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

/// Marks the link as restored, and moves it unless it is on the previous release already.
fn relink(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    link: &Path,
    previous: &Path,
) -> Result<(), Done> {
    // Read before the mark: a read that fails leaves nothing begun.
    let on_previous = linked(env, link, previous)?;
    if !record.rb.link_restored {
        mark_act(env, held, record, set_link_restored)?;
    }
    if on_previous {
        return Ok(());
    }
    env.ports.point();
    env.ports.link.relink(link, previous).map_err(|why| {
        environment(
            Reason::LinkFailed,
            format!(
                "cannot move {} back to {}: {why}",
                link.display(),
                previous.display()
            ),
        )
    })
}

/// What the last look of the lease server showed.
enum Look {
    /// No other session said hello (not yet, or the server shows the one of before).
    Silent,
    /// Another session said hello, and it is not the previous agent.
    Wrong(String),
    /// The server did not answer.
    Unreachable(LeaseError),
}

/// What a reading of the host says of the previous agent: `Ok` is proof. After an act of this
/// step the session must differ from the one recorded before it, by equality (both are the
/// server's stamps, never compared with the host's clock); with no act made the agent that
/// runs is the one that always did, and no other session is asked for.
fn judge(host: &HostReading, record: &Record, previous: &str, runtime: &str) -> Result<(), Look> {
    let rb = &record.rb;
    let acted = rb.stop_begun || rb.link_restored || rb.start_issued;
    let other_session = match host.session_started_ms {
        None => false,
        Some(session) => !acted || Some(session) != record.rb_pre_stop_session_ms,
    };
    let new_session = host.state == "live" && other_session;
    if !new_session {
        return Err(Look::Silent);
    }
    let version_ok = host.agent_version.as_deref() == Some(previous);
    let runtime_ok = match host.runtime.as_deref() {
        None => true,
        Some(reported) => reported == runtime,
    };
    if version_ok && runtime_ok {
        Ok(())
    } else {
        Err(Look::Wrong(format!(
            "the session is {} on {}, not {previous} on {runtime}",
            host.agent_version.as_deref().unwrap_or("of no version"),
            host.runtime.as_deref().unwrap_or("no runtime"),
        )))
    }
}

/// Looks until the previous agent is proved, or the bound passes, and then marks it.
fn await_hello(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    let previous = record.update.previous.clone().unwrap_or_default();
    let bound_ms = ROLLBACK_HELLO_SECS * 1000;
    let began = env.ports.clock.now_ms();
    let mut last = Look::Silent;
    loop {
        refuse_host(env)?;
        let state = show(env, &unit, Reason::StartFailed)?;
        let down = matches!(
            state.active.as_str(),
            "failed" | "inactive" | "deactivating"
        ) || state.sub == "auto-restart";
        if down {
            return Err(failure(
                Reason::StartFailed,
                format!(
                    "{unit} is {} ({}) while the previous agent is awaited",
                    state.active, state.sub
                ),
            ));
        }
        if state.active == "active" {
            last = match env.ports.lease.host() {
                Ok(host) => {
                    match judge(&host, record, &previous, env.declaration.hello_runtime()) {
                        Ok(()) => break,
                        Err(look) => look,
                    }
                }
                Err(why) => Look::Unreachable(why),
            };
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(match last {
                Look::Unreachable(why) => lease_failure(why),
                Look::Wrong(detail) => failure(Reason::VersionMismatch, detail),
                Look::Silent => failure(
                    Reason::NoHello,
                    format!(
                        "no session of the previous agent said hello in {} s",
                        bound_ms / 1000
                    ),
                ),
            });
        }
        pause(env, held, record, params)?;
    }
    // After the observation, so that a mark means it was made (a rerun with the mark set does
    // not come here). A step that made no act has nothing to follow: the rules let
    // `rb.hello_ok` follow `rb.start_issued` only.
    if record.rb.start_issued {
        mark(env, held, record, set_hello_ok)?;
    }
    Ok(())
}

/// Lifts the cordon if this update placed it and it is still ours, and deletes the record.
fn finish(env: &Env<'_>, held: &Held, record: &mut Record) -> Result<Done, Done> {
    let mut note = None;
    if ours(record, env.ports.clock.now_ms()) {
        env.ports.point();
        match env.ports.lease.uncordon() {
            Ok(_) => {
                // So that a kill before the delete does not lift, on a rerun, a cordon placed since.
                record.cordon = Cordon::default();
                save(env, held, record)?;
            }
            // Nothing renews it any more: it lapses within one lease.
            Err(why) => note = Some(format!("cordon left to lapse (could not be lifted: {why})")),
        }
    }
    env.ports.point();
    held.delete().map_err(state_failure)?;
    Ok(Done {
        report: Report {
            detail: note,
            ..Report::default()
        },
        ..success()
    })
}
