//! The per-job half of a heartbeat (design sections 5.3, 5.7 and 6.2, and section 8, rows 25,
//! 37 and 38): what a host that speaks the messages keyed by job id is told, and what its job
//! listing commits.
//!
//! [`serve`] runs once per heartbeat of such a host, after the registry has accepted the beat,
//! in this order:
//!
//! 1. **The listing is committed** ([`commit_listing`]). Each listed job is compared with the
//!    store, one transaction per step, each step carrying the allocation change that goes with
//!    it: `starting` moves placed to starting; `running` moves to running and stamps the start
//!    (the wall clock runs from there); a result moves the job to its end and its allocation
//!    to releasing, and is acknowledged. A job the agent ran and ended between two heartbeats
//!    is walked through the steps it skipped first. The same listing twice commits nothing,
//!    and a report that would move a job backwards or out of an end is ignored.
//! 2. **Retired ids are answered** ([`job_alloc::answer_retired`]): a job the agent never held
//!    and was told to stop is stopped and gone, so a cancelled job ends and its allocation is
//!    freed in one commit.
//! 3. **Confirmed containers are freed**: an allocation that is releasing, of a job that has
//!    ended, whose host no longer lists the job, is freed ([`job_alloc::free_allocation`]). A
//!    host that is only silent is never served here, so silence frees nothing.
//! 4. **The reply is built** from what the store holds now: a stop for every job of the host
//!    with a cancel intent that has not ended, every listed job the store has no live
//!    allocation of on this host, and every listed job that ended without the host reporting
//!    it ([`stops_for`]); then a start for every placed job the host does not list yet
//!    ([`starts_for`]).
//!
//! Every change is committed before the reply that depends on it is built: a failed commit is
//! an error, the heartbeat is answered 500 `state-write-failed` with no start or stop, and the
//! agent reports again.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::{
    Ack, Command, HeldJob, HeldState, JobResult, JobResultAck, JobStartSpec, NetworkCeiling,
    Outcome,
};
use js_jobs::{
    AllocationRecord, AllocationStep, JobEnd, JobId, JobRecord, JobState, JobStoreError, Output,
    StartFailure,
};
use js_types::HostId;
use tracing::warn;

use crate::job_alloc::{self, Freed};
use crate::job_start::start_of;
use crate::job_store::ServerJobs;

/// The exit code a failure is recorded with when the agent reports it with none (a job
/// stopped without a cancel, a failure the agent could not read a status for).
pub(crate) const NO_EXIT_CODE: i32 = -1;

/// What a heartbeat's per-job half adds to the reply.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct JobReply {
    /// The stops, then the starts.
    pub(crate) commands: Vec<Command>,
    /// A verdict on each listed result this version can read.
    pub(crate) job_acks: Vec<JobResultAck>,
    /// Every retired id the heartbeat listed, each answered.
    pub(crate) retired_acks: Vec<String>,
    /// Whether every listed job is one the host holds a live allocation for; a host whose
    /// listing is not clean is told to stop the others.
    pub(crate) clean: bool,
}

/// What [`commit_listing`] found.
#[derive(Debug, Default)]
struct Reported {
    acks: Vec<JobResultAck>,
    /// The listed job ids the store holds no live allocation of on this host.
    unknown: BTreeSet<String>,
}

/// What a heartbeat's per-job half knows of the host it serves. It is the one argument the
/// start builder ([`start_of`]) and the result commit ([`commit_result`]) take beside the
/// job, so what a later change needs of the host reaches both from here.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BeatContext<'a> {
    /// The host whose heartbeat this is.
    pub(crate) host: &'a HostId,
    /// The most network the host's agent reported it gives a job.
    pub(crate) ceiling: &'a NetworkCeiling,
}

/// The per-job half of a heartbeat of the host `context` names, listing `listed` and
/// `retired`, with room for `room` commands: the listing and the retired ids committed,
/// confirmed containers freed, then the stops and starts that follow from the store.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the agent reports again.
pub(crate) fn serve(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    listed: &[HeldJob],
    retired: &[String],
    room: usize,
) -> Result<JobReply, JobStoreError> {
    let host = context.host;
    let reported = commit_listing(jobs, context, listed)?;
    for job_id in retired {
        if let Ok(id) = JobId::parse(job_id) {
            job_alloc::answer_retired(jobs, host, &id)?;
        }
    }
    let held: BTreeSet<&str> = listed
        .iter()
        .map(|job| job.job_id.as_str())
        .chain(retired.iter().map(String::as_str))
        .collect();
    for (allocation, job) in jobs.live_on(host)? {
        let ended = job.as_ref().is_none_or(|job| job.state.is_ended());
        let gone = !held.contains(allocation.job.as_str());
        if (allocation.step == AllocationStep::Releasing) & ended & gone {
            job_alloc::free_allocation(jobs, &allocation.job, Freed::HostConfirmed)?;
        }
    }
    let live = jobs.live_on(host)?;
    let stops = stops_for(&live, listed, &reported.unknown)
        .into_iter()
        .map(|job_id| Command::JobStop { job_id });
    let starts = starts_for(&live, &held, context)
        .into_iter()
        .map(|spec| Command::JobStart(Box::new(spec)));
    let mut commands: Vec<Command> = stops.chain(starts).collect();
    commands.truncate(room);
    Ok(JobReply {
        commands,
        job_acks: reported.acks,
        retired_acks: retired.to_vec(),
        clean: reported.unknown.is_empty(),
    })
}

/// Commits what the host lists of each job (step 1 of the module docs) and answers each
/// result: [`Ack::Accepted`] for an end this call committed, [`Ack::Duplicate`] for a job
/// already ended, [`Ack::Stale`] for a job the store holds no live allocation of on this host
/// (or of another allocation) and for an end the job's state cannot take. A result whose
/// outcome this version does not know is left unanswered and changes nothing.
fn commit_listing(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    listed: &[HeldJob],
) -> Result<Reported, JobStoreError> {
    let mut reported = Reported::default();
    for held in listed {
        let ack = match placement_of(jobs, context.host, held)? {
            Some(job) => report(jobs, context, job, held)?,
            None => {
                reported.unknown.insert(held.job_id.clone());
                held.result.as_ref().map(|_| Ack::Stale)
            }
        };
        reported.acks.extend(ack.map(|ack| JobResultAck {
            job_id: held.job_id.clone(),
            ack,
        }));
    }
    Ok(reported)
}

/// The job `held` names, when the store holds a live allocation of it on `host` (and, when a
/// result is listed, the allocation the result names): `None` for an id that is not a job id,
/// a job the store does not know or has no allocation of, and one whose allocation is freed or
/// on another host. Such a job is never revived and no allocation is made for it.
fn placement_of(
    jobs: &ServerJobs,
    host: &HostId,
    held: &HeldJob,
) -> Result<Option<JobRecord>, JobStoreError> {
    let Ok(id) = JobId::parse(&held.job_id) else {
        return Ok(None);
    };
    let (job, allocation) = (jobs.get_job(&id)?, jobs.get_allocation(&id)?);
    let current = |allocation: &AllocationRecord| {
        let this = allocation.allocation_id.to_string();
        let named = held
            .result
            .as_ref()
            .is_none_or(|result| result.allocation_id == this);
        (allocation.host == *host) & (allocation.step != AllocationStep::Freed) & named
    };
    Ok(job.filter(|_| allocation.as_ref().is_some_and(current)))
}

/// Commits what the host reports of `job`, which it holds: a state moves the job forward, a
/// result ends it ([`commit_result`]). Returns the verdict on the result, if one was listed
/// and read.
fn report(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    job: JobRecord,
    held: &HeldJob,
) -> Result<Option<Ack>, JobStoreError> {
    let Some(result) = &held.result else {
        match held.state {
            HeldState::Starting => advance(jobs, job, JobState::Starting)?,
            HeldState::Running => advance(jobs, job, JobState::Running)?,
            HeldState::ExitedUnacked | HeldState::Unknown => job,
        };
        return Ok(None);
    };
    commit_result(jobs, context, job, result)
}

/// Commits the result the host of `context` lists for `job`, which it holds, and returns the
/// verdict on it: the one place a listed result is recorded. The job ends as [`end_of`] reads
/// the result, in one transaction with its allocation's move to releasing, after the steps it
/// skipped when it ran and ended between two heartbeats ([`Ack::Accepted`]); a job that has
/// already ended is left as it is ([`Ack::Duplicate`]); an end its state cannot take is logged
/// and changes nothing ([`Ack::Stale`]); an outcome this version does not know is left
/// unanswered.
fn commit_result(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    job: JobRecord,
    result: &JobResult,
) -> Result<Option<Ack>, JobStoreError> {
    let Some(end) = end_of(&job, result) else {
        return Ok(None);
    };
    if job.state.is_ended() {
        return Ok(Some(Ack::Duplicate));
    }
    let ran = matches!(
        end,
        JobEnd::Exited { .. } | JobEnd::WallLimit { .. } | JobEnd::Memory { .. }
    );
    let job = if ran {
        advance(jobs, job, JobState::Running)?
    } else {
        job
    };
    match jobs.end_job(&job.id, end) {
        Ok(_) => Ok(Some(Ack::Accepted)),
        Err(JobStoreError::IllegalTransition { from, to, .. }) => {
            warn!(host = %context.host, job = %job.id, ?from, ?to, "a listed end the job's state cannot take is answered stale");
            Ok(Some(Ack::Stale))
        }
        Err(error) => Err(error),
    }
}

/// Moves `job` forward to `target` (starting or running) through each step it has not taken,
/// one commit per step, and returns it as it is then. A job at or past `target`, or ended, is
/// returned as it is: a report never moves a job backwards.
fn advance(
    jobs: &ServerJobs,
    job: JobRecord,
    target: JobState,
) -> Result<JobRecord, JobStoreError> {
    let job = match job.state {
        JobState::Placed => jobs.transition(&job.id, JobState::Placed, JobState::Starting)?,
        _ => job,
    };
    match (job.state, target) {
        (JobState::Starting, JobState::Running) => {
            jobs.transition(&job.id, JobState::Starting, JobState::Running)
        }
        _ => Ok(job),
    }
}

/// The end a listed result records for `job`, or `None` for an outcome this version does not
/// know. A stop the client asked for is a cancel; a stop it did not ask for, and a failure
/// with no exit status, is a failure with [`NO_EXIT_CODE`].
pub(crate) fn end_of(job: &JobRecord, result: &JobResult) -> Option<JobEnd> {
    let tail = result.log_tail.clone().unwrap_or_default();
    let output = Output {
        stdout: tail.stdout,
        stderr: tail.stderr,
        truncated: tail.truncated,
        usage: None,
    };
    let code = result.exit_code;
    let failed = code.filter(|code| *code != 0).unwrap_or(NO_EXIT_CODE);
    let end = match result.outcome {
        Outcome::Succeeded => JobEnd::Exited {
            exit_code: code.unwrap_or(0),
            output,
        },
        Outcome::Killed if job.cancel_requested => JobEnd::Cancelled {
            exit_code: code,
            output,
        },
        Outcome::Failed | Outcome::Killed => JobEnd::Exited {
            exit_code: failed,
            output,
        },
        Outcome::TimedOut => JobEnd::WallLimit {
            exit_code: code,
            output,
        },
        Outcome::OutOfMemory => JobEnd::Memory {
            exit_code: code,
            output,
        },
        Outcome::FailedToStart => JobEnd::FailedToStart(StartFailure::AgentRefused),
        Outcome::Unknown => return None,
    };
    Some(end)
}

/// The jobs a host is told to stop, in id order: every job of `live` (the host's live
/// allocations) with a cancel intent that has not ended, every listed job that has ended
/// without the host listing its result, and every id of `unknown` (listed jobs the store holds
/// no live allocation of on this host). It is resent on every heartbeat until the host stops
/// listing the job or answers with its retired id. This is the one stop rule: a cancel, a
/// start timeout or a wall limit that the store records reaches the host through it.
pub(crate) fn stops_for(
    live: &[(AllocationRecord, Option<JobRecord>)],
    listed: &[HeldJob],
    unknown: &BTreeSet<String>,
) -> BTreeSet<String> {
    let listing: BTreeMap<&str, &HeldJob> = listed
        .iter()
        .map(|held| (held.job_id.as_str(), held))
        .collect();
    let jobs = live.iter().filter_map(|(_, job)| job.as_ref());
    let stopped = jobs.filter(|job| {
        let ended = job.state.is_ended();
        let unreported = listing
            .get(job.id.as_str())
            .is_some_and(|held| held.result.is_none());
        (job.cancel_requested & !ended) | (ended & unreported)
    });
    let mut stops: BTreeSet<String> = stopped.map(|job| job.id.to_string()).collect();
    stops.extend(unknown.iter().cloned());
    stops
}

/// The starts a host is sent: one for each job of `live` that is placed with a booked
/// allocation, has no cancel intent and is not in `held` (listed or retired by the host),
/// resent on every heartbeat until the host lists it. A start the wire cannot carry is logged
/// and not sent; the start timeout ends its job.
fn starts_for(
    live: &[(AllocationRecord, Option<JobRecord>)],
    held: &BTreeSet<&str>,
    context: &BeatContext<'_>,
) -> Vec<JobStartSpec> {
    let placed = live.iter().filter_map(|(allocation, job)| {
        let job = job.as_ref()?;
        let wanted = (allocation.step == AllocationStep::Booked)
            & (job.state == JobState::Placed)
            & !job.cancel_requested
            & !held.contains(job.id.as_str());
        wanted.then_some((allocation, job))
    });
    placed
        .filter_map(|(allocation, job)| {
            start_of(job, allocation, context)
                .inspect_err(|why| warn!(job = %job.id, %why, "a placed job's start is not sent"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
#[path = "per_job_tests.rs"]
mod tests;
