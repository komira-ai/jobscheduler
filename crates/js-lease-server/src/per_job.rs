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
//!    (the wall clock runs from there); a result moves the job to its end (or, for a start
//!    refused for a cause of the host with an attempt left under the job's retry policy, back
//!    to the queue for its next attempt, [`crate::job_retry`]) and its allocation to
//!    releasing, and is acknowledged; a start the host refused as held (`host-held`), for
//!    capacity (`no-capacity`) or while it fetches the job's bundle (`bundle-pending`) sends the
//!    job back to the queue and frees its allocation instead ([`crate::job_held`]), and is
//!    acknowledged too. A job the agent ran and ended between two heartbeats is walked
//!    through the steps it skipped first. The same listing twice commits nothing, and a
//!    report that would move a job backwards or out of an end is ignored.
//!    A listed job the store holds no live allocation of on the host is judged by what the
//!    store records of it ([`judge`]): its end, its cancel, or the end of the host's attempt
//!    of it is a reason to stop it, sent with the stop ([`StopReason`]); a job the store holds
//!    no record of is an orphan, named in the reply's `unknown_jobs`, kept running and kept in
//!    the host's row ([`crate::orphans`]). Either way the host's listing is not clean.
//! 2. **Retired ids are answered** ([`job_alloc::answer_retired`]): a job the agent never held
//!    and was told to stop is stopped and gone, so a cancelled job ends and its allocation is
//!    freed in one commit.
//! 3. **Confirmed containers are freed**: an allocation that is releasing, of a job that has
//!    ended or whose attempt on it a retry ended ([`attempt_over`]), whose host no longer lists
//!    the job, is freed ([`job_alloc::free_allocation`]). A
//!    host that is only silent is never served here, so silence frees nothing. For an agent
//!    that sends no beat sequence the server cannot tell an old beat from a new one (a beat
//!    the agent gave up on can still be applied after a newer one), so its container is freed
//!    only once two applied beats in a row leave it out: the count of each such job is kept in
//!    the host's row ([`JobReply::missing`]).
//! 4. **The reply is built** from what the store holds now: a stop for every job of the host
//!    with a cancel intent that has not ended, every listed job that ended, or whose attempt
//!    there a retry ended, without the host reporting it, and every listed job with no live
//!    allocation on this host that the store records a reason to stop or that is an orphan
//!    ended now ([`stops_for`], [`crate::orphans::judge`]), each with its reason; none but
//!    an operator's when more than half of the listing is orphans (the guard of
//!    [`crate::orphans`]); then a start for every placed job the host does not list yet
//!    ([`starts_for`]). A job that books the whole host is the exception on both sides: no
//!    container exists while it is placed (the host prepares), so it is sent no stop, and it
//!    is sent its start only once the host reported ready and the job is starting. The reply
//!    also names the allocation of such a job ([`JobReply::exclusive`]). For a host that polls
//!    dispatch ([`BeatContext::polls_dispatch`]) a start and a stop that its dispatch queue
//!    carries are left out, the poll hands them out ([`commands_now`]).
//!
//! Every change is committed before the reply that depends on it is built: a failed commit is
//! an error, the heartbeat is answered 500 `state-write-failed` with no start or stop, and the
//! agent reports again.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{
    Ack, Command, HeldJob, HeldState, JobResult, JobResultAck, JobStartSpec, NetworkCeiling,
    Outcome, StopReason, refusal,
};
use js_jobs::{
    AllocationId, AllocationRecord, AllocationStep, Booked, CpuPeak, CpuThrottle, DispatchKind,
    JobEnd, JobId, JobRecord, JobState, JobStoreError, LogLengths, Orphan, Output, ResourceUsage,
    StartFailure,
};
use js_types::{CpuSeries, HostId, MemorySeries, Resources};
use tracing::warn;

use crate::job_alloc::{self, Freed};
use crate::job_expiry::JobLimits;
use crate::job_held;
use crate::job_retry::{self, Settled};
use crate::job_start::start_of;
use crate::job_store::ServerJobs;
use crate::logs::LogGrants;
use crate::metrics::MetricsSink;
use crate::orphans::{self, Judged, Listing};

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
    /// listing is not clean takes no job.
    pub(crate) clean: bool,
    /// The allocation id of the whole-host job the host is to be prepared for or to hold,
    /// when it has one ([`exclusive_of`]): the reply's `exclusive`.
    pub(crate) exclusive: Option<String>,
    /// For an agent that sends no beat sequence: each job whose confirmed container this
    /// listing left out and did not free, with how many applied beats in a row have left it
    /// out. Empty for an agent that sends one, whose containers are freed at once.
    pub(crate) missing: BTreeMap<JobId, u32>,
    /// The listed jobs the store holds no record of, in id order: the reply's `unknown_jobs`.
    pub(crate) unknown_jobs: Vec<String>,
    /// The host's orphans after this listing ([`orphans::held`]), which the host's row keeps.
    pub(crate) orphans: BTreeMap<String, Orphan>,
    /// What the listing decided beside the live allocations, which a reply built again for
    /// the same listing keeps ([`commands_now`]).
    pub(crate) judged: Judged,
}

/// What [`commit_listing`] found.
#[derive(Debug, Default)]
struct Reported {
    acks: Vec<JobResultAck>,
    /// The listed job ids the store holds no live allocation of on this host and records a
    /// reason to stop ([`Listed::Stop`]), each with it.
    stopped: BTreeMap<String, StopReason>,
    /// The listed job ids the store holds no record of ([`Listed::Unknown`]), each with
    /// whether the host listed its result.
    unknown: BTreeMap<String, bool>,
}

/// What the store holds of a job a host lists, for that host ([`judge`]).
#[derive(Debug)]
enum Listed {
    /// A live allocation of the job on the host (and, when a result is listed, the allocation
    /// it names): the host holds it, and its listing is committed.
    Placed(Box<JobRecord>),
    /// No such allocation, and a record that the host is to stop the job, for this reason.
    Stop(StopReason),
    /// No record that bears on the job on this host: an id that is no job id, a job the store
    /// does not hold, or one it holds that ran no attempt on the host and has neither ended
    /// nor been cancelled.
    Unknown,
}

/// What a heartbeat's per-job half knows of the host it serves. It is the one argument the
/// start builder ([`start_of`]) and the result commit ([`commit_result`]) take beside the
/// job, so what a later change needs of the host reaches both from here.
#[derive(Clone, Copy)]
pub(crate) struct BeatContext<'a> {
    /// The host whose heartbeat this is.
    pub(crate) host: &'a HostId,
    /// The most network the host's agent reported it gives a job.
    pub(crate) ceiling: &'a NetworkCeiling,
    /// Whether the host's agent reported that it runs host jobs
    /// ([`js_agent_wire::HOST_JOBS_CAPABILITY`]); a host-isolation start is built only for
    /// one that did.
    pub(crate) host_jobs: bool,
    /// Whether the host's agent reported `layered-rootfs=true` ([`js_caps::LAYERED_ROOTFS`]); a
    /// start with layers, mounts or host ports is built only for one that did.
    pub(crate) layered_rootfs: bool,
    /// The keys `mount-provider.<name>` the host's agent reported with the value `true`
    /// ([`js_caps::mount_provider_key`]); a start whose job requires one of them is built only
    /// for a host that reported it.
    pub(crate) mount_providers: &'a [String],
    /// The builds of the Xcodes the host's agent reported ([`js_caps::XCODE`], split on `,`);
    /// a start whose job names a build is built only for a host that reported it.
    pub(crate) xcodes: &'a [String],
    /// What the registry books for the host, which a start of the whole host books; `None`
    /// when it is not known.
    pub(crate) shape: Option<&'a Resources>,
    /// The sinks of live logs a start carries, when the server has one and the host's agent
    /// reported that it uploads ([`crate::logs`]); `None` otherwise.
    pub(crate) logs: Option<&'a LogGrants>,
    /// Whether the host's agent polls dispatch and this server serves the poll to it
    /// ([`crate::agent_api`]): the reply then leaves out the starts and stops the host's
    /// dispatch queue carries ([`commands_now`]), which the poll hands out.
    pub(crate) polls_dispatch: bool,
    /// Where a start failure the host reports is counted.
    pub(crate) metrics: &'a dyn MetricsSink,
    /// The pool's limits, of which a start refused for capacity reads its requeue count and bar
    /// ([`job_held::held_or_end`]).
    pub(crate) limits: JobLimits,
}

/// What the host's row holds that a heartbeat's per-job half reads ([`serve`]).
#[derive(Clone, Copy, Default)]
pub(crate) struct RowState<'a> {
    /// `None` for an agent that sends a beat sequence, whose confirmed containers are freed
    /// as soon as a listing leaves them out; for one that sends none, the count the host's row
    /// kept of the applied beats in a row that left out each, and a container is freed once
    /// two have ([`JobReply::missing`]).
    pub(crate) missed: Option<&'a BTreeMap<JobId, u32>>,
    /// The host's orphans as its row holds them; `None` reads as none.
    pub(crate) orphans: Option<&'a BTreeMap<String, Orphan>>,
}

/// The per-job half of a heartbeat of the host `context` names, listing `listed` and
/// `retired`, with room for `room` commands and the host's row holding `row`: the listing
/// and the retired ids committed, confirmed containers freed, the listed jobs the host holds
/// no live allocation for judged ([`crate::orphans`]), then the stops and starts that follow
/// from the store.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the agent reports again.
pub(crate) fn serve(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    (listed, retired): (&[HeldJob], &[String]),
    room: usize,
    row: RowState<'_>,
) -> Result<JobReply, JobStoreError> {
    let missed = row.missed;
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
    let mut missing = BTreeMap::new();
    for (allocation, job) in jobs.live_on(host)? {
        let over = job.as_ref().is_none_or(attempt_over);
        let gone = !held.contains(allocation.job.as_str());
        if (allocation.step != AllocationStep::Releasing) | !over | !gone {
            continue;
        }
        let before = missed.map(|missed| missed.get(&allocation.job).copied().unwrap_or(0));
        match before {
            Some(0) => {
                missing.insert(allocation.job, 1);
            }
            _ => {
                job_alloc::free_allocation(jobs, &allocation.job, Freed::HostConfirmed)?;
            }
        }
    }
    let before = row.orphans.cloned().unwrap_or_default();
    let (orphans, now) = orphans::held(jobs, &before, &reported.unknown)?;
    let listing = Listing {
        listed: listed.len(),
        stopped: &reported.stopped,
    };
    let judged = orphans::judge(
        (host, context.metrics),
        &context.limits,
        &listing,
        (&orphans, &before, now),
    );
    let (commands, exclusive) = commands_now(jobs, context, (listed, retired), &judged, room)?;
    Ok(JobReply {
        commands,
        job_acks: reported.acks,
        retired_acks: retired.to_vec(),
        clean: reported.unknown.is_empty() & reported.stopped.is_empty(),
        exclusive,
        missing,
        unknown_jobs: reported.unknown.into_keys().collect(),
        orphans,
        judged,
    })
}

/// Step 4 of the module docs alone: the stops and starts, at most `room` of them, and the
/// allocation of the whole-host job, that follow from what the store holds now for the host
/// that listed `listed` and `retired`, and from what its listing decided (`judged`: the stops
/// of listed jobs it holds no live allocation for, and the guard). It reads and commits
/// nothing else, so a heartbeat whose reply is held ([`crate::held_beat`]) builds it again,
/// once a placement wakes it, over the listing it already committed and what that decided.
///
/// For a host that polls dispatch ([`BeatContext::polls_dispatch`]) a start is left out when
/// the host's dispatch queue carries a start of its allocation, and a stop when the queue
/// carries a stop of its job's live allocation: the poll hands those out. Everything else
/// stays, so a job a release before placed (it queued no message), a stop the queue does not
/// carry (a cancel acknowledged by the agent, which the job's end still waits for) and the
/// stop of a listed job with no live allocation (never queued) still reach the agent by the
/// reply. A host that does not poll is sent every start and stop, as it always was.
///
/// # Errors
/// The [`JobStoreError`] of the read of the host's allocations and, for a host that polls, of
/// its dispatch queue.
pub(crate) fn commands_now(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    (listed, retired): (&[HeldJob], &[String]),
    judged: &Judged,
    room: usize,
) -> Result<(Vec<Command>, Option<String>), JobStoreError> {
    let held: BTreeSet<&str> = listed
        .iter()
        .map(|job| job.job_id.as_str())
        .chain(retired.iter().map(String::as_str))
        .collect();
    let live = jobs.live_on(context.host)?;
    let (queued_starts, queued_stops) = carried_by_queue(jobs, context)?;
    let exclusive = exclusive_of(&live);
    let stopped_by_queue: BTreeSet<String> = live
        .iter()
        .filter(|(allocation, _)| queued_stops.contains(&allocation.allocation_id))
        .map(|(allocation, _)| allocation.job.to_string())
        .collect();
    let stops = stops_for(&live, listed, judged)
        .into_iter()
        .filter(|(job_id, _)| !stopped_by_queue.contains(job_id))
        .map(|(job_id, reason)| Command::JobStop {
            job_id,
            reason: Some(reason),
        });
    let starts = starts_for(&live, (&held, &queued_starts), context)
        .into_iter()
        .map(|spec| Command::JobStart(Box::new(spec)));
    let mut commands: Vec<Command> = stops.chain(starts).collect();
    commands.truncate(room);
    Ok((commands, exclusive))
}

/// The allocations the dispatch queue of the host of `context` carries a start of, and a stop
/// of: both empty for a host that does not poll dispatch, which reads nothing.
fn carried_by_queue(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
) -> Result<(BTreeSet<AllocationId>, BTreeSet<AllocationId>), JobStoreError> {
    let queue = match context.polls_dispatch {
        true => jobs.dispatch_queue(context.host)?,
        false => Vec::new(),
    };
    let of = |kind: DispatchKind| {
        queue
            .iter()
            .filter(|message| message.kind == kind)
            .map(|message| message.allocation_id)
            .collect::<BTreeSet<_>>()
    };
    Ok((of(DispatchKind::Start), of(DispatchKind::Stop)))
}

/// Commits what the host lists of each job (step 1 of the module docs) and answers each
/// result: [`Ack::Accepted`] for an end this call committed, [`Ack::Duplicate`] for a job
/// already ended, [`Ack::Stale`] for a job the store holds no live allocation of on this host
/// (or of another allocation) and for an end the job's state cannot take. A result whose
/// outcome this version does not know is left unanswered and changes nothing. A listed job
/// with no live allocation on the host is set apart as [`judge`] reads it.
fn commit_listing(
    jobs: &ServerJobs,
    context: &BeatContext<'_>,
    listed: &[HeldJob],
) -> Result<Reported, JobStoreError> {
    let mut reported = Reported::default();
    for held in listed {
        let (id, stale) = (
            held.job_id.clone(),
            held.result.as_ref().map(|_| Ack::Stale),
        );
        let ack = match judge(jobs, context.host, held)? {
            Listed::Placed(job) => report(jobs, context, *job, held)?,
            Listed::Stop(reason) => {
                reported.stopped.insert(id, reason);
                stale
            }
            Listed::Unknown => {
                reported.unknown.insert(id, held.result.is_some());
                stale
            }
        };
        reported.acks.extend(ack.map(|ack| JobResultAck {
            job_id: held.job_id.clone(),
            ack,
        }));
    }
    Ok(reported)
}

/// What the store holds of the job `held` names, for `host` ([`Listed`]): placed when the
/// store holds a live allocation of it on `host` (and, when a result is listed, the
/// allocation the result names). Otherwise a stop when the store records a reason, in this
/// order: the job ended ([`StopReason::Ended`]: lost and ended jobs are never revived); its
/// client cancelled it ([`StopReason::Cancelled`]); an attempt of it on `host` ended, so the
/// job was sent back to the queue or placed again and the host runs an old copy
/// ([`StopReason::Replaced`]). Otherwise unknown. No allocation is made for the job in any
/// case.
///
/// Only records the store holds stop a job: a store that is empty, older than the host's
/// work or another farm's holds no end, cancel or attempt of a job it never placed, so its
/// jobs read as unknown, never as stopped.
fn judge(jobs: &ServerJobs, host: &HostId, held: &HeldJob) -> Result<Listed, JobStoreError> {
    let Ok(id) = JobId::parse(&held.job_id) else {
        return Ok(Listed::Unknown);
    };
    // The allocation is read first: a job read after it is as new as the allocation or newer,
    // so an allocation that moved on is explained by the job's end or its ended attempt here.
    let allocation = jobs.get_allocation(&id)?;
    let Some(job) = jobs.get_job(&id)? else {
        return Ok(Listed::Unknown);
    };
    let current = |allocation: &AllocationRecord| {
        let this = allocation.allocation_id.to_string();
        let named = held
            .result
            .as_ref()
            .is_none_or(|result| result.allocation_id == this);
        (allocation.host == *host) & (allocation.step != AllocationStep::Freed) & named
    };
    if allocation.as_ref().is_some_and(current) {
        return Ok(Listed::Placed(Box::new(job)));
    }
    let reason = if job.state.is_ended() {
        Some(StopReason::Ended)
    } else if job.cancel_requested {
        Some(StopReason::Cancelled)
    } else if job.attempt.hosts().contains(host) {
        Some(StopReason::Replaced)
    } else {
        None
    };
    Ok(reason.map_or(Listed::Unknown, Listed::Stop))
}

/// The listed jobs of `listed` the store holds no record of for `host` ([`Listed::Unknown`]),
/// each with whether the host listed its result, read and never committed: what a reply to a
/// beat the row did not apply names, and guards, as the beat that applied the listing did.
///
/// # Errors
/// The first [`JobStoreError`] of a read.
pub(crate) fn unknown_of(
    jobs: &ServerJobs,
    host: &HostId,
    listed: &[HeldJob],
) -> Result<BTreeMap<String, bool>, JobStoreError> {
    let mut unknown = BTreeMap::new();
    for held in listed {
        if let Listed::Unknown = judge(jobs, host, held)? {
            unknown.insert(held.job_id.clone(), held.result.is_some());
        }
    }
    Ok(unknown)
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
/// verdict on it: the one place a listed result is recorded. A start the host refused as held
/// (`host-held`), for capacity (`no-capacity`) or while it fetches the job's bundle
/// (`bundle-pending`) sends the job back to the queue instead,
/// freeing its allocation, and is answered [`Ack::Accepted`] ([`job_held::held_or_end`]).
/// Otherwise the job ends as [`end_of`] reads the result (or, for the refusal of a job such
/// refusals sent back too often, with a detail naming the hosts), in one transaction with its allocation's move to releasing, after the steps
/// it skipped when it ran and ended between two heartbeats ([`Ack::Accepted`]); a job that has
/// already ended is left as it is ([`Ack::Duplicate`]); an end its state cannot take is logged
/// and changes nothing ([`Ack::Stale`]); an outcome this version does not know is left
/// unanswered. An end that retries ([`job_retry::end_or_retry`]: a start refused for a cause of
/// the host, when the job's retry policy leaves another attempt) queues the job's next attempt
/// instead of ending it, and is answered [`Ack::Accepted`] too; a result the host resends after
/// that names the allocation, now releasing, of a job that is queued, whose state takes no end,
/// and is answered [`Ack::Stale`]. A start failure is told to the metrics when it ends the job.
/// A start the host refused is told to the metrics with its code (`none` when it
/// lists none) once, when the requeue or the end it brings is committed, whether it is sent
/// back to the queue or ends the job; a duplicate, a stale listing and a commit that fails are
/// not told.
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
    // A refused start is told once its outcome is committed: a requeue or an end that fails
    // is the beat's error, and the agent lists the result again.
    let refused = matches!(result.outcome, Outcome::FailedToStart);
    let tell_refused = || {
        let code = result.detail.as_deref().unwrap_or("none");
        context.metrics.start_refused(context.host, code);
    };
    let Some(end) = job_held::held_or_end(jobs, context.host, &job, end, &context.limits)? else {
        tell_refused();
        return Ok(Some(Ack::Accepted));
    };
    let ran = matches!(
        end,
        JobEnd::Exited { .. }
            | JobEnd::WallLimit { .. }
            | JobEnd::Memory { .. }
            | JobEnd::InputFailed { .. }
    );
    let job = if ran {
        advance(jobs, job, JobState::Running)?
    } else {
        job
    };
    let failure = match &end {
        JobEnd::FailedToStart(failure) | JobEnd::Refused { failure, .. } => Some(*failure),
        _ => None,
    };
    match job_retry::end_or_retry(jobs, &job, end, context.metrics) {
        Ok(settled) => {
            if refused {
                tell_refused();
            }
            if let Settled::Ended(_) = settled {
                failure
                    .into_iter()
                    .for_each(|failure| context.metrics.start_failed(failure));
            }
            Ok(Some(Ack::Accepted))
        }
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
/// with no exit status, is a failure with [`NO_EXIT_CODE`]. The stream lengths of the
/// result's `logs` are kept with it when the report is consistent
/// ([`JobResult::logs_report`]); an inconsistent one is dropped and the result recorded
/// without it, so that a reader of the job's live logs knows where each stream ends. The
/// result's `usage`, what the job's control group counted, is kept as the output's resource
/// usage, with the CPU periods, the throttled periods, the CPU pressure, the CPU peak and the swap
/// peak it reports; a result with none records none, never a zero, and a counter it does not
/// report is `None`. The
/// result's `cpu_series` is kept as reported, beside the usage and whether or not the result has
/// one, when it is well formed ([`CpuSeries::new`]); a series that is not is dropped. Its
/// `memory_series` is kept the same way ([`MemorySeries::new`]).
///
/// A start the agent refused keeps the agent's code (`detail`) as the job's start failure
/// detail: [`StartFailure::InputUnavailable`] for `input-unavailable`, and
/// [`StartFailure::AgentRefused`] for any other code and for a refusal without one. A job that
/// ran and whose result names an input that failed (`input_failure`) is failed for its input
/// ([`JobEnd::InputFailed`]), whatever its outcome, with the exit status it reported: the
/// failure is its host's (`docs/design/job-rootfs.md`, section 8).
pub(crate) fn end_of(job: &JobRecord, result: &JobResult) -> Option<JobEnd> {
    let tail = result.log_tail.clone().unwrap_or_default();
    let logs = result.logs_report().map(|report| LogLengths {
        stdout_bytes: report.stdout_bytes,
        stderr_bytes: report.stderr_bytes,
        stdout_dropped: report.stdout_dropped,
        stderr_dropped: report.stderr_dropped,
    });
    let usage = result.usage.map(|used| ResourceUsage {
        cpu_time: Duration::from_micros(used.cpu_usage_usec),
        peak_memory_bytes: used.peak_memory_bytes,
        cpu_throttle: used.cpu_throttle.map(|throttle| CpuThrottle {
            periods: throttle.periods,
            throttled_periods: throttle.throttled_periods,
        }),
        cpu_pressure_some: used.cpu_pressure_some_usec.map(Duration::from_micros),
        cpu_peak: used.cpu_peak.map(|peak| CpuPeak {
            millicores: peak.millicores,
            window_usec: peak.window_usec,
        }),
        peak_swap_bytes: used.peak_swap_bytes,
    });
    // The wire type reads only a series with a power-of-two granularity and at most 128
    // points; the store also keeps no granularity past 16 bits, and drops such a series.
    let cpu_series = result.cpu_series.as_ref().and_then(|series| {
        CpuSeries::new(
            series.granularity_secs,
            series.points.clone(),
            series.run_usec,
            series.partial,
        )
    });
    let memory_series = result.memory_series.as_ref().and_then(|series| {
        MemorySeries::new(
            series.granularity_secs,
            series.points.clone(),
            series.run_usec,
            series.partial,
        )
    });
    let output = Output {
        stdout: tail.stdout,
        stderr: tail.stderr,
        truncated: tail.truncated,
        usage,
        logs,
        cpu_series,
        memory_series,
    };
    let code = result.exit_code;
    let failed = code.filter(|code| *code != 0).unwrap_or(NO_EXIT_CODE);
    let ran = !matches!(result.outcome, Outcome::FailedToStart | Outcome::Unknown);
    if let Some(input_failure) = result.input_failure.clone().filter(|_| ran) {
        return Some(JobEnd::InputFailed {
            exit_code: code,
            input_failure,
            output,
        });
    }
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
        Outcome::FailedToStart => refused(result.detail.as_deref()),
        Outcome::Unknown => return None,
    };
    Some(end)
}

/// The end of a start the agent refused with the code `detail`: `input-unavailable` is the
/// host's input failing ([`StartFailure::InputUnavailable`]), every other code the agent's
/// refusal ([`StartFailure::AgentRefused`]), each keeping the code; a refusal without a code
/// keeps none.
fn refused(detail: Option<&str>) -> JobEnd {
    let Some(detail) = detail else {
        return JobEnd::FailedToStart(StartFailure::AgentRefused);
    };
    let failure = match detail {
        refusal::INPUT_UNAVAILABLE => StartFailure::InputUnavailable,
        _ => StartFailure::AgentRefused,
    };
    JobEnd::Refused {
        failure,
        detail: detail.to_owned(),
    }
}

/// Whether the attempt of `job` that holds its live allocation is over: the job has ended, or
/// it is queued again, which a job with a live allocation is only when that allocation is its
/// previous attempt's, which a retry ended ([`crate::job_retry`]): the job's next attempt is
/// placed only once that allocation is freed, so the job never has two at a time.
pub(crate) fn attempt_over(job: &JobRecord) -> bool {
    job.state.is_ended() | (job.state == JobState::Queued)
}

/// Whether `job`, placed on `allocation`, is a whole-host job still preparing: the host holds
/// no container for it, so it is sent no stop and no start, and a cancel of it waits for the
/// host to leave `preparing` ([`crate::job_whole`]).
fn preparing(allocation: &AllocationRecord, job: &JobRecord) -> bool {
    matches!(allocation.booked, Booked::WholeHost) & (job.state == JobState::Placed)
}

/// The jobs a host is told to stop, in id order, each with its reason: every job of `live`
/// (the host's live allocations) with a cancel intent that has not ended (a whole-host job
/// that is still preparing excepted: [`StopReason::Cancelled`]), every listed job whose
/// attempt on the host is over ([`attempt_over`]) without the host listing its result
/// ([`StopReason::Ended`] for a job that ended, [`StopReason::Replaced`] for one a retry sent
/// back to the queue), and every job of `judged.stops` (listed jobs the host holds no live
/// allocation for, judged by [`crate::orphans::judge`]). Under the guard
/// (`judged.guarded`) the live allocations stop nothing: only `judged.stops`, which the guard
/// left with an operator's stops alone. It is resent on every heartbeat until the host stops
/// listing the job or answers with its retired id. This is the one stop rule: a cancel, a
/// start timeout or a wall limit that the store records reaches the host through it.
pub(crate) fn stops_for(
    live: &[(AllocationRecord, Option<JobRecord>)],
    listed: &[HeldJob],
    judged: &Judged,
) -> BTreeMap<String, StopReason> {
    let listing: BTreeMap<&str, &HeldJob> = listed
        .iter()
        .map(|held| (held.job_id.as_str(), held))
        .collect();
    let jobs = live
        .iter()
        .filter_map(|(allocation, job)| job.as_ref().map(|job| (allocation, job)));
    let stopped = jobs.filter_map(|(allocation, job)| {
        let ended = job.state.is_ended();
        let unreported = listing
            .get(job.id.as_str())
            .is_some_and(|held| held.result.is_none());
        let cancelled = job.cancel_requested & !ended & !preparing(allocation, job);
        let reason = match (cancelled, attempt_over(job) & unreported, ended) {
            (true, _, _) => StopReason::Cancelled,
            (false, true, true) => StopReason::Ended,
            (false, true, false) => StopReason::Replaced,
            (false, false, _) => return None,
        };
        Some((job.id.to_string(), reason))
    });
    let mut stops: BTreeMap<String, StopReason> = match judged.guarded {
        true => BTreeMap::new(),
        false => stopped.collect(),
    };
    stops.extend(judged.stops.iter().map(|(job, why)| (job.clone(), *why)));
    stops
}

/// The starts a host is sent: one for each job of `live` with a booked allocation and no
/// cancel intent that is not in `held` (listed or retired by the host) and whose allocation is
/// not in `queued` (the dispatch queue hands its start out), resent on every heartbeat until
/// the host lists it. A job that shares the host is sent its start while it is
/// placed; a job that books the whole host, once the host reported ready and moved it to
/// starting. A start the wire cannot carry is logged and not sent; the start timeout ends its
/// job.
fn starts_for(
    live: &[(AllocationRecord, Option<JobRecord>)],
    (held, queued): (&BTreeSet<&str>, &BTreeSet<AllocationId>),
    context: &BeatContext<'_>,
) -> Vec<JobStartSpec> {
    let placed = live.iter().filter_map(|(allocation, job)| {
        let job = job.as_ref()?;
        let whole = matches!(allocation.booked, Booked::WholeHost);
        let due = match whole {
            true => JobState::Starting,
            false => JobState::Placed,
        };
        let wanted = (allocation.step == AllocationStep::Booked)
            & (job.state == due)
            & !job.cancel_requested
            & !held.contains(job.id.as_str())
            & !queued.contains(&allocation.allocation_id);
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

/// The allocation id of the whole-host job `live` holds a host for, which the host is to
/// prepare for or keep: a booked allocation of the whole host whose job is placed (and has no
/// cancel intent: a cancelled job's host is let go), starting or running. `None` for a host
/// with no such job. A host holds at most one.
pub(crate) fn exclusive_of(live: &[(AllocationRecord, Option<JobRecord>)]) -> Option<String> {
    let jobs = live
        .iter()
        .filter_map(|(allocation, job)| job.as_ref().map(|job| (allocation, job)));
    let mut named = jobs.filter(|(allocation, job)| {
        let whole = matches!(allocation.booked, Booked::WholeHost);
        let waiting = (job.state == JobState::Placed) & !job.cancel_requested;
        let held = matches!(job.state, JobState::Starting | JobState::Running);
        whole & (waiting | held) & (allocation.step == AllocationStep::Booked)
    });
    named
        .next()
        .map(|(allocation, _)| allocation.allocation_id.to_string())
}

#[cfg(test)]
#[path = "per_job_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "per_job_unknown_tests.rs"]
mod unknown_tests;
