//! `cutover` (host-updates design, 4.2): look once more at the cordon and the leases, then stop
//! the unit and start it again on the staged release. The updater has moved the release link
//! (hostkeeper's `current`) to the staged release before it runs this step; the hook reads the
//! link and never writes it. The stop is the commit: once it is begun every ending says
//! `reversible: false`.
//!
//! In this order:
//! 1. the lock, the record and the params agree; there must be a record, and the release link
//!    must point at the staged release (`link_failed` otherwise, nothing done);
//! 2. unless the stop is begun already: a taint or an `owed-*` record halts (76), and the host
//!    must be cordoned and hold no lease. `prepare` can end with no cordon held (it was killed
//!    between the placing and the confirming), so a cordon of ours that is gone is put back
//!    within the cap, and a cap that is spent or a lease that came in defers (75, class
//!    environment) with nothing stopped (a failed lift of the cordon on the way is no reason to
//!    fail: the cordon stays as recorded, as in `prepare`, for the next prepare, cleanup or
//!    rollback to lift). A cordon that is not ours (an operator's) is neither renewed nor
//!    lifted. A cordon in force under a mark that was never confirmed may be ours or not, and
//!    nothing would renew it through the stop: that defers too, the mark left as it is;
//! 3. the unit is read, so that a read that fails leaves nothing begun; then the record gets
//!    `stop_begun` together with `pre_stop_session_ms`, the session the lease server showed for
//!    the host in the reading of step 2 (one save, so that the stop is never marked without it
//!    and it is never written without the stop; a rerun that finds the stop begun reads no host
//!    and keeps it), and the unit is stopped and waited for (renewing the cordon if it is
//!    ours), for at most the declared bound less the start wait and a margin;
//! 4. if the record holds a restart plan (the unit changed, see `prepare`) on systemd, the wait
//!    for the unit's cgroup to hold no process, so that the new `Slice`, `Delegate` and
//!    `DelegateSubgroup` layout starts from an empty cgroup (`KillMode=process` leaves strays
//!    behind); a cgroup still busy at the same bound ends `unit_not_empty`, class environment.
//!    launchd's `bootout` already empties the job;
//! 5. the record gets `link_moved` (kept under its name: the link was moved by the updater and
//!    this is the mark that the start is next, and for a first install the commit), then
//!    `start_issued`, then the unit is started and waited for. A start that fails is the
//!    release's fault only if the unit ran and died (`failed`, or `auto-restart` after a crash,
//!    at any look); a unit still waiting in `start` is not.
//!
//! Each mark is written before its act, so a run that is killed leaves the marks of every act
//! it may have made. A rerun does not rely on them: it asks the unit manager, and does what is
//! not yet done, so every act is a no-op when its effect is already there. The state of the unit
//! is read with `show` and not `is-active`, which cannot tell a failed run of `systemctl` from an
//! inactive unit. A unit that is up after the stop was begun is the new agent only if the
//! record says the start was issued: the link is on the staged release in every case, so it
//! cannot say which agent runs.
//!
//! A first install has no agent to drain, no host the lease server knows and no unit to stop:
//! it skips steps 2 to 4, writes no `stop_begun`, and is irreversible from `link_moved` on.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::update_hook::{
    CUTOVER_MARGIN_SECS, Class, Cordon, Done, Held, HostReading, Manager, Outcome, POLL_SECS,
    Params, Reason, Record, Report, START_WAIT_SECS, SYSTEMCTL, UnitState,
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
pub(super) fn begun(record: &Record) -> bool {
    if record.update.previous.is_none() {
        record.link_moved
    } else {
        record.stop_begun
    }
}

pub(super) fn irreversible(mut done: Done) -> Done {
    done.report.reversible = false;
    done
}

fn go(env: &Env<'_>, params: &Params, held: &Held, record: &mut Record) -> Result<Done, Done> {
    if !linked(env, &params.release_link, &record.update.staged)? {
        return Err(environment(
            Reason::LinkFailed,
            format!(
                "the release link {} is not on the staged release {}: the updater moves it before cutover",
                params.release_link.display(),
                record.update.staged.display()
            ),
        ));
    }
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
        if !entered_begun {
            // One save: the stop is never marked without the session it is to be judged against
            // (`mark` saves a copy of the record as it stands, with the session in it).
            record.pre_stop_session_ms = session;
            mark(env, held, record, |record| record.stop_begun = true)?;
        }
        let since = env.ports.clock.now_ms();
        stop(env, held, record, params, state, entered_begun)?;
        // A unit started by an earlier run holds the new agent: nothing to wait for.
        if !record.start_issued {
            wait_empty(env, held, record, params, since)?;
        }
    }
    mark_link_moved(env, held, record)?;
    start(env, held, record, params)?;
    Ok(success())
}

/// A failure of something remote or shared: not the release's doing.
pub(super) fn environment(reason: Reason, detail: impl Into<String>) -> Done {
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

/// Whether the cordon is one this update placed and confirmed, and has not lapsed.
pub(super) fn ours(record: &Record, now_ms: u64) -> bool {
    record.cordon.placed && !record.cordon.provisional && now_ms < record.cordon.expires_ms
}

/// Sets a mark in the record, durably, before the act it names.
pub(super) fn mark(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    set: fn(&mut Record),
) -> Result<(), Done> {
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
///
/// A cordon that cannot be lifted is left as recorded (placed, with its expiry), as `prepare`'s
/// deferral leaves it: the next prepare, cleanup or rollback tries to lift it, and it lapses
/// within one lease if nothing does. The deferral stands; the failure goes in the detail.
fn busy(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    host: &HostReading,
) -> Result<Done, Done> {
    let mut left = None;
    if ours(record, env.ports.clock.now_ms()) {
        env.ports.point();
        match env.ports.lease.uncordon() {
            Ok(_) => {
                record.cordon = Cordon::default();
                save(env, held, record)?;
            }
            Err(error) => left = Some(error),
        }
    }
    let mut detail = format!(
        "the host is not idle: {} lease(s), releasing: {}",
        host.leases, host.releasing
    );
    if let Some(error) = left {
        detail.push_str(&format!(
            "; the cordon could not be lifted and is left as recorded; it lapses within one lease if nothing lifts it: {error}"
        ));
    }
    Ok(deferred(params, Reason::DrainBound, detail))
}

/// What the unit manager says of a unit, in the three ways that matter here.
pub(super) enum Life {
    /// `active`.
    Up,
    /// `activating`, `reloading` and the like: on its way up, or waiting to restart.
    Starting,
    /// `deactivating`.
    Stopping,
    /// `inactive` or `failed`.
    Down,
}

pub(super) fn life(state: &UnitState) -> Life {
    match state.active.as_str() {
        "active" => Life::Up,
        "inactive" | "failed" => Life::Down,
        "deactivating" => Life::Stopping,
        _ => Life::Starting,
    }
}

pub(super) fn show(env: &Env<'_>, unit: &str, reason: Reason) -> Result<UnitState, Done> {
    env.ports
        .units
        .show(unit)
        .map_err(|why| environment(reason, format!("cannot read the state of {unit}: {why}")))
}

/// Where a link at `link` that holds `target` points: an absolute target as it is, a relative
/// one from the link's directory, with `.` and `..` resolved by name (nothing is looked up).
pub(super) fn resolved(link: &Path, target: &Path) -> PathBuf {
    let mut path = link.parent().unwrap_or(link).to_path_buf();
    for part in target.components() {
        match part {
            Component::RootDir => path = PathBuf::from("/"),
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir | Component::Prefix(_) => {}
            Component::Normal(name) => path.push(name),
        }
    }
    path
}

/// Whether the link points at the staged release. A target the updater wrote relative to the
/// link's directory (`current -> 1.4.0`) is read from there.
pub(super) fn linked(env: &Env<'_>, link: &Path, staged: &Path) -> Result<bool, Done> {
    let target = env.ports.link.target(link).map_err(|why| {
        environment(
            Reason::LinkFailed,
            format!("cannot read {}: {why}", link.display()),
        )
    })?;
    Ok(target.is_some_and(|target| resolved(link, &target) == staged))
}

/// Waits one poll and renews the cordon if it is ours.
pub(super) fn pause(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
) -> Result<(), Done> {
    env.ports.clock.pause(Duration::from_secs(POLL_SECS));
    if ours(record, env.ports.clock.now_ms()) {
        renew(env, held, record, params)?;
    }
    Ok(())
}

/// Stops the unit and waits for it to be down. A unit that is up after the stop was begun and
/// the start was issued is the new agent, started by an earlier run: it is left alone.
fn stop(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    state: UnitState,
    entered_begun: bool,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    match life(&state) {
        Life::Down => return Ok(()),
        Life::Stopping => {}
        Life::Up | Life::Starting => {
            if entered_begun && record.start_issued {
                return Ok(());
            }
            env.ports.point();
            env.ports.units.stop_no_block(&unit).map_err(|why| {
                environment(Reason::StopFailed, format!("cannot stop {unit}: {why}"))
            })?;
        }
    }
    // The step is killed at the declared bound; ending a little before it says why.
    let bound_ms = stop_bound_ms(params);
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

/// The longest the stop and the wait for the cgroup may take: the step is killed at the
/// declared bound, and ending a little before it says why.
fn stop_bound_ms(params: &Params) -> u64 {
    params
        .settings
        .timeouts
        .cutover
        .saturating_sub(START_WAIT_SECS + CUTOVER_MARGIN_SECS)
        .saturating_mul(1000)
}

/// The processes the unit's cgroup holds: `0` for a unit that accounts for none
/// (`TasksCurrent` is `[not set]` then).
fn tasks_current(env: &Env<'_>, unit: &str) -> Result<u64, Done> {
    let printed = env
        .runner
        .run(
            SYSTEMCTL,
            &["show", "--property=TasksCurrent", "--value", unit],
        )
        .map_err(|why| {
            environment(
                Reason::StopFailed,
                format!("cannot read the tasks of {unit}: {why}"),
            )
        })?;
    match printed.trim() {
        "[not set]" => Ok(0),
        text => text.parse().map_err(|_| {
            environment(
                Reason::StopFailed,
                format!("cannot read the tasks of {unit}: systemctl printed {text:?}"),
            )
        }),
    }
}

/// With a restart plan on systemd, waits until the stopped unit's cgroup holds no process, for
/// at most the stop's bound counted from `since_ms`, so that the unit starts on its new layout
/// from an empty cgroup.
fn wait_empty(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    since_ms: u64,
) -> Result<(), Done> {
    let Some(restart) = record.unit_restart.clone() else {
        return Ok(());
    };
    if env.manager != Manager::Systemd {
        return Ok(());
    }
    let unit = record.update.unit.clone();
    let bound_ms = stop_bound_ms(params);
    loop {
        let tasks = tasks_current(env, &unit)?;
        if tasks == 0 {
            return Ok(());
        }
        if env.ports.clock.now_ms().saturating_sub(since_ms) >= bound_ms {
            return Err(environment(
                Reason::UnitNotEmpty,
                format!(
                    "{unit} is stopped and its cgroup still holds {tasks} process(es) after {} s; the unit changes ({}), and starts only from an empty cgroup",
                    bound_ms / 1000,
                    restart.changed.join(", ")
                ),
            ));
        }
        pause(env, held, record, params)?;
    }
}

/// Marks that the link is on the staged release and the start is next, before the start. On a
/// first install this mark is the commit.
fn mark_link_moved(env: &Env<'_>, held: &Held, record: &mut Record) -> Result<(), Done> {
    if record.link_moved {
        return Ok(());
    }
    mark(env, held, record, |record| record.link_moved = true)
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

/// Marks the start, then starts the unit and waits for it to be active.
fn start(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<(), Done> {
    if !record.start_issued {
        mark(env, held, record, |record| record.start_issued = true)?;
    }
    bring_up(env, held, record, params)
}

/// Starts the unit if it is down and waits for it to be active, for at most the start wait. A
/// start that fails is the release's fault only if the unit ran and died. The mark of the
/// start is the caller's: `cutover`'s and `rollback`'s are not the same.
pub(super) fn bring_up(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
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
