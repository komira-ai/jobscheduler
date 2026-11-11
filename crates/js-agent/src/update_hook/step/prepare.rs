//! `prepare` (host-updates design, 4.2, 3.6): check that the host can take the update, then
//! cordon it and wait for it to hold no lease. Nothing a client sees changes, and the agent is
//! never stopped or signalled here: a drain bound that passes with a lease still running lifts
//! the cordon this step placed and ends deferred (75); if the lift fails the cordon stays as
//! recorded and the step still ends deferred.
//!
//! In this order, each before anything is written unless it says so:
//! 1. the lock, the record (if there is one) and the params agree; a record that breaks a rule
//!    is `state_invalid` (1), except a record of another target that never took the host, which
//!    is superseded (see [`supersede`](super::supersede)): moved aside just before the new record
//!    is first written (5), and named in the detail of the step's last line. The state
//!    directory is made, mode 0700, when it is not there: hostkeeper makes none, so a first
//!    install finds none;
//! 2. a taint or an `owed-*` record halts (76);
//! 3. the staged release's agent (`bin/js-agent`, or `tree/usr/bin/js-agent` in a native package
//!    release, see [`agent_in`]) `--version` names the target version (1), and the staged release
//!    carries the unit the hook manages (`bad_artifact` otherwise);
//! 4. a first install needs a join token: the token file must be a regular file of the hook's
//!    user, mode 0600 or 0400, unless the state root already holds a host credential;
//!    otherwise the step defers (75, `join_token_missing`) and the operator places the file;
//! 5. the record is written, with the cordon marked as ours *before* it is placed unless the
//!    host is cordoned already (an operator's cordon is never ours), so that a step killed
//!    after the placing finds it marked; the reply of the lease server corrects the mark;
//! 6. the wait: poll the host, renewing the cordon we placed after each pause and never
//!    asking for more than the cap allows.
//!
//! A first install has no agent to drain and no host the lease server knows: it ends after 5.
//!
//! An update compares the unit the staged release carries with the one the previous release
//! carried (the installed one, see [`plan`](crate::update_hook::plan)). A difference is recorded
//! as the record's restart plan and named in the detail of the success; `cutover` acts on it.

use std::time::Duration;

use crate::update_hook::{
    Class, Cordon, Done, Held, HostReading, Outcome, POLL_SECS, Params, PlanError, Reason, Record,
    Report, UnitRestart, UpdateInfo, agent_in, plan,
};

use super::supersede::{judge, set_aside};
use super::{Env, check, failure, lease_failure, open_unchecked, save, success};

/// The agent's name, the first word of its `--version`.
const AGENT_NAME: &str = "js-agent";

/// Runs `prepare` of the release in `params.staged_release`.
pub fn prepare(env: &Env<'_>, params: &Params) -> Done {
    let mut superseded = None;
    let done = run(env, params, &mut superseded).unwrap_or_else(|done| done);
    note_superseded(done, superseded)
}

fn run(env: &Env<'_>, params: &Params, superseded: &mut Option<String>) -> Result<Done, Done> {
    let (held, loaded) = open_unchecked(env, params)?;
    let stale = match &loaded {
        Some(record) => judge(env, params, record)?,
        None => false,
    };
    // Before anything else is looked at or written: no version can repair a taint.
    refuse_host(env)?;
    let (fresh, aside, mut record) = match loaded {
        Some(record) if !stale => (false, None, record),
        aside => {
            let record = new_record(params);
            check(params, &record)?;
            (true, aside, record)
        }
    };
    check_agent(env, params)?;
    let restart = staged_unit(env, params)?;
    if params.previous_version.is_none() {
        join_token(env, params)?;
    }
    // The record of another target goes aside only now, when the new one is about to be
    // written: a refusal before this point leaves the host as it found it.
    if let Some(aside) = &aside {
        *superseded = Some(set_aside(env, &held, aside)?);
    }
    if params.previous_version.is_none() {
        if fresh {
            save(env, &held, &record)?;
        }
        return Ok(success());
    }
    if fresh {
        record.unit_restart = restart;
    }
    place(env, &held, &mut record, params, fresh)?;
    let done = wait(env, &held, &mut record, params)?;
    Ok(note_restart(done, &record))
}

/// The change from the installed unit to the staged one, and the staged unit must be there.
fn staged_unit(env: &Env<'_>, params: &Params) -> Result<Option<UnitRestart>, Done> {
    plan(
        env.manager,
        &params.settings.unit,
        &params.staged_release,
        params.previous_release.as_deref(),
    )
    .map_err(|why| match why {
        PlanError::Staged { .. } => bad_artifact(why),
        PlanError::Installed { .. } => failure(Reason::StateInvalid, why.to_string()),
    })
}

/// Any ending of a step that set a record of another target aside says so first.
fn note_superseded(mut done: Done, superseded: Option<String>) -> Done {
    if let Some(note) = superseded {
        done.report.detail = Some(match done.report.detail.take() {
            Some(detail) => format!("{note}; {detail}"),
            None => note,
        });
    }
    done
}

/// A success that says the unit changes, and which keys.
fn note_restart(mut done: Done, record: &Record) -> Done {
    if let (Outcome::Success, Some(restart)) = (done.outcome, &record.unit_restart) {
        done.report.detail = Some(format!(
            "the unit changes ({}): cutover starts the agent on the new unit",
            restart.changed.join(", ")
        ));
    }
    done
}

/// A first install needs the host's join token, which the operator (or the machine image)
/// places: a regular file of the hook's user, mode 0600 or 0400. A host credential in the state
/// root does without. Otherwise the step defers, with nothing changed, until the file is right.
fn join_token(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    if env.ports.state.has_credential() {
        return Ok(());
    }
    let file = &params.token_file;
    let problem = match (env.token)(file) {
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => "is not there".to_owned(),
        Err(why) => format!("cannot be read: {why}"),
        Ok(facts) if !facts.regular => "is not a regular file".to_owned(),
        Ok(facts) if !matches!(facts.mode, 0o600 | 0o400) => {
            format!("must be mode 0600 or 0400, not {:04o}", facts.mode)
        }
        Ok(facts) if facts.uid != env.euid => {
            format!("must be owned by uid {}, not {}", env.euid, facts.uid)
        }
        Ok(_) => return Ok(()),
    };
    Err(Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(Reason::JoinTokenMissing),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(format!(
                "the join token file {} {problem}; place it, root-owned and mode 0600, to install the agent",
                file.display()
            )),
            ..Report::default()
        },
    })
}

/// The record of an update that is just beginning: nothing done, no cordon. The digest is the
/// updater's; the staged tree is never hashed again.
fn new_record(params: &Params) -> Record {
    Record::new(UpdateInfo {
        target: params.target_version.clone(),
        previous: params.previous_version.clone(),
        staged: params.staged_release.clone(),
        previous_target: params.previous_release.clone(),
        unit: params.settings.unit.clone(),
        artifact_digest: params.artifact_digest.clone(),
    })
}

fn bad_artifact(why: impl ToString) -> Done {
    failure(Reason::BadArtifact, why.to_string())
}

/// A taint or an owed check halts: no version can repair it, and the old agent would refuse
/// its next start.
pub(super) fn refuse_host(env: &Env<'_>) -> Result<(), Done> {
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
/// required one (`crun` for the Linux unit, `native` for the Mac one) is among them.
fn check_agent(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    let binary = agent_in(&params.staged_release);
    let printed = env
        .runner
        .run(&binary.to_string_lossy(), &["--version"])
        .map_err(bad_artifact)?;
    let mut words = printed.lines().next().unwrap_or("").split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(AGENT_NAME), Some(version), None) if version == params.target_version => {}
        (Some(AGENT_NAME), Some(version), None) => {
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
                "{} --version printed {:?}, not `{AGENT_NAME} <version>`",
                binary.display(),
                printed.lines().next().unwrap_or("")
            )));
        }
    }
    let runtimes = printed
        .lines()
        .find_map(|line| line.strip_prefix("runtimes:"));
    let required = params.settings.runtime.as_str();
    if runtimes.is_some_and(|list| !list.split_whitespace().any(|name| name == required)) {
        return Err(failure(
            Reason::VersionMismatch,
            format!("the staged agent lacks the {required} runtime"),
        ));
    }
    Ok(())
}

/// The seconds a cordon may be asked for at `now`: the lease, and no more than is left of the
/// cap counted from the first placement (from now, for a cordon not placed yet).
pub(super) fn ask_secs(cordon: &Cordon, params: &Params, now_ms: u64) -> u64 {
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
pub(super) fn place(
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
        // The cap bounds the host's total time out of service across retries, counted from the
        // first placement. A prepare resumed after `cordon_max_secs` has passed since then
        // fails here on every retry, until rollback deletes the record: deliberate, not a
        // bug to retry past.
        // The host's own history, not the release: an environment matter.
        let mut done = failure(
            Reason::CordonCapReached,
            "the cordon's cap leaves no time to ask a cordon for",
        );
        done.report.class = Class::Environment;
        return Err(done);
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
pub(super) fn renew(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
) -> Result<(), Done> {
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
    // A cordon that cannot be lifted is left as recorded (placed, with its expiry): the next
    // prepare, cleanup or rollback handles it, and it lapses on its own within one lease. The
    // deferral stands; the failure goes in the detail.
    let mut left = None;
    if record.cordon.placed {
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
        "the host still held {} lease(s) after {} s",
        host.leases, params.drain_wait_secs
    );
    if let Some(error) = left {
        detail.push_str(&format!(
            "; the cordon could not be lifted and is left to lapse: {error}"
        ));
    }
    Ok(Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(Reason::DrainBound),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(detail),
            ..Report::default()
        },
    })
}
