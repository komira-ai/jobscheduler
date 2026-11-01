//! `verify` (host-updates design, 4.2): look, again and again until a bound, for proof that the
//! new agent runs and is serving. It reads and never changes the machine: the cordon, the link
//! and the unit are left as `cutover` left them, for `rollback` or `cleanup`. The one write is
//! the record's `verify_ok`, after the observation it names.
//!
//! The proof, at a look where the unit is `active`:
//! - the lease server shows the host `live` in a session other than the one it showed in the
//!   last reading before the stop (the record's `pre_stop_session_ms`; none recorded, on a first
//!   install or when the server showed no session, means any session is other). The two stamps
//!   are both the server's, and equality is all that is asked of them: the host's clock is never
//!   compared with the server's, so a skew between them cannot make a healthy session read as
//!   old or the old one as new. A server that still shows the old session, or no session yet
//!   (`awaiting`, after a server restart), is no proof; a session of a restarted server is
//!   another session and is proof only with the target version, below;
//! - the hello of that session carries the target version, and the execution mode of the
//!   declared runtime if it names one (`container` for `crun` and `docker`, `sandbox-exec` for
//!   `native`).
//!
//! Before it starts: a record with `start_issued` and the release link on the staged release
//! (`state_invalid` otherwise, nothing read of the lease server). A taint or an `owed-*`
//! record halts (76), at every look. At each look the unit is read with `show`: `failed`,
//! `inactive`, `deactivating` or a restart pending (`auto-restart`) ends the step at once as a
//! `start_failed` of the release: the binary ran and died.
//!
//! The wait ends at the declared `verify` timeout less a margin. Then the failure is the
//! release's (`no_hello`: no new session said hello; `version_mismatch`: it did, and is not
//! the target), unless the last look of the lease server failed: that is `lease_unreachable`,
//! class environment, and says nothing of the release.
//!
//! A rerun with `verify_ok` set succeeds without a look of the unit or the lease server (a
//! taint or an `owed-*` record still halts it); without it, it observes again.

use std::time::Duration;

use crate::update_hook::{
    Class, Done, HostReading, LeaseError, POLL_SECS, Params, Reason, Record, VERIFY_MARGIN_SECS,
};

use super::prepare::refuse_host;
use super::{Env, failure, lease_failure, open_state, save, success};

/// Runs `verify`.
pub fn verify(env: &Env<'_>, params: &Params) -> Done {
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
    // A taint halts even a rerun after the mark: nothing clears it.
    refuse_host(env)?;
    if record.verify_ok {
        return Ok(success());
    }
    if !record.start_issued {
        return Err(failure(
            Reason::StateInvalid,
            "the record does not say that cutover started the unit",
        ));
    }
    let on_staged = params.release_link.as_deref().is_some_and(|link| {
        let target = env.ports.link.target(link);
        target.ok().flatten().as_deref() == Some(record.update.staged.as_path())
    });
    if !on_staged {
        return Err(failure(
            Reason::StateInvalid,
            "the release link is not on the staged release",
        ));
    }
    wait(env, &record)?;
    // After the observation, so that a mark means it was made.
    record.verify_ok = true;
    save(env, &held, &record)?;
    Ok(success())
}

/// What the last look of the lease server showed.
enum Look {
    /// No new session said hello (not yet, or the server shows the session of before the stop).
    Silent,
    /// A new session said hello, and it is not the target.
    Wrong(String),
    /// The server did not answer.
    Unreachable(LeaseError),
}

/// What a reading of the host says of the new agent: `Ok` is proof.
fn judge(host: &HostReading, record: &Record, runtime: &str) -> Result<(), Look> {
    // Equality with the stamp taken before the stop, never an order against the host's clock.
    let new_session = host.state == "live"
        && host
            .session_started_ms
            .is_some_and(|session| Some(session) != record.pre_stop_session_ms);
    if !new_session {
        return Err(Look::Silent);
    }
    let target = record.update.target.as_str();
    let version_ok = host.agent_version.as_deref() == Some(target);
    let runtime_ok = host
        .runtime
        .as_deref()
        .is_none_or(|reported| reported == runtime);
    if version_ok && runtime_ok {
        Ok(())
    } else {
        Err(Look::Wrong(format!(
            "the new session is {} on {}, not {target} on {runtime}",
            host.agent_version.as_deref().unwrap_or("of no version"),
            host.runtime.as_deref().unwrap_or("no runtime"),
        )))
    }
}

/// Looks until the new agent is proved, or the bound passes.
fn wait(env: &Env<'_>, record: &Record) -> Result<(), Done> {
    let unit = record.update.unit.as_str();
    let bound_ms = env
        .declaration
        .timeouts
        .verify
        .saturating_sub(VERIFY_MARGIN_SECS)
        .saturating_mul(1000);
    let began = env.ports.clock.now_ms();
    let mut last = Look::Silent;
    loop {
        refuse_host(env)?;
        let state = env.ports.units.show(unit).map_err(|why| {
            let mut done = failure(
                Reason::StartFailed,
                format!("cannot read the state of {unit}: {why}"),
            );
            done.report.class = Class::Environment;
            done
        })?;
        let down = matches!(
            state.active.as_str(),
            "failed" | "inactive" | "deactivating"
        ) || state.sub == "auto-restart";
        if down {
            return Err(failure(
                Reason::StartFailed,
                format!("{unit} is {} ({}) during verify", state.active, state.sub),
            ));
        }
        if state.active == "active" {
            last = match env.ports.lease.host() {
                Ok(host) => match judge(&host, record, env.declaration.hello_runtime()) {
                    Ok(()) => return Ok(()),
                    Err(look) => look,
                },
                Err(why) => Look::Unreachable(why),
            };
        }
        if env.ports.clock.now_ms().saturating_sub(began) >= bound_ms {
            return Err(match last {
                Look::Unreachable(why) => lease_failure(why),
                Look::Wrong(detail) => failure(Reason::VersionMismatch, detail),
                Look::Silent => failure(
                    Reason::NoHello,
                    format!(
                        "no new session of the target said hello in {} s",
                        bound_ms / 1000
                    ),
                ),
            });
        }
        env.ports.clock.pause(Duration::from_secs(POLL_SECS));
    }
}
