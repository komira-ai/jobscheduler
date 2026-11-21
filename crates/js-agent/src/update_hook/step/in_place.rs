//! What `cutover` and `rollback` share when the update hands the runs over in place (the
//! record's `in_place`, see `prepare`): the unit is not stopped and not started. The hook
//! writes the handover file, then signals the unit's main process with SIGHUP; an agent that
//! lists `in-place` on its `stops:` line hands the runs over and runs its own program again as
//! the same process, so the unit manager places no process and the unit stays active. The
//! program that runs again, the new release's on a cutover and the previous one's on a
//! rollback, adopts the runs and removes the handover file at its start.
//!
//! So the request is taken when the file is gone. The wait here ends then if the unit is still
//! active. A unit that is not active, because the agent ended instead of running again or the
//! new program died, is brought up as a start is (see `bring_up`), which judges the start.

use crate::update_hook::{Done, Held, Params, Reason, Record, SYSTEMCTL};

use super::Env;
use super::cutover::{Life, bring_up, environment, life, pause, show, withdraw_handover};

/// Signals the main process of `unit` with SIGHUP, the request that the agent hands its runs
/// over in place. A signal the unit manager refuses reached no agent: the request written for
/// it is withdrawn and the step fails as an environment matter.
pub(super) fn signal(env: &Env<'_>, record: &Record, unit: &str) -> Result<(), Done> {
    env.ports.point();
    let args = ["kill", "--kill-whom=main", "--signal=SIGHUP", "--", unit];
    env.runner.run(SYSTEMCTL, &args).map(|_| ()).map_err(|why| {
        withdraw_handover(env, record);
        environment(
            Reason::StopFailed,
            format!("cannot signal {unit} to hand its runs over in place: {why}"),
        )
    })
}

/// Whether the handover file is still there: the agent has not taken the request.
pub(super) fn pending(env: &Env<'_>) -> Result<bool, Done> {
    env.ports.state.handover_pending().map_err(|why| {
        environment(
            Reason::StopFailed,
            format!("cannot look at the handover request: {why}"),
        )
    })
}

/// Waits until the agent has taken the request (the handover file is gone) while the unit is
/// active, for at most `bound_ms`. A unit that is not active is brought up, and `suffix` is
/// added to the detail of a start that fails. A bound that passes with the request still there
/// fails the step as an environment matter and leaves the request for the agent.
pub(super) fn wait_taken(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    bound_ms: u64,
    suffix: &str,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    let began = env.ports.clock.now_ms();
    loop {
        let state = show(env, &unit, Reason::StopFailed)?;
        if !matches!(life(&state), Life::Up) {
            return bring_up(env, held, record, params, suffix);
        }
        if !pending(env)? {
            return Ok(());
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(environment(
                Reason::StopFailed,
                format!(
                    "the agent of {unit} has not handed over in place in {} s: the request is left for it",
                    bound_ms / 1000
                ),
            ));
        }
        pause(env, held, record, params)?;
    }
}
