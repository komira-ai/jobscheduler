//! `drain` (hostkeeper's maintenance-updates design, 5.2 to 5.4): take the host out of service
//! and return once it has no work. Nothing is stopped or signalled: the work in flight ends on
//! its own.
//!
//! In this order:
//! 1. the hold file is written in the agent's state root (see [`crate::lifecycle::hold`]),
//!    unless one is there: from then on the agent starts no work, cordons the host itself after
//!    each hello and renews the cordon, so the host stays out of service when this step is
//!    killed, the host reboots or the run halts. The file notes whether a cordon was in force
//!    before (an operator's, which `resume` then leaves) and the host facts, which `resume`
//!    compares. A hold that a `resume` began to end and did not finish is written again with
//!    its note;
//! 2. the host is cordoned through the lease server's own-host route, with the host's own
//!    credential and no holder, which extends the cordon in force whoever placed it, and the
//!    cordon is asked again once half of `cordon_lease_secs` has passed. While the server does
//!    not know the host (its hello has not reached the server, `unknown-host`) the step waits,
//!    and cordons as soon as the server knows it;
//! 3. the host is read (`GET /v1/hosts/{id}`) until it is idle by the test `js-agent cordon`
//!    uses: no lease, not releasing, no mark. Then the step succeeds.
//!
//! The wait has no bound unless the params name a deadline (`updater.drain_deadline_unix`).
//! An idle host succeeds whatever the deadline. Once a deadline has passed with the host not
//! idle (or not yet known to the server), the step puts the host back as `resume` does, and
//! ends deferred (75, `drain_deadline`): no work was stopped and nothing changed. A server
//! that refuses or cannot be reached fails the step (`lease_unreachable`); the hold stays, and
//! the agent keeps the host cordoned.
//!
//! Run twice, the second run finds the hold and the cordon in force and changes neither: a
//! cordon asked again at the same time keeps its expiry.

use std::time::Duration;

use crate::cordon::idle;
use crate::update_hook::{
    Class, Done, DrainParams, HoldState, LeaseError, Outcome, POLL_SECS, Reason, Report,
};

use super::held::Note;
use super::resume::put_back;
use super::{MaintainEnv, hold_failure, lease_failure, success};

/// Runs `drain`.
pub fn drain(env: &MaintainEnv<'_>, params: &DrainParams) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &MaintainEnv<'_>, params: &DrainParams) -> Result<Done, Done> {
    let note = hold(env)?;
    let half_lease_ms = params.cordon_lease_secs.saturating_mul(500);
    // When the cordon was last asked for and placed; `None` until the server knows the host.
    let mut cordoned_ms: Option<u64> = None;
    loop {
        let now = env.clock.now_ms();
        if cordoned_ms.is_none_or(|at| now.saturating_sub(at) >= half_lease_ms) {
            cordoned_ms = cordon(env, params)?.then_some(now);
        }
        if cordoned_ms.is_some() {
            match env.lease.host() {
                Ok(host) if idle(&host) => return Ok(success()),
                Ok(_) => {}
                // The server forgot the host after the cordon (it restarted): cordon again.
                Err(LeaseError::UnknownHost) => cordoned_ms = None,
                Err(why) => return Err(lease_failure(why)),
            }
        }
        if passed(env, params) {
            return defer(env, params, &note);
        }
        env.clock.pause(Duration::from_secs(POLL_SECS));
    }
}

/// Writes the hold file unless one is there, and returns its note.
fn hold(env: &MaintainEnv<'_>) -> Result<Note, Done> {
    let text = match env.holds.state().map_err(hold_failure)? {
        HoldState::Held(text) => return Ok(Note::parse(&text)),
        HoldState::Releasing(text) => text,
        HoldState::Free => Note {
            cordoned_before: cordoned(env)?,
            facts: env.facts.read().ok(),
        }
        .text(),
    };
    env.gate.point();
    env.holds.hold(&text).map_err(hold_failure)?;
    Ok(Note::parse(&text))
}

/// Whether a cordon is in force on the host before the drain places one: an operator's, or
/// another holder's. A host the server does not know holds none.
fn cordoned(env: &MaintainEnv<'_>) -> Result<bool, Done> {
    match env.lease.host() {
        Ok(host) => Ok(host.cordon.is_some()),
        Err(LeaseError::UnknownHost) => Ok(false),
        Err(why) => Err(lease_failure(why)),
    }
}

/// Asks for the cordon: `true` when it is in force, `false` while the server does not know
/// the host.
fn cordon(env: &MaintainEnv<'_>, params: &DrainParams) -> Result<bool, Done> {
    env.gate.point();
    match env.lease.cordon(params.cordon_lease_secs) {
        Ok(_) => Ok(true),
        Err(LeaseError::UnknownHost) => Ok(false),
        Err(why) => Err(lease_failure(why)),
    }
}

/// Whether the deadline, if there is one, has passed.
fn passed(env: &MaintainEnv<'_>, params: &DrainParams) -> bool {
    params
        .drain_deadline_unix
        .is_some_and(|deadline| env.clock.now_ms() / 1000 >= deadline)
}

/// The deadline passed with the host not idle: put it back, and defer. Nothing was stopped.
fn defer(env: &MaintainEnv<'_>, params: &DrainParams, note: &Note) -> Result<Done, Done> {
    put_back(env, note, true)?;
    Ok(Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(Reason::DrainDeadline),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(
                "the drain deadline passed with work on the host: the host is back in service \
                 and no work was stopped"
                    .to_owned(),
            ),
            ..Report::default()
        },
    })
}
