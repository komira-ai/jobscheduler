//! `resume` (hostkeeper's maintenance-updates design, 5.2 and 5.4): put the host back in
//! service after `drain`.
//!
//! In this order:
//! 1. the hold file is read. When there is none, and none that a step putting the host back
//!    renamed, the host was never drained (or was put back already): the step succeeds and
//!    changes nothing, and lifts no cordon, not even one in force;
//! 2. the host facts are read again and compared with the ones the drain noted. The agent reads
//!    them only when it starts, and says hello with them, so when they differ (a new major
//!    version of macOS, another GPU) and the host is idle, the agent's unit is stopped, waited
//!    for (at most [`RESTART_STOP_SECS`]) and started, and the step waits for the hello of the
//!    agent's new session (at most [`RESTART_HELLO_SECS`]): the lease server then places work
//!    by the new facts once the cordon is lifted. Equal facts restart nothing. A host that is
//!    not idle is not restarted, so no work is cut, and facts that cannot be read restart
//!    nothing either; the success then says so in its detail. The hold file is still there
//!    while the agent restarts, so the new agent starts no work and cordons the host after its
//!    hello;
//! 3. the hold is ended ([`put_back`]): the hold file is renamed (the agent then stops renewing
//!    the cordon), the cordon is lifted unless the drain found one in force, and the renamed
//!    file is removed. A renewal the agent sent after it last saw the hold file and that
//!    reaches the server after the lift places the cordon again; nothing renews it, and it
//!    lapses within the agent's lease of an hour (`hold_cordon::HOLD_LEASE_SECS`). A step killed after the rename finds the renamed file and does the rest
//!    without restarting anything.
//!
//! Run twice, the second run finds no hold and changes nothing.

use std::time::Duration;

use crate::cordon::idle;
use crate::update_hook::{Done, DrainParams, HoldState, LeaseError, POLL_SECS, Reason};

use super::cutover::{Life, environment, life};
use super::held::Note;
use super::{MaintainEnv, hold_failure, lease_failure, success};

/// The longest `resume` waits for the agent's unit to stop when it restarts it, in seconds. The
/// host is idle, so the agent has nothing to drain.
pub const RESTART_STOP_SECS: u64 = 300;

/// The longest `resume` waits for the hello of the restarted agent's new session, in seconds.
pub const RESTART_HELLO_SECS: u64 = 300;

/// Runs `resume`.
pub fn resume(env: &MaintainEnv<'_>, params: &DrainParams) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &MaintainEnv<'_>, params: &DrainParams) -> Result<Done, Done> {
    let (note, held, said) = match env.holds.state().map_err(hold_failure)? {
        HoldState::Free => return Ok(success()),
        HoldState::Held(text) => {
            let note = Note::parse(&text);
            let said = refresh(env, params, &note)?;
            (note, true, said)
        }
        HoldState::Releasing(text) => (Note::parse(&text), false, None),
    };
    put_back(env, &note, held)?;
    let mut done = success();
    done.report.detail = said;
    Ok(done)
}

/// Restarts the agent when the host facts changed since the drain and the host is idle, and
/// waits for its new session. What the success should say when nothing was restarted although
/// the facts may have changed.
fn refresh(
    env: &MaintainEnv<'_>,
    params: &DrainParams,
    note: &Note,
) -> Result<Option<String>, Done> {
    match (env.facts.read(), &note.facts) {
        (Ok(now), Some(then)) if now == *then => return Ok(None),
        (Ok(_), Some(_)) => {}
        (Err(why), _) => {
            return Ok(Some(format!(
                "the host facts cannot be read ({why}): the agent was not restarted"
            )));
        }
        (Ok(_), None) => {
            return Ok(Some(
                "the drain could not read the host facts: the agent was not restarted".to_owned(),
            ));
        }
    }
    let host = env.lease.host().map_err(lease_failure)?;
    if !idle(&host) {
        return Ok(Some(
            "the host facts changed and the host is not idle: the agent was not restarted, \
             and says hello with the facts it read when it started"
                .to_owned(),
        ));
    }
    restart(env, params, host.session_started_ms)?;
    Ok(None)
}

/// Stops the agent's unit, waits for it to be down, starts it, and waits for a session other
/// than `session` to say hello.
fn restart(env: &MaintainEnv<'_>, params: &DrainParams, session: Option<u64>) -> Result<(), Done> {
    let unit = params.unit.as_str();
    env.gate.point();
    env.units
        .stop_no_block(unit)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {unit}: {why}")))?;
    let began = env.clock.now_ms();
    loop {
        let state = env.units.show(unit).map_err(|why| {
            environment(
                Reason::StopFailed,
                format!("cannot read the state of {unit}: {why}"),
            )
        })?;
        if matches!(life(&state), Life::Down) {
            break;
        }
        if waited_ms(env, began) >= RESTART_STOP_SECS * 1000 {
            return Err(environment(
                Reason::StopFailed,
                format!(
                    "{unit} is still {} after {RESTART_STOP_SECS} s",
                    state.active
                ),
            ));
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
    env.gate.point();
    env.units
        .start(unit)
        .map_err(|why| environment(Reason::StartFailed, format!("cannot start {unit}: {why}")))?;
    let began = env.clock.now_ms();
    loop {
        // A server that does not answer yet is no hello yet.
        let new = env.lease.host().ok().filter(|host| host.state == "live");
        if new.is_some_and(|host| host.session_started_ms.is_some_and(|s| Some(s) != session)) {
            return Ok(());
        }
        if waited_ms(env, began) >= RESTART_HELLO_SECS * 1000 {
            return Err(environment(
                Reason::NoHello,
                format!("the restarted agent said no hello in {RESTART_HELLO_SECS} s"),
            ));
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
}

/// The milliseconds since `began`, on the step's clock.
fn waited_ms(env: &MaintainEnv<'_>, began: u64) -> u64 {
    env.clock.now_ms().saturating_sub(began)
}

/// Ends the hold: renames the hold file when `held` (a step before this one may have done it),
/// lifts the cordon unless the drain found one in force, and removes the renamed file. A host
/// the server does not know holds no cordon to lift.
pub(super) fn put_back(env: &MaintainEnv<'_>, note: &Note, held: bool) -> Result<(), Done> {
    if held {
        env.gate.point();
        env.holds.release().map_err(hold_failure)?;
    }
    if !note.cordoned_before {
        env.gate.point();
        match env.lease.uncordon() {
            Ok(_) | Err(LeaseError::UnknownHost) => {}
            Err(why) => return Err(lease_failure(why)),
        }
    }
    env.gate.point();
    env.holds.released().map_err(hold_failure)
}
