//! `prepare` (host-updates design, 4.2, 3.6): check that the host can take the update, then,
//! unless the update hands the runs over (below), cordon it and wait for it to hold no lease.
//! Nothing a client sees changes, and the agent is never stopped or signalled here: a drain
//! bound that passes with a lease still running lifts the cordon this step placed and ends
//! deferred (75); if the lift fails the cordon stays as recorded and the step still ends
//! deferred.
//!
//! In this order, each before anything is written unless it says so:
//! 1. the lock, the record (if there is one) and the params agree; a record that breaks a rule
//!    is `state_invalid` (1), except a record of another target that never took the host, which
//!    is superseded (see [`supersede`](super::supersede)): moved aside just before the new record
//!    is first written (6), and named in the detail of the step's last line. The state
//!    directory is made, mode 0700, when it is not there, and so is every directory above it
//!    that is not there, mode 0755: hostkeeper makes none, so a first install finds none (on a
//!    Mac, not even `/Library/Application Support/js-agent`);
//! 2. a taint the restarted agent will not re-check halts (76); a taint it will re-check (a
//!    residue of a post-job check the pin file does not name, see
//!    [`restored_rechecks`](crate::update_hook::StateView::restored_rechecks)) does not, and the
//!    step goes on as for a clean host. The `owed-*` records are looked at after item 3: one
//!    halts a first install (no agent runs that could settle it, so the agent the install
//!    starts would taint itself for it); on an update it does not: it is the record of a job of
//!    the running agent whose post-job checks have not run yet, which the wait of item 7 waits
//!    for on an update that drains, and which the agent finishes before it ends on one that
//!    hands the runs over (see `refuse_taint`);
//! 3. the staged release's agent (`bin/js-agent`, or `tree/usr/bin/js-agent` in a native package
//!    release, see [`agent_in`]) `--version` names the target version (1), and the staged release
//!    carries the unit the hook manages (`bad_artifact` otherwise);
//! 4. a first install needs a way to authenticate: when the host's flags file names the host's
//!    certificate and key (`--host-cert` and `--host-key`), both files must be there and the
//!    key must pass the rule the agent holds it to (a regular file owned by root or the hook's
//!    user, no access for others, not writable by its group), and the token file is not looked
//!    at; when it names neither, or cannot be read, the state root holds a host credential, or
//!    the token file is a regular file of the hook's user, mode 0600 or 0400. Otherwise the
//!    step defers (75, `join_token_missing`, a reason that is a shipped wire value) and the
//!    detail names the file to fix;
//! 5. on a Mac whose params ask for it, first install or update, the host setup (see
//!    [`host_setup`](super::host_setup)): the helper's socket group, the agent's state root and
//!    a scratch root on a volume of its own are made when they are not there. A part that cannot
//!    be read or made defers (75, `host_setup_missing`), naming it, before the record is
//!    written or the host cordoned: the agent cannot start without it, so the step defers here
//!    rather than letting `cutover` fail after its commit. A system call the system refuses
//!    (`EPERM`) fails the step there instead (1, `host_setup_refused`), since no retry changes
//!    it;
//! 6. the record is written, with the cordon marked as ours *before* it is placed unless the
//!    host is cordoned already (an operator's cordon is never ours), so that a step killed
//!    after the placing finds it marked; the reply of the lease server corrects the mark;
//! 7. the wait: poll the host, renewing the cordon we placed after each pause and never
//!    asking for more than the cap allows, until it holds no lease, has no run to finish and
//!    the state root holds no `owed-*` record. The records are read after the host, and a
//!    taint is looked for after the records at every poll: the agent writes the taint of a
//!    check that failed before it removes the record, so a check that fails during the wait
//!    with a taint the restarted agent will not re-check (item 2) halts the step (76), the
//!    cordon left in place. A drain bound that passes with a record still there defers (75,
//!    `drain_bound`) as for a lease, and the detail names the runs.
//!
//! An update hands the runs over instead (`hands_over`, decided once, when the record is
//! written, and kept in it as `handover`) on systemd with the crun runtime, the staged unit the
//! same as the installed one, and a previous agent whose `--version` lists `handover` on its
//! `stops:` line. A host whose flags file sets an egress ceiling (or cannot be read) hands over
//! only when both the previous agent, which leaves the runs' network wiring in place, and the
//! staged agent, which adopts it (and which the rollback stops in its turn), list
//! `egress-handover` there too. Its record is written with no cordon and the step ends after 6,
//! saying so in its detail; the owed checks of the running agent are not looked at (it finishes
//! them before it hands over, see `cutover`).
//!
//! The hand-over is made in place (decided at the same time, kept in the record as `in_place`,
//! and only with `handover`) when both the previous release's agent and the staged release's
//! agent list `in-place` on their `stops:` line: `cutover` and `rollback` then signal the
//! running agent, which hands over and runs its program again as the same process, and never
//! stop the unit. Otherwise the unit is stopped and started, which a systemd that starts a
//! service's first process in the unit's own cgroup (258 or newer; see [`crate::systemd`])
//! refuses while the runs a stop leaves hold controllers in it: that way is taken only when
//! `systemctl --version` is read and is older than 258, and every other host drains. The detail
//! of the success says which hand-over it is.
//!
//! An update that does not hand over drains, and a success of `prepare` says why in its detail
//! (one fixed text per [`Drain`] cause, in the order `hands_over` checks them, the systemd
//! cause last), as does the deferral of a drain that passes its bound with a lease still
//! running.
//!
//! A first install has no agent to drain and no host the lease server knows: it ends after 6.
//!
//! An update compares the unit the staged release carries with the one the previous release
//! carried (the installed one, see [`plan`](crate::update_hook::plan)). A difference is recorded
//! as the record's restart plan and named in the detail of the success; `cutover` acts on it.

use std::path::Path;
use std::time::Duration;

use js_client::{HostIdentity, host_key_problem};

use crate::systemd;
use crate::update_hook::{
    Class, Cordon, Done, Held, HostReading, Manager, Outcome, POLL_SECS, Params, PlanError, Reason,
    Record, Report, SYSTEMCTL, UnitRestart, UpdateInfo, agent_in, plan,
};

use super::host_setup::set_up;
use super::supersede::{judge, set_aside};
use super::{Env, TokenFacts, check, failure, lease_failure, open_unchecked, save, success};

/// The agent's name, the first word of its `--version`.
const AGENT_NAME: &str = "js-agent";

/// The `--version` line that lists the ways the agent's stop can go, and the word of the
/// handover in it (see [`crate::lifecycle::handover`]).
const STOPS: &str = "stops:";
const HANDOVER_WORD: &str = "handover";
/// The word of an agent that hands its runs over without ending: on the signal of the request
/// it hands over and runs its own program again, as the same process (see
/// [`crate::lifecycle::handover`]), so the unit is not stopped and not started.
const IN_PLACE_WORD: &str = "in-place";
/// The word of an agent that keeps the network wiring of the runs it leaves to the next
/// process, and adopts the wiring of the runs it finds (see [`crate::lifecycle::handover`]).
const EGRESS_WORD: &str = "egress-handover";

/// The detail of a `prepare` that hands the runs over by stopping the unit and starting it.
pub(super) const HANDOVER_NOTE: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";

/// The detail of a `prepare` that hands the runs over in place.
pub(super) const IN_PLACE_NOTE: &str = "the cutover hands the running jobs over in place (the unit is not stopped): no cordon and no drain";

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
    // Before anything else is looked at or written: no version can repair a taint the agent
    // cannot clear itself. The owed checks are looked at once the update knows whether it
    // drains (below).
    refuse_taint(env)?;
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
    let verdict = hands_over(env, params, restart.as_ref());
    let handover = match fresh {
        true => verdict.is_ok(),
        false => record.handover,
    };
    let in_place = match fresh {
        true => matches!(verdict, Ok(Way::InPlace)),
        false => record.in_place,
    };
    // Why the update drains, for the detail: the cause found now; a rerun whose conditions now
    // allow a hand-over keeps the drain it began with.
    let cause = match verdict {
        Err(cause) => cause,
        Ok(_) => Drain::Decided,
    };
    // The owed checks: a first install has no agent to settle them, and halts on them. An update
    // that drains waits for them (see `wait`) and lists them now only so that a state root that
    // cannot be listed fails the step with nothing changed; one that hands the runs over leaves
    // them to the agent, which finishes them before it ends (see `cutover`).
    if params.previous_version.is_none() {
        refuse_owed(env)?;
    } else if !handover {
        owed(env)?;
    }
    if params.previous_version.is_none() {
        join_token(env, params)?;
    }
    set_up(env, params)?;
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
        record.handover = handover;
        record.in_place = in_place;
    }
    if record.handover {
        if fresh {
            save(env, &held, &record)?;
        }
        let note = match record.in_place {
            true => IN_PLACE_NOTE,
            false => HANDOVER_NOTE,
        };
        return Ok(Done {
            report: Report {
                detail: Some(note.to_owned()),
                ..Report::default()
            },
            ..success()
        });
    }
    place(env, &held, &mut record, params, fresh)?;
    let done = wait(env, &held, &mut record, params)?;
    Ok(note_drain(done, &record, cause))
}

/// The condition that made an update drain the host instead of handing the runs over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// The host does not run the agent under systemd.
    NotSystemd,
    /// The runtime is not crun.
    NotCrun,
    /// The staged unit differs from the installed one.
    UnitChanges,
    /// The previous agent does not list `handover`, could not be run, or there is none.
    PreviousKeepsNoHandover,
    /// The host has an egress ceiling and the previous agent does not list `egress-handover`.
    PreviousKeepsNoEgress,
    /// The host has an egress ceiling and the staged agent does not list `egress-handover`.
    StagedAdoptsNoEgress,
    /// The host's systemd starts a service's first process in the unit's own cgroup (258 or
    /// newer, or its version cannot be read), and the agents do not both hand over in place.
    OwnCgroupStart,
    /// A rerun of a prepare that began as a drain, whose conditions now allow a hand-over.
    Decided,
}

impl Drain {
    /// The cause in plain words, one fixed text for each.
    fn text(self) -> &'static str {
        match self {
            Self::NotSystemd => "the host does not run the agent under systemd",
            Self::NotCrun => "the jobs do not run on the crun runtime",
            Self::UnitChanges => "the unit changes",
            Self::PreviousKeepsNoHandover => {
                "the previous release's agent does not hand its jobs over \
                 (no `handover` on its `stops:` line, or it could not be run)"
            }
            Self::PreviousKeepsNoEgress => {
                "the host has an egress ceiling (or its flags file cannot be read) and the \
                 previous release's agent does not keep the jobs' egress wiring \
                 (no `egress-handover` on its `stops:` line)"
            }
            Self::StagedAdoptsNoEgress => {
                "the host has an egress ceiling (or its flags file cannot be read) and the \
                 staged release's agent does not adopt the jobs' egress wiring \
                 (no `egress-handover` on its `stops:` line)"
            }
            Self::OwnCgroupStart => {
                "the host's systemd starts a service's first process in the unit's own cgroup \
                 (258 or newer, or its version cannot be read), which the jobs a stop leaves keep \
                 from taking one, and the previous and staged agents do not both hand over in \
                 place (no `in-place` on their `stops:` line)"
            }
            Self::Decided => "decided when the update began",
        }
    }

    /// The detail of a success that drains.
    fn note(self) -> String {
        format!("the cutover drains the host first: {}", self.text())
    }
}

/// `Ok` when this update hands the agent's runs over to the new agent at the cutover, instead
/// of draining the host, and otherwise the first condition, in a fixed order, that makes it
/// drain: on systemd with the crun runtime, whose runs outlive the agent and are adopted by its
/// next process; with the same unit (a changed one is started from an empty cgroup, see
/// `cutover`); and with a previous agent that takes the request: the previous release's agent
/// lists `handover` on its `stops:` line of `--version`. An agent from before the handover would
/// read the stop as a drain and hold the cutover for its whole drain wait. On a host with an
/// egress ceiling in its flags (`env.egress`) the runs have wiring, and the agent that is
/// stopped must leave it and the agent that starts must adopt it: both list `egress-handover`
/// (the staged agent is the one that starts, and the one a rollback stops). An agent without
/// the word would cut the runs' network or remove the runs.
///
/// The way it hands over is then in place when both the previous and the staged agent list
/// `in-place`: the unit is signalled and never stopped, so systemd places no process in its
/// cgroup. Otherwise the unit is stopped and started, which a systemd that starts a service's
/// first process in the unit's own cgroup (258 or newer) refuses while the runs the stop left
/// hold controllers there: that is allowed only on a systemd whose version is read and is
/// older, and every other host drains.
fn hands_over(env: &Env<'_>, params: &Params, restart: Option<&UnitRestart>) -> Result<Way, Drain> {
    if env.manager != Manager::Systemd {
        return Err(Drain::NotSystemd);
    }
    if params.settings.runtime != "crun" {
        return Err(Drain::NotCrun);
    }
    if restart.is_some() {
        return Err(Drain::UnitChanges);
    }
    let previous = params
        .previous_release
        .as_deref()
        .map(|release| stops_of(env, release))
        .unwrap_or_default();
    if !previous.iter().any(|word| word == HANDOVER_WORD) {
        return Err(Drain::PreviousKeepsNoHandover);
    }
    let staged = stops_of(env, &params.staged_release);
    if env.egress {
        if !previous.iter().any(|word| word == EGRESS_WORD) {
            return Err(Drain::PreviousKeepsNoEgress);
        }
        if !staged.iter().any(|word| word == EGRESS_WORD) {
            return Err(Drain::StagedAdoptsNoEgress);
        }
    }
    let lists = |words: &[String]| words.iter().any(|word| word == IN_PLACE_WORD);
    if lists(&previous) && lists(&staged) {
        return Ok(Way::InPlace);
    }
    let printed = env.runner.run(SYSTEMCTL, &["--version"]);
    match printed.ok().and_then(|printed| systemd::version(&printed)) {
        Some(version) if version < systemd::OWN_CGROUP_START => Ok(Way::StopStart),
        _ => Err(Drain::OwnCgroupStart),
    }
}

/// How an update that hands the runs over does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// The unit is signalled, and the agent hands over and runs again as the same process.
    InPlace,
    /// The unit is stopped and started.
    StopStart,
}

/// The words on the `stops:` lines of the `--version` of the agent of the release `release`;
/// none when it cannot be run.
fn stops_of(env: &Env<'_>, release: &Path) -> Vec<String> {
    let binary = agent_in(env.manager, release);
    let printed = env.runner.run(&binary.to_string_lossy(), &["--version"]);
    printed
        .iter()
        .flat_map(|printed| printed.lines())
        .filter_map(|line| line.strip_prefix(STOPS))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
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

/// A success of a prepare that drains says why, and the unit that changes, and which keys; a
/// deferral for the drain bound says why the host was draining. The unit's text stands alone
/// when the unit is the cause, so that no two notes say the same thing.
fn note_drain(mut done: Done, record: &Record, cause: Drain) -> Done {
    match done.outcome {
        Outcome::Success => {
            let drain = cause.note();
            done.report.detail = Some(match (&record.unit_restart, cause) {
                (Some(restart), Drain::UnitChanges) => unit_note(restart),
                (Some(restart), _) => format!("{}; {drain}", unit_note(restart)),
                (None, _) => drain,
            });
        }
        // The other ending `wait` gives: the deferral for the drain bound.
        _ => {
            let detail = done.report.detail.take().unwrap_or_default();
            done.report.detail = Some(format!("{detail}; draining because {}", cause.text()));
        }
    }
    done
}

/// The detail that says the unit changes, and which keys.
fn unit_note(restart: &UnitRestart) -> String {
    format!(
        "the unit changes ({}): cutover starts the agent on the new unit",
        restart.changed.join(", ")
    )
}

/// A first install needs a way to authenticate to the lease server. When the host's flags file
/// names its certificate and key (`--host-cert` and `--host-key`), that is the way: the agent
/// presents them and reads no token, so the token file is not looked at and nothing defers for
/// it, and the step checks only that the certificate file is there and that the key file is a
/// regular file the agent may use (see [`js_client::host_key_problem`]); otherwise it defers
/// with the file to fix. When the flags file names neither, or cannot be read, the host
/// credential of the state root does, and so does the join token, which the operator (or the
/// machine image) places, in a regular file of the hook's user, mode 0600 or 0400. The step
/// defers, with nothing changed, until it is right.
fn join_token(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    if let Some(identity) = env.host_identity {
        return match host_files_problem(env, identity) {
            Some(problem) => Err(deferred(
                params,
                Reason::JoinTokenMissing,
                format!(
                    "the host certificate and key named in {}: {problem}; fix it to install the agent",
                    params.flags_file.display()
                ),
            )),
            None => Ok(()),
        };
    }
    if env.ports.state.has_credential() {
        return Ok(());
    }
    let file = &params.token_file;
    let problem = match (env.token)(file) {
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => "is not there".to_owned(),
        Err(why) => format!("cannot be read: {why}"),
        Ok(facts) => match owner_only_problem(&facts, env.euid) {
            Some(problem) => problem,
            None => return Ok(()),
        },
    };
    Err(deferred(
        params,
        Reason::JoinTokenMissing,
        format!(
            "the join token file {} {problem}; place it, root-owned and mode 0600, to install the agent",
            file.display()
        ),
    ))
}

/// What is wrong with the host certificate and key files, named by path, or `None` when the
/// certificate file is a regular file and the key passes the agent's rule. The text holds no
/// content.
fn host_files_problem(env: &Env<'_>, identity: &HostIdentity) -> Option<String> {
    let cert = identity.cert_path();
    let key = identity.key_path();
    let look = |path: &Path| match (env.token)(path) {
        Ok(facts) => Ok(facts),
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Err("is not there".to_owned()),
        Err(why) => Err(format!("cannot be read: {why}")),
    };
    let cert_problem = match look(cert) {
        Ok(facts) => (!facts.regular).then(|| "is not a regular file".to_owned()),
        Err(why) => Some(why),
    };
    if let Some(problem) = cert_problem {
        return Some(format!("{} {problem}", cert.display()));
    }
    let key_problem = match look(key) {
        Ok(facts) => host_key_problem(facts.regular, facts.mode, facts.uid, env.euid),
        Err(why) => Some(why),
    };
    key_problem.map(|problem| format!("{} {problem}", key.display()))
}

/// Why a join token file is not one the agent will use: not a regular file, not mode 0600 or
/// 0400, or not owned by the hook's user (`euid`).
fn owner_only_problem(facts: &TokenFacts, euid: u32) -> Option<String> {
    if !facts.regular {
        return Some("is not a regular file".to_owned());
    }
    if !matches!(facts.mode, 0o600 | 0o400) {
        return Some(format!("must be mode 0600 or 0400, not {:04o}", facts.mode));
    }
    (facts.uid != euid).then(|| format!("must be owned by uid {euid}, not {}", facts.uid))
}

/// The deferral of a host that cannot take the release yet, for `reason`: a first install that
/// cannot authenticate (`join_token_missing`, with nothing changed), or a Mac whose host setup is
/// not complete (`host_setup_missing`). The coordinator retries after `defer_retry_secs`.
pub(super) fn deferred(params: &Params, reason: Reason, detail: String) -> Done {
    Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(reason),
            class: Class::Environment,
            retry_after_secs: Some(params.defer_retry_secs),
            detail: Some(detail),
            ..Report::default()
        },
    }
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

/// A taint the restarted agent will not re-check halts: no version can repair it. A taint it
/// will re-check does not: a residue of a post-job check (`process-as-job-user`, or
/// `daemonised-child`, `new-mount`, `user-changed`, `group-changed`, `kernel-module-changed` or
/// `watch-dir-changed`, which an older agent wrote and which the checks now only note) that the
/// state root's pin file does not name. The marker stays, the restarted agent restores it, and
/// its own re-checks clear it, so a halt would only keep the release that carries that clearing
/// from reaching the host. Every unchecked taint (`*-unreadable`, `watch-over-bound`,
/// `cgroup-not-found`) still halts, because a restart restores it pinned, as does a residue the
/// pin file names.
///
/// The `owed-*` records are not looked at: while the agent that wrote them runs, a record is a
/// job whose post-job checks have not run yet, which the agent settles in seconds, and on a
/// busy host there almost always is one. A drain waits for them (`owed`), and an agent that
/// hands its runs over finishes them before it ends; a check that fails writes its taint before
/// its record goes, and this judges that taint as any other. A record left by an agent that
/// stopped inside the window becomes a `check-owed` taint at the agent's next start, which this
/// refuses.
pub(super) fn refuse_taint(env: &Env<'_>) -> Result<(), Done> {
    let state = env.ports.state;
    match state
        .taint()
        .filter(|taint| !state.restored_rechecks(taint))
    {
        Some(taint) => Err(Done {
            outcome: Outcome::Halt,
            report: Report::failure(
                Reason::Tainted,
                format!("the agent is tainted: {}", taint.reason),
            ),
        }),
        None => Ok(()),
    }
}

/// The runs whose post-job checks are owed; a state root that cannot be listed fails the step
/// (`state_invalid`).
pub(super) fn owed(env: &Env<'_>) -> Result<Vec<String>, Done> {
    env.ports
        .state
        .owed()
        .map_err(|why| failure(Reason::StateInvalid, why.to_string()))
}

/// [`refuse_taint`], and an `owed-*` record halts too: for the steps that run where no agent
/// that could settle the record runs (a first install's `prepare` and `cutover`) or once this
/// update has stopped the agent (`verify`, and `cutover` and `rollback` after the stop). The
/// stopped agent can no longer settle it, and the agent started next taints itself for it.
pub(super) fn refuse_host(env: &Env<'_>) -> Result<(), Done> {
    refuse_taint(env)?;
    refuse_owed(env)
}

/// [`refuse_host`] for an update that hands the runs over (`handover`): only the taint. The
/// agents on either side of a handover run their jobs' post-job checks while the hook waits
/// for them, so a record of owed checks is a check under way. The agent the hook stops finishes
/// its checks before it ends, and a record one leaves anyway taints the next start
/// (`check-owed`), which then halts here as a taint.
pub(super) fn refuse_host_in(env: &Env<'_>, handover: bool) -> Result<(), Done> {
    match handover {
        true => refuse_taint(env),
        false => refuse_host(env),
    }
}

/// The owed-check half of [`refuse_host`]: an `owed-*` record halts (`owed_check`).
fn refuse_owed(env: &Env<'_>) -> Result<(), Done> {
    let owed = owed(env)?;
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

/// Whether the agent is idle enough to be stopped: no lease, no run to finish, and no job whose
/// post-job checks are owed.
pub(super) fn idle(host: &HostReading, owed: &[String]) -> bool {
    host.leases == 0 && !host.releasing && owed.is_empty()
}

/// What keeps the host from being idle, after `lead`: the leases, and the runs whose checks are
/// owed when there are any.
pub(super) fn busy_detail(lead: String, owed: &[String]) -> String {
    match owed.is_empty() {
        true => lead,
        false => format!("{lead}; checks are owed for {}", owed.join(", ")),
    }
}

/// The staged agent runs and is the version the order names; if it lists its runtimes, the
/// required one (`crun` for the Linux unit, `native` for the Mac one) is among them.
fn check_agent(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    let binary = agent_in(env.manager, &params.staged_release);
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
    host.cordon
        .as_ref()
        .map_or(0, |cordon| cordon.expires_at_ms)
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

/// Waits for the host to hold no lease and owe no check, up to the drain bound. The host is
/// read first, then the records, then the taint (see item 7 of the module header).
fn wait(env: &Env<'_>, held: &Held, record: &mut Record, params: &Params) -> Result<Done, Done> {
    let started = env.ports.clock.now_ms();
    loop {
        let host = env.ports.lease.host().map_err(lease_failure)?;
        let owed = owed(env)?;
        refuse_taint(env)?;
        if idle(&host, &owed) {
            return Ok(success());
        }
        let waited = env.ports.clock.now_ms().saturating_sub(started);
        if waited >= params.drain_wait_secs.saturating_mul(1000) {
            return defer(env, held, record, params, &host, &owed);
        }
        env.ports.clock.pause(Duration::from_secs(POLL_SECS));
        if record.cordon.placed {
            renew(env, held, record, params)?;
        }
    }
}

/// The bound passed with a lease still running or a check still owed: lift the cordon if it is
/// ours, record that there is none, and defer. Nothing was stopped.
fn defer(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    host: &HostReading,
    owed: &[String],
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
    let mut detail = busy_detail(
        format!(
            "the host still held {} lease(s) after {} s",
            host.leases, params.drain_wait_secs
        ),
        owed,
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
