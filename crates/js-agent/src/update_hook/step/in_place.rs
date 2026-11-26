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
//!
//! A unit can read `active` and have no process to signal: under `Restart=always` a `Type=simple`
//! unit whose agent dies at its start shows `active` for a moment on each try. A signal the unit
//! manager refuses is therefore judged by the unit's state read after it: still active, the
//! refusal is the failure; not active, no agent was there to ask, and the unit is brought up as
//! above, its start judged as any start is. The agent it runs (on a cutover the staged release's,
//! the link being moved already; on a rollback the previous one's) takes the request at its start.

use crate::update_hook::{Done, Held, Params, Reason, Record, SYSTEMCTL};

use super::Env;
use super::cutover::{Life, bring_up, environment, life, pause, show};

/// What [`signal`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signalled {
    /// The unit manager delivered the signal to the agent.
    Sent,
    /// The unit manager refused the signal and the unit was not active after: no agent was
    /// there to take the request.
    NoAgent,
}

/// Signals the main process of `unit` with SIGHUP, the request that the agent hands its runs
/// over in place. A signal the unit manager refuses while the unit is still active reached no
/// agent, and the step fails as an environment matter with the request left on disk: the marks
/// of the act are written already, so the request is what tells a rerun that the agent has not
/// taken it, and the rerun signals again. (A stop that reaches the agent before a rerun finds the
/// request too, and hands the runs over instead of draining, as the update meant.) A refusal
/// after which the unit is not active is [`Signalled::NoAgent`]: the caller's wait brings the
/// unit up, and the agent it runs takes the request at its start.
pub(super) fn signal(env: &Env<'_>, unit: &str) -> Result<Signalled, Done> {
    env.ports.point();
    let args = ["kill", "--kill-whom=main", "--signal=SIGHUP", "--", unit];
    let Err(why) = env.runner.run(SYSTEMCTL, &args) else {
        return Ok(Signalled::Sent);
    };
    let state = show(env, unit, Reason::StopFailed)?;
    if matches!(life(&state), Life::Up) {
        return Err(environment(
            Reason::StopFailed,
            format!("cannot signal {unit} to hand its runs over in place: {why}"),
        ));
    }
    Ok(Signalled::NoAgent)
}

/// The signal, then the wait until the agent has taken the request (see [`wait_taken`]). After
/// a signal that found no agent ([`Signalled::NoAgent`]) the wait brings the unit up, and the
/// detail of a start that fails says that the unit was not active when it was signalled.
pub(super) fn signal_and_wait(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    bound_ms: u64,
    suffix: &str,
) -> Result<(), Done> {
    let unit = record.update.unit.clone();
    match signal(env, &unit)? {
        Signalled::Sent => wait_taken(env, held, record, params, bound_ms, suffix),
        Signalled::NoAgent => {
            let suffix = format!(
                "; {unit} was not active when it was signalled to hand its runs over in place (no agent was there to take the signal), so the unit was brought up instead{suffix}"
            );
            wait_taken(env, held, record, params, bound_ms, &suffix)
        }
    }
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
