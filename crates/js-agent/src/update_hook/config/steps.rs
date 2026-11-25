//! The steps of the config hook, over the ports: `prepare`, `cutover`, `verify`, `rollback`
//! and `cleanup`. `uninstall` does nothing and has no body here.
//!
//! A config package's files are in place before `cutover` (the installer's activate). Before
//! `rollback` an update's previous version is back, but a first install has no previous version:
//! the installer has replaced whatever file was at the package's paths (one a person placed) and
//! removes the package only after `rollback`. What the hook adds is the hand-over: the agent
//! reads its flags once, at its start, so a new flags file takes effect by restarting the
//! agent's unit, and the proof that it did is the agent's hello on the lease server, a session
//! other than the one the server showed before the restart. And it keeps the files the package
//! replaces as they were (see [`kept`](super::kept)), so that `rollback` puts them back before
//! it restarts the unit, first install or not.
//!
//! Every step but `prepare` and `cleanup` ends with the unit as it found it or restarted;
//! only `rollback` changes the files the package placed, to put back what was kept. Each takes
//! the state directory's lock first (making the directory when it is not there) and loads the
//! record, which must be of the unit the params name.

use std::path::Path;
use std::time::Duration;

use js_update_hook::{
    CUTOVER_MARGIN_SECS, Clock, Outcome, POLL_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS,
    UnitState, Units, VERIFY_MARGIN_SECS,
};

use crate::update_hook::step::{Life, life};
use crate::update_hook::{Class, HostReading, Lease, LeaseError, StatFn};

use super::Done;
use super::kept::{self, KeptError};
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
    /// The host's root, below which a package's files are placed (`/` on a machine).
    pub root: &'a Path,
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

/// The kept copy could not be made or removed.
fn keep_failure(error: KeptError) -> Done {
    environment(Reason::KeepFailed, error.to_string())
}

/// How long hostkeeper waits before it tries a `cutover` again that could not ask the lease
/// server which session the host is in.
pub const CUTOVER_RETRY_SECS: u64 = 60;

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

/// `prepare`: the params were read and are sound. It takes the lock, so that a step still
/// running refuses it. It keeps, in the state directory, what is at each path the package staged
/// in `staged` places and at the flags file, before the installer replaces it (see
/// [`kept`](super::kept)); a path it cannot keep (a directory, one it cannot read) ends the step
/// `keep_failed`, before anything is replaced. It begins a new update, so a record it finds is of
/// an earlier one whose `cleanup` never ran, and is deleted: left, its marks would make this
/// update's `cutover` and `verify` succeed without restarting anything.
///
/// A copy it finds is of an earlier update that neither verified nor rolled back (`verify` and
/// `rollback` remove theirs), so the files in place may be that update's: it is kept as it is,
/// and this update puts back the same files. Only a copy whose record says that `verify`
/// succeeded is replaced.
#[must_use]
pub fn prepare(env: &Env<'_>, params: &Params, staged: &Path) -> Done {
    prepare_run(env, params, staged).unwrap_or_else(|done| done)
}

fn prepare_run(env: &Env<'_>, params: &Params, staged: &Path) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let unfinished = if loaded.is_some_and(|record| record.verified) {
        false
    } else {
        kept::exists(&params.state_dir).map_err(keep_failure)?
    };
    let done = if unfinished {
        success_with(
            "kept as it is the copy of the files from before an update that did not finish",
        )
    } else {
        let paths = kept::placed(env.root, staged, &params.flags_file).map_err(keep_failure)?;
        kept::keep(&params.state_dir, &paths).map_err(keep_failure)?;
        success()
    };
    held.delete().map_err(state_failure)?;
    Ok(done)
}

/// `cutover`: checks the flags file now in place, notes the host's session, restarts the unit.
///
/// A flags file the agent could not use ends the step `bad_config`, reversible, before anything
/// is restarted: hostkeeper then puts the previous files back. The session the server shows is
/// recorded before the stop, once; a rerun after the restart does nothing. A server that does not
/// know the host, or shows it in no session, is recorded as no session. A server that cannot be
/// reached or refuses the question defers the step (`lease_unreachable`, retried after
/// [`CUTOVER_RETRY_SECS`]) before anything is restarted: without the session, `verify` could
/// take the old agent's for the new one's.
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
        record.session_started_ms = match lease.host() {
            Ok(host) => host.session_started_ms,
            Err(LeaseError::UnknownHost) => None,
            Err(why) => {
                let mut done = lease_failure(why);
                done.outcome = Outcome::Deferred;
                done.report.retry_after_secs = Some(CUTOVER_RETRY_SECS);
                return Err(done);
            }
        };
        record.session_read = true;
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
        return Ok(drop_kept(params));
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
    Ok(drop_kept(params))
}

/// The end of a `verify` that succeeded: the kept copy is removed, as nothing rolls this update
/// back now. A copy that cannot be removed does not fail the step; `cleanup` removes it.
fn drop_kept(params: &Params) -> Done {
    match kept::discard(&params.state_dir) {
        Ok(()) => success(),
        Err(why) => success_with(format!("the kept copy is left for cleanup: {why}")),
    }
}

/// `rollback`: first the files `prepare` kept are put back, each path that holds other than
/// what was there before the update written back (an update's previous version is already back,
/// so nothing is written; a first install's files are replaced by the ones it replaced); a path
/// that held nothing is left to the installer. A copy that cannot be put back fails the step
/// (`restore_failed`, class environment) before the unit is touched, the copy and the record left
/// for a rerun. Then the unit is restarted onto the files, and the agent's hello is waited for
/// when the join token file (`token_file`) exists. A missing hello never fails the rollback (a
/// host must not be wedged by it): it is told in the detail.
///
/// A `cutover` that never began the restart (a flags file refused, a step that never ran) left
/// the unit on the config it has: the files are put back, and the unit is not touched. The
/// record is deleted at the end, so a rerun restarts nothing, and so is the copy, except on a
/// first install whose copy holds a file or link: the updater removes the package after this
/// step, and its removal deletes every file the package placed, the one just put back among
/// them, so the copy stays in the state directory, where the next `prepare` keeps it.
#[must_use]
pub fn rollback(env: &Env<'_>, params: &Params) -> Done {
    rollback_run(env, params).unwrap_or_else(|done| done)
}

/// The end of a `rollback`: the copy removed (or, on a first install, kept and said so) and the
/// record deleted. What the detail says of the copy. A copy that cannot be removed does not fail
/// the step, whose work is done: it is told, and it holds the files now in place, so the next
/// `prepare`, which keeps it, keeps what it would have read.
fn finish_rollback(
    held: &Held,
    params: &Params,
    put: Option<&kept::PutBack>,
) -> Result<String, Done> {
    let told = if params.previous_version.is_none() && put.is_some_and(kept::PutBack::holds_any) {
        format!(
            "; the copy stays in {}, as removing the package deletes the files it placed",
            kept::dir_of(&params.state_dir).display()
        )
    } else {
        match kept::discard(&params.state_dir) {
            Ok(()) => String::new(),
            Err(why) => format!("; the kept copy could not be removed: {why}"),
        }
    };
    held.delete().map_err(state_failure)?;
    Ok(told)
}

fn rollback_run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    let put = kept::put_back(&params.state_dir)
        .map_err(|why| environment(Reason::RestoreFailed, why.to_string()))?;
    let restored = put.as_ref().map(kept::PutBack::told).unwrap_or_default();
    if !loaded.is_some_and(|record| record.restart_begun) {
        let stays = finish_rollback(&held, params, put.as_ref())?;
        return Ok(success_with(format!(
            "{restored}cutover restarted nothing: the unit is left as it is{stays}"
        )));
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
    let stays = finish_rollback(&held, params, put.as_ref())?;
    Ok(success_with(format!(
        "{restored}the unit was restarted; {hello}{stays}"
    )))
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

/// `cleanup`: deletes the kept copy, if `verify` left one, and the record, once `verify` has
/// succeeded. A `cleanup` that finds no record has finished before.
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
    kept::discard(&params.state_dir).map_err(keep_failure)?;
    held.delete().map_err(state_failure)?;
    Ok(success())
}
