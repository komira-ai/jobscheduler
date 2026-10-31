//! `prepare` (host-updates design, 4.2, 3.6): check that the host can take the update, then
//! cordon it and wait for it to hold no lease. Nothing a client sees changes, and the agent is
//! never stopped or signalled here: a drain bound that passes with a lease still running lifts
//! the cordon this step placed and ends deferred (75).
//!
//! In this order, each before anything is written unless it says so:
//! 1. the lock, the record (if there is one) and the params agree; a record that breaks a rule
//!    is `state_invalid` (1);
//! 2. a taint or an `owed-*` record halts (76);
//! 3. `<staged>/js-agent --version` names the target version (1);
//! 4. the record is written, with the cordon marked as ours *before* it is placed unless the
//!    host is cordoned already (an operator's cordon is never ours), so that a step killed
//!    after the placing finds it marked; the reply of the lease server corrects the mark;
//! 5. the wait: poll the host, renewing the cordon we placed after each pause and never
//!    asking for more than the declared cap allows.
//!
//! A first install has no agent to drain and no host the lease server knows: it ends after 3.
//!
//! TODO: compare the staged unit's flags and cgroup layout (`supervisor`, `actions`) with the
//! running unit's (design 4.2, "new work" 6). The unit manager port reads `ActiveState` and
//! `SubState` only, and the staged release carries no unit file the hook is told of.

use std::time::Duration;

use crate::update_hook::{
    Class, Cordon, Done, Held, HostReading, Outcome, POLL_SECS, Params, Reason, Record, Report,
    UpdateInfo,
};

use super::{Env, check, failure, lease_failure, open_state, save, success};

/// The agent's file in a release directory.
const AGENT_BINARY: &str = "js-agent";

/// The runtime a staged agent that names its runtimes must have: the unit is the `crun` one.
const REQUIRED_RUNTIME: &str = "crun";

/// Runs `prepare` of the release in `params.staged_release`.
pub fn prepare(env: &Env<'_>, params: &Params) -> Done {
    run(env, params).unwrap_or_else(|done| done)
}

fn run(env: &Env<'_>, params: &Params) -> Result<Done, Done> {
    let (held, loaded) = open_state(env, params)?;
    // Before anything else is looked at or written: no version can repair a taint.
    refuse_host(env)?;
    let fresh = loaded.is_none();
    let mut record = match loaded {
        Some(record) => record,
        None => {
            let record = new_record(env, params);
            check(env, params, &record)?;
            record
        }
    };
    check_agent(env, params)?;
    if params.previous_version.is_none() {
        if fresh {
            save(env, &held, &record)?;
        }
        return Ok(success());
    }
    place(env, &held, &mut record, params, fresh)?;
    wait(env, &held, &mut record, params)
}

/// The record of an update that is just beginning: nothing done, no cordon. The digest is the
/// updater's; the staged tree is never hashed again.
fn new_record(env: &Env<'_>, params: &Params) -> Record {
    Record::new(UpdateInfo {
        target: params.target_version.clone(),
        previous: params.previous_version.clone(),
        staged: params.staged_release.clone(),
        previous_target: params.previous_release.clone(),
        // The declaration names exactly one unit (it is refused at the parse otherwise).
        unit: env.declaration.units.first().cloned().unwrap_or_default(),
        artifact_digest: params.artifact_digest.clone(),
    })
}

fn bad_artifact(why: impl ToString) -> Done {
    failure(Reason::BadArtifact, why.to_string())
}

/// A taint or an owed check halts: no version can repair it, and the old agent would refuse
/// its next start.
fn refuse_host(env: &Env<'_>) -> Result<(), Done> {
    if let Some(taint) = env.ports.state.taint() {
        return Err(Done {
            outcome: Outcome::Halt,
            report: Report::failure(
                Reason::Tainted,
                format!("the agent is tainted: {}", taint.reason),
            ),
        });
    }
    let owed = env
        .ports
        .state
        .owed()
        .map_err(|why| failure(Reason::StateInvalid, why.to_string()))?;
    if owed.is_empty() {
        return Ok(());
    }
    Err(Done {
        outcome: Outcome::Halt,
        report: Report::failure(
            Reason::OwedCheck,
            format!("checks are owed for {}", owed.join(", ")),
        ),
    })
}

/// The staged agent runs and is the version the order names; if it lists its runtimes, the
/// unit's is among them.
fn check_agent(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    let binary = params.staged_release.join(AGENT_BINARY);
    let printed = env
        .runner
        .run(&binary.to_string_lossy(), &["--version"])
        .map_err(bad_artifact)?;
    let mut words = printed.lines().next().unwrap_or("").split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(AGENT_BINARY), Some(version), None) if version == params.target_version => {}
        (Some(AGENT_BINARY), Some(version), None) => {
            return Err(failure(
                Reason::VersionMismatch,
                format!(
                    "the staged agent is {version}, not {}",
                    params.target_version
                ),
            ));
        }
        _ => {
            return Err(bad_artifact(format!(
                "{} --version printed {:?}, not `{AGENT_BINARY} <version>`",
                binary.display(),
                printed.lines().next().unwrap_or("")
            )));
        }
    }
    let runtimes = printed
        .lines()
        .find_map(|line| line.strip_prefix("runtimes:"));
    if runtimes.is_some_and(|list| !list.split_whitespace().any(|name| name == REQUIRED_RUNTIME)) {
        return Err(failure(
            Reason::VersionMismatch,
            format!("the staged agent lacks the {REQUIRED_RUNTIME} runtime"),
        ));
    }
    Ok(())
}

/// The seconds a cordon may be asked for at `now`: the lease, and no more than is left of the
/// cap counted from the first placement (from now, for a cordon not placed yet).
fn ask_secs(cordon: &Cordon, params: &Params, now_ms: u64) -> u64 {
    let first = if cordon.placed {
        cordon.first_ms
    } else {
        now_ms
    };
    let end = first.saturating_add(params.cordon_max_secs.saturating_mul(1000));
    params
        .cordon_lease_secs
        .min(end.saturating_sub(now_ms) / 1000)
}

/// When the reply says the cordon lapses.
fn expiry_of(host: &HostReading) -> u64 {
    host.cordon.map_or(0, |cordon| cordon.expires_at_ms)
}

/// Writes the record, places the cordon and records whose it is.
///
/// A cordon in force when the step looks is an operator's and is not ours. Otherwise the
/// record is written with the cordon marked as ours and *provisional* before it is placed, so
/// that a step killed after the placing finds it marked. A reply that says the host was not
/// cordoned confirms the mark. A reply that says it was cordoned is only ours if the mark is
/// confirmed and has not lapsed: after a kill before the confirmation, or after the expiry,
/// the cordon found may be anyone's, and it is left alone (ours, if it was, lapses within one
/// lease).
fn place(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    fresh: bool,
) -> Result<(), Done> {
    let looked = env.ports.lease.host().map_err(lease_failure)?;
    let now = env.ports.clock.now_ms();
    let ours_before = record.cordon.placed;
    let ask = ask_secs(&record.cordon, params, now);
    if ask == 0 {
        return Err(failure(
            Reason::CordonCapReached,
            "the cordon's cap leaves no time to ask a cordon for",
        ));
    }
    let mark = !ours_before && looked.cordon.is_none();
    if mark {
        record.cordon = Cordon {
            placed: true,
            provisional: true,
            first_ms: now,
            expires_ms: now.saturating_add(ask.saturating_mul(1000)),
        };
    }
    if fresh || mark {
        save(env, held, record)?;
    }
    let unsure = record.cordon.provisional || now >= record.cordon.expires_ms;
    env.ports.point();
    let reply = env.ports.lease.cordon(ask).map_err(lease_failure)?;
    let ours = record.cordon.placed && !(reply.already_cordoned && unsure);
    let cordon = if ours {
        Cordon {
            provisional: false,
            expires_ms: record.cordon.expires_ms.max(expiry_of(&reply.host)),
            ..record.cordon
        }
    } else {
        Cordon::default()
    };
    if cordon != record.cordon {
        record.cordon = cordon;
        save(env, held, record)?;
    }
    Ok(())
}

/// Extends the cordon we placed when less than half a lease is left of it, if the cap leaves
/// anything to ask for. The record follows when the expiry moves.
fn renew(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<(), Done> {
    let now = env.ports.clock.now_ms();
    let half_lease_ms = params.cordon_lease_secs.saturating_mul(500);
    if record.cordon.expires_ms.saturating_sub(now) >= half_lease_ms {
        return Ok(());
    }
    let ask = ask_secs(&record.cordon, params, now);
    if ask == 0 {
        return Ok(());
    }
    env.ports.point();
    let reply = env.ports.lease.cordon(ask).map_err(lease_failure)?;
    let expires_ms = record.cordon.expires_ms.max(expiry_of(&reply.host));
    if expires_ms != record.cordon.expires_ms {
        record.cordon.expires_ms = expires_ms;
        save(env, held, record)?;
    }
    Ok(())
}

/// Waits for the host to hold no lease, up to the drain bound.
fn wait(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<Done, Done> {
    let started = env.ports.clock.now_ms();
    loop {
        let host = env.ports.lease.host().map_err(lease_failure)?;
        if host.leases == 0 && !host.releasing {
            return Ok(success());
        }
        let waited = env.ports.clock.now_ms().saturating_sub(started);
        if waited >= params.drain_wait_secs.saturating_mul(1000) {
            return defer(env, held, record, params, &host);
        }
        env.ports.clock.pause(Duration::from_secs(POLL_SECS));
        if record.cordon.placed {
            renew(env, held, record, params)?;
        }
    }
}

/// The bound passed with a lease still running: lift the cordon if it is ours, record that
/// there is none, and defer. Nothing was stopped.
fn defer(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    host: &HostReading,
) -> Result<Done, Done> {
    if record.cordon.placed {
        env.ports.point();
        env.ports.lease.uncordon().map_err(lease_failure)?;
        record.cordon = Cordon::default();
        save(env, held, record)?;
    }
    Ok(Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(Reason::DrainBound),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(format!(
                "the host still held {} lease(s) after {} s",
                host.leases, params.drain_wait_secs
            )),
            ..Report::default()
        },
    })
}
