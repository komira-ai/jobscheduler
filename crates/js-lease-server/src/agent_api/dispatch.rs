//! The dispatch poll (`docs/design/multi-instance.md` section 0.2): `POST /v1/agent/dispatch`
//! hands a host's agent the starts and stops of its dispatch queue ([`js_jobs::DispatchMessage`],
//! `dispatch/<host>/<seq>`), from whichever server answers.
//!
//! # Who is served
//!
//! A host whose hello reported [`js_agent_wire::DISPATCH_CAPABILITY`] to a server that holds
//! the job tables ([`Agents::jobs`]); that server said `dispatch_poll` in the welcome
//! ([`super::serve_hello_in`]) and leaves the starts and stops the queue carries out of the
//! heartbeat's reply ([`crate::per_job::commands_now`]). Any other poll is a 409
//! [`NOT_SERVED`]: the agent backs off and polls again, as after any refusal, and the next
//! welcome that does not say `dispatch_poll` ends its polls. The route is behind the same
//! authentication as the heartbeat (a host certificate, a one-time token or host credential,
//! or a pool's token), the body is held to [`js_agent_wire::DispatchPoll::validate`] (400
//! `invalid-dispatch`), and a one-time token or host credential is held to its host.
//!
//! The session is checked as a heartbeat's is, by [`js_registry::Registry::check_heartbeat`],
//! which records nothing: a poll is not a beat, so it neither keeps the host alive nor moves
//! its phase, load or taint. A wrong host, epoch or session is refused as the heartbeat is
//! (409 `unknown-host`, 409 `stale-epoch`, 403 `session-mismatch`, 403 `host-bound`).
//!
//! # A poll, in order
//!
//! 1. **Acknowledgements.** Each [`js_agent_wire::DispatchAck`] names a message by allocation
//!    id and kind; the ones that name a message of this host that was handed out are deleted,
//!    in one transaction ([`js_jobs::JobStore::ack_dispatch`]). An id that does not parse, a
//!    message already gone and one never handed out change nothing. A failed commit is a 500
//!    `state-write-failed` and the agent sends them again. A draining server applies them too:
//!    an acknowledgement is not a pull.
//! 2. **The drain.** A server that drains ([`crate::drain`]) claims nothing and holds nothing:
//!    it answers an empty reply at once.
//! 3. **The claim.** The host's due messages are claimed ([`js_jobs::JobStore::claim_dispatch`],
//!    each in a transaction of its own, `FOR UPDATE SKIP LOCKED` on Postgres) for
//!    `server.dispatch_lease_secs` on the store's clock, under this server's name
//!    ([`crate::dispatch_state::DispatchState::claimer`]), and the reply is built from them: a
//!    stop is a stop; a start is built from the job and allocation rows exactly as a heartbeat
//!    builds it ([`crate::job_start::start_of`], over the context of the host's hello: network
//!    ceiling, `host-jobs`, layered rootfs, mount providers, booked shape and log sink). The
//!    body a message carries is not read: the rows are the truth, so a start reflects the
//!    allocation as it is now. A start whose allocation is not the booked one the message
//!    names, whose job has ended or has a cancel intent, or that cannot be built (logged) is
//!    left out of the reply and stays claimed until its lease ends; every end of a job deletes
//!    its start. A start that cannot be built because it breaks a rule of the wire (a job
//!    admitted with an environment the wire refuses, before `SubmitJob` checked it) is one no
//!    host could be sent: its job is ended `failed-to-start` (`agent-refused`, with the wire's
//!    code as its detail) in the same claim, which deletes the start. At most [`js_agent_wire::limits::MAX_COMMANDS`] messages are answered; the
//!    rest stay claimed until their lease ends. A failed claim is a 500 `state-write-failed`.
//! 4. **The hold.** With nothing to answer the poll is held for 5 s at most (the hold of a
//!    held beat, `crate::held_beat::MAX_HOLD`) and claims again when a placement on this
//!    server wakes it, when a notice of any server over the store tells of a message for the
//!    host (a start or a stop it wrote; [`crate::listening`]), and every
//!    `server.dispatch_recheck_ms` whatever woke it (what a lost notice leaves, and what a store
//!    one process writes sends no notice of), and answers what it claimed; at the bound, or
//!    when the drain begins, it answers empty. The watch for the wakes is taken before the first claim, so a placement between
//!    the claim and the hold ends the hold at once. A claim that fails inside the hold is
//!    logged and answered empty: the agent polls again at once, and its next first claim says
//!    500 if the store is still failing.
//!
//! A poll that has claimed is answered, and its claim is committed already (the claim is the
//! transaction), whether or not the reply reaches the agent: a reply that is lost leaves the
//! message claimed until its lease ends, and the next poll, on any server, hands it out again.
//! On a stop of the server ([`crate::server::Running::hold`]) the listener finishes the
//! request it is serving (within `--drain-timeout`) and the drain ends every held poll at once.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use js_agent_wire::{
    DispatchAck, DispatchKind as WireKind, DispatchPoll, DispatchReply, Dispatched, Heartbeat,
    Phase, limits, refusal,
};
use js_jobs::{
    AllocationId, AllocationRecord, AllocationStep, DispatchKind, DispatchMessage, JobEnd,
    JobRecord, JobState, JobStoreError, StartFailure,
};
use js_registry::{Credential, Refusal};
use js_types::HostId;
use tracing::warn;

use super::held::{HostFacts, Inputs};
use super::{
    CertifiedHost, OneTime, Refused, STATE_WRITE_FAILED, decode, host_cert, read, uploading,
};
use crate::admit::reports_dispatch_poll;
use crate::agents::Agents;
use crate::held_beat::Watch;
use crate::job_start::{Unsendable, start_of};
use crate::job_store::ServerJobs;
use crate::per_job::BeatContext;
use crate::util::blocking;

/// The code of the 409 a poll is answered when this server does not serve the poll to the
/// host: it holds no job tables, or the host's hello did not report the capability. The agent
/// backs off and polls again; its next welcome, which does not say `dispatch_poll`, ends its
/// polls, and the heartbeat's reply carries its starts and stops.
pub(super) const NOT_SERVED: &str = "dispatch-not-served";

/// The poll route: see the module documentation.
pub(super) async fn poll(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    (one_time, certified): (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<DispatchReply>, Refused> {
    let poll: DispatchPoll = decode(
        &read(body, refusal::INVALID_DISPATCH)?,
        refusal::INVALID_DISPATCH,
    )?;
    poll.validate()
        .map_err(|err| Refused::of(&Refusal::Invalid(err)))?;
    let certified = certified.map(|Extension(certified)| certified);
    host_cert::check(certified.as_ref(), &poll.host_id)?;
    if let Some(Extension(OneTime(presented))) = one_time {
        let served = agents.serve_join(presented, poll.host_id.clone()).await;
        served.map_err(Refused::join)?;
    }
    // The session is judged as a beat's is and nothing is recorded: a poll is not a beat. A
    // poll of an epoch this server's registry does not hold may be of a session another
    // server's hello began: the registry takes it from the host's row first, as for a beat.
    let beat = Heartbeat {
        seq: None,
        host_id: poll.host_id.clone(),
        epoch: poll.epoch,
        session: poll.session.clone(),
        phase: Phase::Serving,
        held: Vec::new(),
        results: Vec::new(),
        taint: None,
        taint_cleared: None,
        tainted: None,
        exclusive_ready: None,
        exclusive_refused: None,
        load: None,
        disk_gib: None,
        jobs: Vec::new(),
        retired_jobs: Vec::new(),
        log_grant_state: None,
    };
    let refreshed = super::rows::refresh_for_beat(&agents, &beat).await;
    refreshed.map_err(|error| super::unread_binding(&beat.host_id, &error))?;
    agents.registry.check_heartbeat(&beat, &who)?;
    let served = agents
        .jobs
        .as_ref()
        .zip(HostFacts::of(&agents, &poll.host_id))
        .filter(|(_, facts)| reports_dispatch_poll(facts.claim.capabilities()));
    let Some((jobs, facts)) = served else {
        return Err(Refused::new(StatusCode::CONFLICT, NOT_SERVED));
    };
    acknowledge(&agents, jobs, &facts.id, &poll.acks).await?;
    if agents.draining.begun() {
        return Ok(Json(DispatchReply::default()));
    }
    let logs = uploading(&agents, &poll.host_id);
    let everything = (Vec::new(), Vec::new(), limits::MAX_COMMANDS);
    let inputs = Arc::new(Inputs::new(jobs, &facts, logs, everything));
    // Watched from before the first claim: a placement after it wakes the hold at once.
    let watch = agents.dispatching.wakes.watch(&facts.id);
    let messages = hand_out(&agents, &inputs, &watch).await?;
    Ok(Json(DispatchReply { messages }))
}

/// Deletes the messages of `host` that `acks` name and that were handed out, on the pool for
/// blocking calls. A failed commit is a 500 `state-write-failed`.
async fn acknowledge(
    agents: &Arc<Agents>,
    jobs: &Arc<ServerJobs>,
    host: &HostId,
    acks: &[DispatchAck],
) -> Result<(), Refused> {
    let named: Vec<(AllocationId, DispatchKind)> = acks
        .iter()
        .filter_map(|ack| allocation_id(&ack.allocation_id).zip(kind_of(ack.kind)))
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    let (jobs, own) = (Arc::clone(jobs), host.clone());
    let acked = blocking(move || jobs.ack_dispatch(&own, &named)).await;
    acked.map(drop).map_err(|error| {
        warn!(%host, %error, "a dispatch poll's acknowledgements could not be committed");
        failed(agents)
    })
}

/// The 500 `state-write-failed` of a store call the poll could not make, counted.
fn failed(agents: &Agents) -> Refused {
    agents.meter().sink().store_write_failed();
    Refused::new(StatusCode::INTERNAL_SERVER_ERROR, STATE_WRITE_FAILED)
}

/// The allocation id `text` names: 32 lowercase hex digits, as the server writes one; `None`
/// for any other text.
fn allocation_id(text: &str) -> Option<AllocationId> {
    let mut bits = [0u8; 16];
    for (byte, pair) in bits.iter_mut().zip(text.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(AllocationId::from_bits(bits)).filter(|id| id.to_string() == text)
}

/// The kind of message the store knows for a wire kind; `None` for a kind this version does
/// not know, which [`DispatchPoll::validate`] has refused already.
fn kind_of(kind: WireKind) -> Option<DispatchKind> {
    match kind {
        WireKind::Start => Some(DispatchKind::Start),
        WireKind::Stop => Some(DispatchKind::Stop),
        WireKind::Unknown => None,
    }
}

/// The messages to answer a poll with: what the first claim gets, else what a claim gets when
/// a wake or the recheck comes within the hold, else nothing at the bound or at the drain.
async fn hand_out(
    agents: &Arc<Agents>,
    inputs: &Arc<Inputs>,
    watch: &Watch,
) -> Result<Vec<Dispatched>, Refused> {
    let first = claim_round(agents, inputs).await.map_err(|error| {
        warn!(host = %inputs.host, %error, "a dispatch poll's claim failed");
        failed(agents)
    })?;
    if !first.is_empty() {
        return Ok(first);
    }
    let times = agents.dispatching.times;
    let bound = tokio::time::sleep(times.hold);
    tokio::pin!(bound);
    let draining = agents.draining.begins();
    tokio::pin!(draining);
    loop {
        agents.dispatching.wakes.waiting();
        // A drain ends the hold before anything else, and a wake comes before the bound: a
        // message that is there is answered now rather than by the next poll.
        tokio::select! {
            biased;
            () = &mut draining => return Ok(Vec::new()),
            () = watch.woken() => {}
            () = tokio::time::sleep(times.recheck) => {}
            () = &mut bound => return Ok(Vec::new()),
        }
        match claim_round(agents, inputs).await {
            Ok(messages) if !messages.is_empty() => return Ok(messages),
            Ok(_) => {}
            Err(error) => {
                warn!(host = %inputs.host, %error, "a held dispatch poll's claim failed; it is answered empty");
                return Ok(Vec::new());
            }
        }
    }
}

/// One claim of the host's due messages, on the pool for blocking calls, and the reply built
/// from them.
async fn claim_round(
    agents: &Arc<Agents>,
    inputs: &Arc<Inputs>,
) -> Result<Vec<Dispatched>, JobStoreError> {
    let (agents, inputs) = (Arc::clone(agents), Arc::clone(inputs));
    blocking(move || {
        let state = &agents.dispatching;
        let claimed =
            inputs
                .jobs
                .claim_dispatch(&inputs.host, state.claimer(), state.times.lease)?;
        let context = inputs.context(agents.meter().sink());
        handed_out(&inputs.jobs, &context, &claimed)
    })
    .await
}

/// The messages of `claimed` as the poll answers them, in the order claimed, at most
/// [`limits::MAX_COMMANDS`]: a stop as it is, a start built from the rows ([`start_for`]) or
/// left out.
fn handed_out(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    claimed: &[DispatchMessage],
) -> Result<Vec<Dispatched>, JobStoreError> {
    let mut messages = Vec::with_capacity(claimed.len());
    for message in claimed {
        let handed = match message.kind {
            DispatchKind::Stop => Some(Dispatched::stop(
                message.job.to_string(),
                message.allocation_id.to_string(),
            )),
            DispatchKind::Start => start_for(jobs, context, message)?,
        };
        messages.extend(handed);
    }
    messages.truncate(limits::MAX_COMMANDS);
    Ok(messages)
}

/// The start the claimed start `message` hands out, built from the job and allocation rows as
/// they are now; `None` when it is not to be sent ([`startable`]) or cannot be built. A start
/// that breaks a rule of the wire ([`Unsendable::Invalid`]) is one the job's own record makes,
/// which no host and no later poll can send: its job is ended ([`end_unsendable`]). Any other
/// start that cannot be built (the host has not reported what the job needs) is logged and
/// stays claimed.
fn start_for(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    message: &DispatchMessage,
) -> Result<Option<Dispatched>, JobStoreError> {
    let (job, allocation) = (
        jobs.get_job(&message.job)?,
        jobs.get_allocation(&message.job)?,
    );
    let rows = job
        .zip(allocation)
        .filter(|(job, allocation)| startable(message, job, allocation));
    let Some((job, allocation)) = rows else {
        return Ok(None);
    };
    match start_of(&job, &allocation, context) {
        Ok(start) => Ok(Some(Dispatched::start(start))),
        Err(Unsendable::Invalid { code, field }) => {
            end_unsendable(jobs, context, &job, code, field);
            Ok(None)
        }
        Err(why) => {
            warn!(job = %job.id, %why, "a dispatched start is not sent");
            Ok(None)
        }
    }
}

/// Ends `job`, whose start breaks the wire's rule `code` on `field`, `failed-to-start` as a
/// start its agent refused with that code ([`StartFailure::AgentRefused`], the code kept as
/// the job's detail), which is what an agent sent the start would answer; the end deletes the
/// job's start and, the host never having had it, queues no stop. The failure is counted when
/// this end commits, and not when the job had ended another way first. An end that does not commit is logged and the start stays claimed, so its
/// next delivery reads the rows again and ends the job then.
fn end_unsendable(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    job: &JobRecord,
    code: &'static str,
    field: &'static str,
) {
    let failure = StartFailure::AgentRefused;
    let end = JobEnd::Refused {
        failure,
        detail: code.to_owned(),
    };
    match jobs.end_job(&job.id, end) {
        // A job another writer ended between the read and the end keeps that end, which
        // `end_job` returns unwritten: it is not counted, nor logged as this failure.
        Ok(record) => Some(record)
            .filter(|record| record.state == JobState::FailedToStart(failure))
            .into_iter()
            .for_each(|_| {
                warn!(job = %job.id, code, field, "a dispatched start breaks a rule of the wire; its job failed to start");
                context.metrics.start_failed(failure);
            }),
        Err(error) => {
            warn!(job = %job.id, code, field, %error, "a job whose start breaks a rule of the wire could not be ended");
        }
    }
}

/// Whether the start `message` is still one to send for `job` on `allocation`: the allocation
/// is the one the message names and is booked, and the job has not ended and has no cancel
/// intent. A start whose job went another way after it was queued is not sent; the end that
/// did it deletes the message.
fn startable(message: &DispatchMessage, job: &JobRecord, allocation: &AllocationRecord) -> bool {
    (allocation.allocation_id == message.allocation_id)
        & (allocation.step == AllocationStep::Booked)
        & !job.state.is_ended()
        & !job.cancel_requested
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
