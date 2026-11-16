//! The steps of the config hook, over the ports: `prepare`, `cutover`, `verify`, `rollback`
//! and `cleanup`. `uninstall` does nothing and has no body here.
//!
//! A config package's files are in place before `cutover` (the installer's activate) and the
//! previous version's are back before `rollback`. What the hook adds is the hand-over: the
//! agent reads its flags once, at its start, so a new flags file takes effect by restarting the
//! agent's unit, and the proof that it did is the agent's hello on the lease server, a session
//! other than the one the server showed before the restart.
//!
//! Every step but `prepare` and `cleanup` ends with the unit as it found it or restarted;
//! none changes the flags file. Each takes the state directory's lock first (making the
//! directory when it is not there) and loads the record, which must be of the unit the params
//! name.

use std::time::Duration;

use js_update_hook::{
    CUTOVER_MARGIN_SECS, Clock, Outcome, POLL_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    UnitState, Units, VERIFY_MARGIN_SECS,
};

use crate::update_hook::step::{Life, life};
use crate::update_hook::{Class, HostReading, Lease, LeaseError, StatFn};

use super::Done;
use super::params::{HELLO_WAIT_SECS, Params};
use super::reason::{Reason, Report};
use super::record::{Held, Invalid, Record, StateError};

/// How the lease server's own-host routes are had for the flags file now in place. The error is
/// why the flags file cannot be used: it does not parse as the agent's flags, or names no usable
/// host id. A server that cannot be reached is not an error here: the lease answers every call
/// with it.
pub type LeaseOf<'a> = dyn Fn(&Params) -> Result<Box<dyn Lease + 'a>, String> + 'a;

/// What a step touches.
pub struct Env<'a> {
    /// The unit manager.
    pub units: &'a dyn Units,
    /// Time, and waiting for it to pass.
    pub clock: &'a dyn Clock,
    /// How a directory of the state directory's path is looked at (see
    /// [`fd_stat`](crate::update_hook::fd_stat)).
    pub stat: &'a StatFn,
    /// The lease server's own-host routes for the host the flags file now in place names.
    pub lease: &'a LeaseOf<'a>,
}

fn ended(outcome: Outcome, report: Report) -> Done {
    Done { outcome, report }
}

fn success() -> Done {
    ended(Outcome::Success, Report::default())
}

/// A success that has something to say.
fn success_with(detail: impl Into<String>) -> Done {
    ended(
        Outcome::Success,
        Report {
            detail: Some(detail.into()),
            ..Report::default()
        },
    )
}

/// A failure of the step's own: exit 1, a `release` matter, reversible.
fn failure(reason: Reason, detail: impl Into<String>) -> Done {
    ended(Outcome::Failure, Report::failure(reason, detail))
}

/// A failure that says nothing of the config: the host or a service it needs.
fn environment(reason: Reason, detail: impl Into<String>) -> Done {
    let mut done = failure(reason, detail);
    done.report.class = Class::Environment;
    done
}

/// The state directory, the lock or the record is not as the hook requires.
fn state_failure(error: StateError) -> Done {
    failure(error.reason(), error.to_string())
}

/// The lease server did not answer as asked: not the config's fault.
fn lease_failure(error: LeaseError) -> Done {
    environment(Reason::LeaseUnreachable, error.to_string())
}

/// Takes the lock, loads the record and holds it to the rules.
fn open_state(env: &Env<'_>, params: &Params) -> Result<(Held, Option<Record>), Done> {
    let held = Held::open_or_make(&params.state_dir, env.stat).map_err(state_failure)?;
    let record = held.load().map_err(state_failure)?;
    if let Some(record) = record.as_ref().filter(|record| record.unit != params.unit) {
        return Err(state_failure(StateError::Invalid(Invalid::Unit(
            record.unit.clone(),
        ))));
    }
    Ok((held, record))
}

fn save(held: &Held, record: &Record) -> Result<(), Done> {
    held.save(record).map_err(state_failure)
}

fn show(env: &Env<'_>, unit: &str, reason: Reason) -> Result<UnitState, Done> {
    env.units
        .show(unit)
        .map_err(|why| environment(reason, format!("cannot read the state of {unit}: {why}")))
}

/// The session the server shows the host in; `None` when it cannot be asked, does not know the
/// host or shows none.
fn session_of(lease: &dyn Lease) -> Option<u64> {
    lease.host().ok().and_then(|host| host.session_started_ms)
}

/// Whether `host` is live in a session other than `before` (an identifier, compared for
/// equality only: the host's clock is never compared with the server's).
fn is_new_session(host: &HostReading, before: Option<u64>) -> bool {
    host.state == "live"
        && host
            .session_started_ms
            .is_some_and(|session| Some(session) != before)
}

/// `prepare`: the params were read and are sound, which is all there is to check. It takes the
/// lock, so that a step still running refuses it, and changes nothing.
#[must_use]
pub fn prepare(env: &Env<'_>, params: &Params) -> Done {
    open_state(env, params).map_or_else(|done| done, |_| success())
}

/// `cutover`: checks the flags file now in place, notes the host's session, restarts the unit.
///
/// A flags file the agent could not use ends the step `bad_config`, reversible, before anything
/// is restarted: hostkeeper then puts the previous files back. The session the server shows is
/// recorded before the stop, once; a rerun after the restart does nothing.
#[must_use]
pub fn cutover(env: &Env<'_>, params: &Params) -> Done {
    cutover_run(env, params).unwrap_or_else(|done| done)
}

fn cutover_run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let mut record = loaded.unwrap_or_else(|| Record::new(params));
    if record.restarted {
        return Ok(success_with("the unit was restarted by an earlier run"));
    }
    let lease = (env.lease)(params).map_err(|why| failure(Reason::BadConfig, why))?;
    if !record.session_read {
        record.session_read = true;
        record.session_started_ms = session_of(lease.as_ref());
    }
    record.restart_begun = true;
    save(&held, &record)?;
    let stop_secs = params
        .cutover_secs
        .saturating_sub(START_WAIT_SECS + CUTOVER_MARGIN_SECS);
    restart(env, &params.unit, stop_secs)?;
    record.restarted = true;
    save(&held, &record)?;
    Ok(success())
}

/// Stops the unit, waits (at most `stop_secs`) for it to be down, starts it and waits for it to
/// be active, for at most the start wait.
fn restart(env: &Env<'_>, unit: &str, stop_secs: u64) -> Result<(), Done> {
    let state = show(env, unit, Reason::StopFailed)?;
    if !matches!(life(&state), Life::Down) {
        if !matches!(life(&state), Life::Stopping) {
            env.units.stop_no_block(unit).map_err(|why| {
                environment(Reason::StopFailed, format!("cannot stop {unit}: {why}"))
            })?;
        }
        wait_down(env, unit, stop_secs)?;
    }
    env.units
        .start(unit)
        .map_err(|why| environment(Reason::StartFailed, format!("cannot start {unit}: {why}")))?;
    wait_up(env, unit)
}

fn wait_down(env: &Env<'_>, unit: &str, stop_secs: u64) -> Result<(), Done> {
    let began = env.clock.now_ms();
    loop {
        let state = show(env, unit, Reason::StopFailed)?;
        if matches!(life(&state), Life::Down) {
            return Ok(());
        }
        if env.clock.now_ms().saturating_sub(began) >= stop_secs * 1000 {
            return Err(environment(
                Reason::StopFailed,
                format!("{unit} is still {} after {stop_secs} s", state.active),
            ));
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
}

/// Waits for the unit just started to be active. A unit that ran and died is the config's
/// fault (a flags file the agent runs badly on); one that never ran is the host's.
fn wait_up(env: &Env<'_>, unit: &str) -> Result<(), Done> {
    let began = env.clock.now_ms();
    let mut seen_crash = false;
    loop {
        let state = show(env, unit, Reason::StartFailed)?;
        seen_crash |= state.crashed();
        let ending = match life(&state) {
            Life::Up => return Ok(()),
            Life::Down | Life::Stopping => {
                format!("{unit} is {} after it was started", state.active)
            }
            Life::Starting
                if env.clock.now_ms().saturating_sub(began) >= START_WAIT_SECS * 1000 =>
            {
                format!("{unit} is still {} after {START_WAIT_SECS} s", state.active)
            }
            Life::Starting => {
                env.clock.pause(Duration::from_secs(POLL_SECS));
                continue;
            }
        };
        return Err(if seen_crash {
            failure(Reason::StartFailed, ending)
        } else {
            environment(Reason::StartFailed, ending)
        });
    }
}

/// `verify`: looks, again and again until a bound, for the agent's hello after the restart: the
/// unit active and the lease server showing the host `live` in a session other than the one
/// `cutover` recorded (any session, when none was recorded).
///
/// The wait ends at the declared `verify` timeout less a margin. Then the failure is the
/// config's (`no_hello`) unless the last look of the lease server failed: that is
/// `lease_unreachable`, class environment, and says nothing of the config. A unit that is down
/// ends the step at once as `start_failed`.
#[must_use]
pub fn verify(env: &Env<'_>, params: &Params) -> Done {
    verify_run(env, params).unwrap_or_else(|done| done)
}

fn verify_run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let Some(mut record) = loaded.filter(|record| record.restarted) else {
        return Err(failure(
            Reason::StateInvalid,
            "the record does not say that cutover restarted the unit",
        ));
    };
    if record.verified {
        return Ok(success());
    }
    let lease = (env.lease)(params).map_err(|why| failure(Reason::BadConfig, why))?;
    let bound_ms = params
        .verify_secs
        .saturating_sub(VERIFY_MARGIN_SECS)
        .saturating_mul(1000);
    let began = env.clock.now_ms();
    let mut last: Option<LeaseError> = None;
    loop {
        let state = show(env, &params.unit, Reason::StartFailed)?;
        if matches!(life(&state), Life::Down | Life::Stopping) {
            return Err(failure(
                Reason::StartFailed,
                format!(
                    "{} is {} ({}) during verify",
                    params.unit, state.active, state.sub
                ),
            ));
        }
        if matches!(life(&state), Life::Up) {
            match lease.host() {
                Ok(host) if is_new_session(&host, record.session_started_ms) => break,
                Ok(_) | Err(LeaseError::UnknownHost) => last = None,
                Err(why) => last = Some(why),
            }
        }
        if env.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(last.map_or_else(
                || {
                    failure(
                        Reason::NoHello,
                        format!(
                            "no new session of the host said hello in {} s",
                            bound_ms / 1000
                        ),
                    )
                },
                lease_failure,
            ));
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
    // After the observation, so that a mark means it was made.
    record.verified = true;
    save(&held, &record)?;
    Ok(success())
}

/// `rollback`: the previous files are back, so the unit is restarted onto them, and the agent's
/// hello is waited for when the join token file (`token_file`) exists. A missing hello never fails the
/// rollback (a host must not be wedged by it): it is told in the detail.
///
/// A `cutover` that never began the restart (a flags file refused, a step that never ran) left
/// the unit on the config it has: there is nothing to undo, and the unit is not touched. The
/// record is deleted at the end, so a rerun restarts nothing.
#[must_use]
pub fn rollback(env: &Env<'_>, params: &Params) -> Done {
    rollback_run(env, params).unwrap_or_else(|done| done)
}

fn rollback_run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    if !loaded.is_some_and(|record| record.restart_begun) {
        held.delete().map_err(state_failure)?;
        return Ok(success_with(
            "cutover restarted nothing: the unit is left as it is",
        ));
    }
    let lease = (env.lease)(params);
    let waiting = params.token_file.exists();
    let before = lease
        .as_ref()
        .ok()
        .filter(|_| waiting)
        .and_then(|lease| session_of(lease.as_ref()));
    let stop_secs = params
        .rollback_secs
        .saturating_sub(START_WAIT_SECS + HELLO_WAIT_SECS + ROLLBACK_MARGIN_SECS);
    restart(env, &params.unit, stop_secs)?;
    let hello = match (waiting, lease) {
        (false, _) => "no join token file: not waiting for a hello".to_owned(),
        (true, Err(why)) => format!("not waiting for a hello: {why}"),
        (true, Ok(lease)) => hello_after(env, lease.as_ref(), before),
    };
    held.delete().map_err(state_failure)?;
    Ok(success_with(format!("the unit was restarted; {hello}")))
}

/// Waits, for at most [`HELLO_WAIT_SECS`], for the host to be live in a session other than
/// `before`; says what it saw.
fn hello_after(env: &Env<'_>, lease: &dyn Lease, before: Option<u64>) -> String {
    let began = env.clock.now_ms();
    loop {
        let seen = match lease.host() {
            Ok(host) if is_new_session(&host, before) => return "the agent said hello".to_owned(),
            Ok(_) => "no new session".to_owned(),
            Err(why) => why.to_string(),
        };
        if env.clock.now_ms().saturating_sub(began) >= HELLO_WAIT_SECS * 1000 {
            return format!("no hello in {HELLO_WAIT_SECS} s: {seen}");
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
}

/// `cleanup`: deletes the record, once `verify` has succeeded. A `cleanup` that finds no record
/// has finished before.
#[must_use]
pub fn cleanup(env: &Env<'_>, params: &Params) -> Done {
    cleanup_run(env, params).unwrap_or_else(|done| done)
}

fn cleanup_run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let Some(record) = loaded else {
        return Ok(success_with("there is no record: cleanup has finished"));
    };
    if !record.verified {
        return Err(failure(
            Reason::NotVerified,
            "the record does not say that verify succeeded",
        ));
    }
    held.delete().map_err(state_failure)?;
    Ok(success())
}
