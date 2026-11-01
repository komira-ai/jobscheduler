//! `rollback` (host-updates design, 4.2): a repair, not a restore. The updater runs it after a
//! `prepare`, `cutover` or `verify` that failed, timed out, was killed or was deferred. It
//! decides from the record, and from what the unit manager and the release link show, and ends
//! by lifting our cordon and deleting the record (no `cleanup` follows a rollback).
//!
//! In this order:
//! 1. the lock, the record and the params agree. No record is a rollback that has finished
//!    (or an update that never wrote one): nothing to repair. A taint or an `owed-*` record
//!    halts (76) here and at every wait below, wherever the rollback stands; a halt runs
//!    nothing further, and the cordon lapses within one lease;
//! 2. a first install has no previous release to go back to. Before its link moved there is
//!    nothing to undo; after, the record is deleted and the step fails with
//!    `no_previous_release`;
//! 3. the agent was never stopped (`stop_begun` unset): nothing to repair beyond the cordon;
//! 4. otherwise the first matching row, on what the unit and the link show:
//!    - the unit is `deactivating`: wait for it to be down, then the next row;
//!    - the unit is down (a failed or killed `cutover`): move the link back to the previous
//!      release, start the unit, wait for the previous version's hello;
//!    - the unit is up and the link is on the staged release (a `verify` that failed: the new
//!      agent runs): the cordon is still ours, so it should hold no lease, and the step checks
//!      that instead of assuming it. It waits up to the drain bound for the host to hold no
//!      lease, renewing the cordon (`rb.drained`); a lease at the bound ends the step as a
//!      failure and the agent is **not** stopped, for a stop would land on a job of unknown
//!      phase and the agent's own drain would start a second wait; the cordon is left to
//!      lapse. Then it stops the unit (`rb.stop_begun`), waits for it to be down, and goes on
//!      as for a unit that is down;
//!    - the unit is up and the link is on the previous release (a resumed rollback, or an old
//!      agent the manager started again): wait for the hello;
//!    - the link is on neither release: nothing is touched.
//!
//! Each of the four acts (stop, relink, start, and the hello mark) is marked in the record
//! before it is made and is a no-op when its effect is already there, so a rollback killed
//! anywhere and run again does the rest: it asks the unit manager and the link, and trusts what
//! they show before the marks. Once the stop was begun every ending says `reversible: false`:
//! the runs the stop ended are not brought back.
//!
//! The waits share the declared `rollback` timeout, less a margin: the drain gets what is left
//! after the stop and the hello are provided for, the stop what is left after the hello's
//! allowance, and the hello wait at most [`ROLLBACK_HELLO_SECS`] (the start and the hello in
//! one wait). The previous agent's hello must name the previous version and the runtime of the
//! declaration, in a session other than the one the server showed before the stop.

use std::path::Path;

use crate::update_hook::{
    Done, Held, HostReading, Params, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS,
    ROLLBACK_STOP_MIN_SECS, Reason, Record, Report,
};

use super::cutover::{
    Life, begun, environment, irreversible, life, link_target, mark, pause, release_link, show,
    start_failure,
};
use super::prepare::refuse_host;
use super::verify::{Look, judge};
use super::{Env, failure, lease_failure, open_state, state_failure, success};

/// Runs `rollback`.
pub fn rollback(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let Some(mut record) = loaded else {
        return Ok(Done {
            report: Report {
                detail: Some("there is no record: there is nothing to roll back".to_owned()),
                ..Report::default()
            },
            ..success()
        });
    };
    let done = go(env, params, &held, &mut record).unwrap_or_else(|done| done);
    // The stop is the commit, on success and on failure.
    Ok(if begun(&record) {
        irreversible(done)
    } else {
        done
    })
}

/// How much of the declared timeout is left.
struct Budget {
    deadline_ms: u64,
}

impl Budget {
    /// The declared timeout less a margin, counted from now.
    fn new(env: &Env<'_>) -> Budget {
        let secs = env
            .declaration
            .timeouts
            .rollback
            .saturating_sub(ROLLBACK_MARGIN_SECS);
        Budget {
            deadline_ms: env
                .ports
                .clock
                .now_ms()
                .saturating_add(secs.saturating_mul(1000)),
        }
    }

    /// Milliseconds left now, less `reserve_secs` kept for what comes after.
    fn left_ms(&self, env: &Env<'_>, reserve_secs: u64) -> u64 {
        self.deadline_ms
            .saturating_sub(reserve_secs.saturating_mul(1000))
            .saturating_sub(env.ports.clock.now_ms())
    }
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    refuse_host(env)?;
    let Some(previous) = record.update.previous_target.clone() else {
        return first_install(env, held, record);
    };
    if record.stop_begun && !record.rb.hello_ok {
        restore(env, params, held, record, &previous)?;
    }
    finish(env, held, record)
}

/// A first install has no previous release: before its link moved there is nothing to undo,
/// after it there is nothing to go back to. The record goes either way, so that the next order
/// is not refused for it.
fn first_install(env: &Env<'_>, held: &Held, record: &Record) -> Result<Done, Done> {
    env.ports.point();
    held.delete().map_err(state_failure)?;
    if record.link_moved {
        return Err(failure(
            Reason::NoPreviousRelease,
            "a first install has no previous release to go back to",
        ));
    }
    Ok(success())
}

/// Lifts our cordon, if it is still in force, and deletes the record.
fn finish(env: &Env<'_>, held: &Held, record: &Record) -> Result<Done, Done> {
    // A cordon that has lapsed is gone: one in force now is someone else's.
    if record.cordon.placed && env.ports.clock.now_ms() < record.cordon.expires_ms {
        env.ports.point();
        env.ports.lease.uncordon().map_err(lease_failure)?;
    }
    env.ports.point();
    held.delete().map_err(state_failure)?;
    Ok(success())
}

/// The agent was stopped: bring the previous one back.
fn restore(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    previous: &Path,
) -> Result<(), Done> {
    let link = release_link(env, params)?;
    let unit = record.update.unit.clone();
    let state = show(env, &unit, Reason::StopFailed)?;
    let target = link_target(env, &link)?;
    let on_staged = target.as_deref() == Some(record.update.staged.as_path());
    let on_previous = target.as_deref() == Some(previous);
    let budget = Budget::new(env);
    let to_start = match (life(&state), on_staged, on_previous) {
        (Life::Down, ..) => true,
        (Life::Stopping, ..) => {
            wait_down(env, params, held, record, &budget)?;
            true
        }
        (_, true, _) => {
            drain(env, params, held, record, &budget)?;
            stop(env, params, held, record, &budget)?;
            true
        }
        (_, false, true) => false,
        (_, false, false) => {
            return Err(environment(
                Reason::LinkFailed,
                format!(
                    "{} is on neither release, and {unit} runs: nothing is touched",
                    link.display()
                ),
            ));
        }
    };
    if to_start {
        relink(env, held, record, &link, previous, on_previous)?;
        start(env, held, record)?;
    }
    wait_hello(env, params, held, record, &budget)?;
    mark(env, held, record, |record| record.rb.hello_ok = true)
}

/// A wait's tick: a taint or an owed check halts, then one poll, renewing the cordon.
fn tick(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<(), Done> {
    refuse_host(env)?;
    pause(env, held, record, params)
}

/// Waits for the host to hold no lease, up to the drain bound: the declared wait, cut to what
/// the declared timeout leaves after the stop and the hello are provided for.
fn drain(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    budget: &Budget,
) -> Result<(), Done> {
    if record.rb.drained {
        return Ok(());
    }
    let bound_ms = params
        .drain_wait_secs
        .saturating_mul(1000)
        .min(budget.left_ms(env, ROLLBACK_HELLO_SECS + ROLLBACK_STOP_MIN_SECS));
    let began = env.ports.clock.now_ms();
    loop {
        let host = env.ports.lease.host().map_err(lease_failure)?;
        if host.leases == 0 && !host.releasing {
            break;
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(still_busy(&host, bound_ms / 1000));
        }
        tick(env, params, held, record)?;
    }
    mark(env, held, record, |record| record.rb.drained = true)
}

/// A lease under our cordon at the bound: a fault to see, not to repair around.
fn still_busy(host: &HostReading, secs: u64) -> Done {
    environment(
        Reason::DrainBound,
        format!(
            "the host still holds {} lease(s) (releasing: {}) {secs} s into the rollback, \
             under our cordon: the agent is not stopped and the cordon is left to lapse",
            host.leases, host.releasing
        ),
    )
}

/// Stops the unit and waits for it to be down.
fn stop(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    budget: &Budget,
) -> Result<(), Done> {
    if !record.rb.stop_begun {
        mark(env, held, record, |record| record.rb.stop_begun = true)?;
    }
    let unit = record.update.unit.clone();
    env.ports.point();
    env.ports
        .units
        .stop_no_block(&unit)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {unit}: {why}")))?;
    wait_down(env, params, held, record, budget)
}

/// Waits for the unit to be down, up to what the declared timeout leaves after the hello's
/// allowance.
fn wait_down(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    budget: &Budget,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    let bound_ms = budget.left_ms(env, ROLLBACK_HELLO_SECS);
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
        tick(env, params, held, record)?;
    }
}

/// Points the link at the previous release.
fn relink(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    link: &Path,
    previous: &Path,
    on_previous: bool,
) -> Result<(), Done> {
    if !record.rb.link_restored {
        mark(env, held, record, |record| record.rb.link_restored = true)?;
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

/// Starts the unit.
fn start(env: &Env<'_>, held: &Held, record: &mut Record) -> Result<(), Done> {
    if !record.rb.start_issued {
        mark(env, held, record, |record| record.rb.start_issued = true)?;
    }
    let unit = record.update.unit.clone();
    env.ports.point();
    // The unit may still show the failed run being rolled back: a start that errors says
    // nothing of the previous release.
    env.ports
        .units
        .start(&unit)
        .map_err(|why| environment(Reason::StartFailed, format!("cannot start {unit}: {why}")))
}

/// Looks until the previous agent says hello, or the hello wait ends.
fn wait_hello(
    env: &Env<'_>,
    params: &Params,
    held: &Held,
    record: &mut Record,
    budget: &Budget,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    let version = record.update.previous.clone().unwrap_or_default();
    let bound_ms = (ROLLBACK_HELLO_SECS * 1000).min(budget.left_ms(env, 0));
    let began = env.ports.clock.now_ms();
    let mut last = Look::Silent;
    let mut seen_crash = false;
    loop {
        let state = show(env, &unit, Reason::StartFailed)?;
        seen_crash |= state.crashed();
        match life(&state) {
            Life::Down | Life::Stopping => {
                return Err(start_failure(
                    env,
                    &unit,
                    format!("{unit} is {} after it was started", state.active),
                    seen_crash,
                ));
            }
            Life::Up => {
                last = match env.ports.lease.host() {
                    Ok(host) => {
                        match judge(&host, record, &version, env.declaration.hello_runtime()) {
                            Ok(()) => return Ok(()),
                            Err(look) => look,
                        }
                    }
                    Err(why) => Look::Unreachable(why),
                };
            }
            Life::Starting => {}
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(match last {
                Look::Unreachable(why) => lease_failure(why),
                Look::Wrong(detail) => failure(Reason::VersionMismatch, detail),
                Look::Silent => failure(
                    Reason::NoHello,
                    format!(
                        "the previous agent {version} said no hello in {} s",
                        bound_ms / 1000
                    ),
                ),
            });
        }
        tick(env, params, held, record)?;
    }
}
