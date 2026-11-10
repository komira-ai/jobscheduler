//! The per-job half of a fake agent's process: the per-job messages of the wire, with a
//! second [`FakeAgent`], keyed by job id, as their engine.
//!
//! A fake agent speaks them when its hello reports [`PER_JOB_CAPABILITY`] (the cell's
//! agents do). The real agent's node has its own half (`real_jobs.rs`), which shares the
//! heartbeat book and the start intent below. Every call below goes to the second fake,
//! so a [`Defect`] of the agent acts on its jobs as it acts on its runs.
//!
//! | what arrives or happens | what the agent does |
//! |---|---|
//! | `JobStart` | [`AgentHarness::start`]`(job)`: launched once; a start for a job id it has started is a repeat ([`Counter::StartRepeated`]), one for a job id a stop named is refused ([`Counter::StartAfterStop`]) |
//! | `JobStop` of a job it runs | the job ends; its result, `killed`, rides in every heartbeat until acknowledged |
//! | `JobStop` of a job whose result is not yet acknowledged | nothing: the result rides on |
//! | `JobStop` of a job it does not hold | [`AgentHarness::cancel`]`(job)`: the id is retired (the first stop of a job no answer handed it a start of counts in [`Counter::StopBeforeStart`]), listed in `retired_jobs` until an answer's `retired_acks` names it, and a later start of it is refused |
//! | a job of [`SIM_RUN_IMAGE`] runs out its time (its first argument, in milliseconds) | [`AgentHarness::complete`]`(job)`: its result, `succeeded`, rides in every heartbeat until acknowledged |
//! | `job_acks` names a job with a verdict this version reads | [`AgentHarness::ack`]`(job)`: the result is dropped |
//! | a heartbeat is sent, the connection drops or comes back, an answer arrives | [`AgentHarness::heartbeat`], `disconnect`, `reconnect`, `contact`; a result sent again counts in [`Counter::ResultResent`] |
//!
//! Every heartbeat lists each job the agent runs (`running`, no result) and each whose
//! result is not yet acknowledged (`exited_unacked`, with the result). The resources a job
//! holds are its start's `booked` (and one GPU per index it names).
//!
//! What the agent sent and read is written into the cell's ledger: each heartbeat
//! ([`JobBeat`]), each answer with what its heartbeat listed ([`JobReply`]), each result made
//! ([`ResultMade`]), and each launch, in the launches beside the runs', named
//! `<job>/<allocation>`. A job it runs that no heartbeat has listed yet counts as a run
//! the server cannot have seen listed (`AgentNode::runs_unlisted`), as a run does.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{
    Command, HeartbeatReply, HeldJob, HeldState, JobResult, JobResultAck, JobStartSpec, Outcome,
    PER_JOB_CAPABILITY, ReplyCheck,
};
use js_conformance::agent::{AgentHarness, FakeAgent};
use js_jobs::{AllocationId, JobId};
use js_types::Resources;
use js_types::sched::FarmTime;

use super::process::verdict;
use super::{AgentKind, AgentSpec, Io, SIM_RUN_IMAGE};
use crate::bus::CallId;
use crate::counters::Counter;
use crate::jobs::{JobBeat, JobReply, JobStartSeen, ResultMade, allocation_id};
use crate::observe::{Launch, RunId, RunningJob, Verdict};

#[cfg(doc)]
use js_conformance::agent::Defect;

/// A job the agent launched, as its start named it.
pub(super) struct Placed {
    pub(super) allocation: AllocationId,
    pub(super) resources: Resources,
}

/// A heartbeat sent and not yet answered: when, and the jobs it listed without a result.
#[derive(Default)]
struct Sent {
    at: FarmTime,
    listed: BTreeSet<JobId>,
}

/// What one heartbeat lists.
#[derive(Clone, Default)]
pub(super) struct Listing {
    /// The wire's `jobs`.
    pub(super) held: Vec<HeldJob>,
    /// The wire's `retired_jobs`.
    pub(super) retired: Vec<String>,
    pub(super) running: BTreeSet<JobId>,
    pub(super) results: BTreeSet<JobId>,
    pub(super) retired_ids: BTreeSet<JobId>,
}

/// The verdicts and the retired ids an answer acknowledges, each list as the wire's check let
/// it pass: a list that fails its check is ignored whole, as the wire says.
pub(super) struct Acks<'a> {
    pub(super) job_acks: &'a [JobResultAck],
    pub(super) retired_acks: &'a [String],
}

impl<'a> Acks<'a> {
    /// The acknowledgements of `reply`, as `check` (its check) lets them pass.
    pub(super) fn of(reply: &'a HeartbeatReply, check: &ReplyCheck<'_>) -> Self {
        let job_acks: &[JobResultAck] = if check.job_acks.is_ok() {
            &reply.job_acks
        } else {
            &[]
        };
        let retired_acks: &[String] = if check.retired_acks.is_ok() {
            &reply.retired_acks
        } else {
            &[]
        };
        Self {
            job_acks,
            retired_acks,
        }
    }
}

/// What both engines keep of the heartbeats: those sent and not yet answered, the jobs
/// whose result a heartbeat sent, and the jobs a heartbeat listed, running or with its result.
#[derive(Default)]
pub(super) struct Beats {
    /// The heartbeats sent and not yet answered.
    sent: BTreeMap<CallId, Sent>,
    /// Every job whose result a heartbeat sent at least once.
    reported: BTreeSet<JobId>,
    /// Every job a heartbeat listed, running or with its result.
    listed: BTreeSet<JobId>,
}

impl Beats {
    /// The heartbeat of `listing` was sent as call `id`: each result sent before counts as
    /// sent again ([`Counter::ResultResent`]), and the ledger is told.
    pub(super) fn sent(&mut self, id: CallId, listing: Listing, io: &mut Io) {
        for job in &listing.results {
            if !self.reported.insert(job.clone()) {
                io.bump(Counter::ResultResent);
            }
        }
        let Listing {
            running,
            results,
            retired_ids,
            ..
        } = listing;
        self.listed.extend(running.iter().chain(&results).cloned());
        let sent = Sent {
            at: io.now,
            listed: running.clone(),
        };
        self.sent.insert(id, sent);
        io.ledger.job_beat(JobBeat {
            host: io.host.clone(),
            at: io.now,
            running,
            results,
            retired: retired_ids,
        });
    }

    /// What the answer to the heartbeat sent as call `id` reads, so far: when that
    /// heartbeat was sent and the jobs it listed without a result.
    pub(super) fn reply(&mut self, id: CallId, io: &Io) -> JobReply {
        let beat = self.sent.remove(&id).unwrap_or_default();
        JobReply {
            host: io.host.clone(),
            at: io.now,
            beat_at: beat.at,
            listed: beat.listed,
            starts: Vec::new(),
            stops: BTreeSet::new(),
            acks: Vec::new(),
        }
    }

    /// Whether a heartbeat listed `job` as running.
    pub(super) fn was_listed(&self, job: &JobId) -> bool {
        self.listed.contains(job)
    }
}

/// A start of a job whose job id and allocation id are the cell server's form.
pub(super) struct Intent {
    pub(super) job: JobId,
    pub(super) allocation: AllocationId,
    /// How long the job runs by itself, for a [`SIM_RUN_IMAGE`] job.
    millis: Option<u64>,
    /// What the job holds on the host.
    resources: Resources,
}

impl Intent {
    /// The start `spec` makes on a host of `capacity`; `None` when it names a job id or an
    /// allocation id that does not parse.
    pub(super) fn of(spec: &JobStartSpec, capacity: &Resources) -> Option<Self> {
        let first = spec.command.first();
        let millis = first.and_then(|first| first.parse::<u64>().ok());
        JobId::parse(&spec.job_id)
            .ok()
            .zip(allocation_id(&spec.allocation_id))
            .map(|(job, allocation)| Self {
                job,
                allocation,
                millis: millis.filter(|_| spec.image == SIM_RUN_IMAGE),
                resources: booked(spec, capacity),
            })
    }

    /// The agent launched the job: the ledger is told, the end of a [`SIM_RUN_IMAGE`] job
    /// is armed in `timers`, and the job is entered in `jobs`.
    pub(super) fn launched(
        self,
        jobs: &mut BTreeMap<JobId, Placed>,
        timers: &mut BTreeMap<u64, JobId>,
        io: &mut Io,
    ) {
        io.ledger.launched(Launch {
            run: RunId::new(format!("{}/{}", self.job, self.allocation)),
            host: io.host.clone(),
            at: io.now,
        });
        if let Some(millis) = self.millis {
            let tag = io.timer(Duration::from_millis(millis));
            timers.insert(tag, self.job.clone());
        }
        let placed = Placed {
            allocation: self.allocation,
            resources: self.resources,
        };
        jobs.insert(self.job, placed);
    }
}

/// Counts a start of `job` that arrives after a stop of it (`stopped`), or for a job the
/// agent launched already (`launched`).
pub(super) fn note_start(stopped: &BTreeSet<JobId>, launched: bool, job: &JobId, io: &mut Io) {
    if stopped.contains(job) {
        io.bump(Counter::StartAfterStop);
    } else if launched {
        io.bump(Counter::StartRepeated);
    }
}

/// Counts a stop that is the first for its job (`first`) of a job no answer this process
/// read handed it a start of (`handed` is `false`).
pub(super) fn note_stop(first: bool, handed: bool, io: &mut Io) {
    if first && !handed {
        io.bump(Counter::StopBeforeStart);
    }
}

/// The verdicts `acks` carries on job ids and verdicts this version reads.
pub(super) fn verdicts(acks: &Acks<'_>) -> Vec<(JobId, Verdict)> {
    let read = acks
        .job_acks
        .iter()
        .filter_map(|ack| JobId::parse(&ack.job_id).ok().zip(verdict(ack.ack)));
    read.collect()
}

/// Whether the hello of `spec` reports [`PER_JOB_CAPABILITY`] as `true`.
pub(super) fn speaks(spec: &AgentSpec) -> bool {
    let value = spec.capabilities.get(PER_JOB_CAPABILITY);
    value.is_some_and(|value| value == "true")
}

/// The per-job half of one agent process.
pub(super) struct PerJob {
    fake: FakeAgent,
    /// Every job launched.
    jobs: BTreeMap<JobId, Placed>,
    /// The job each armed end timer ends.
    timers: BTreeMap<u64, JobId>,
    /// Every job a stop named.
    stopped: BTreeSet<JobId>,
    /// Every job an answer's start named, launched or not.
    handed: BTreeSet<JobId>,
    /// The jobs a stop ended while they ran: their result is `killed`.
    killed: BTreeSet<JobId>,
    /// The ids retired by a stop of a job not held, until an answer acknowledges them.
    retired: BTreeSet<JobId>,
    beats: Beats,
}

impl PerJob {
    /// The per-job half of a process of `spec`'s agent: `None` unless it is a fake whose
    /// hello reports [`PER_JOB_CAPABILITY`].
    pub(super) fn of(spec: &AgentSpec) -> Option<Self> {
        let defect = match &spec.kind {
            AgentKind::Fake(defect) => *defect,
            AgentKind::Real(_) => return None,
        };
        let fake = defect.map_or_else(|| FakeAgent::with_window(None), FakeAgent::broken);
        speaks(spec).then(|| Self {
            fake,
            jobs: BTreeMap::new(),
            timers: BTreeMap::new(),
            stopped: BTreeSet::new(),
            handed: BTreeSet::new(),
            killed: BTreeSet::new(),
            retired: BTreeSet::new(),
            beats: Beats::default(),
        })
    }

    /// The jobs the agent runs now.
    pub(super) fn truth(&self) -> BTreeMap<JobId, RunningJob> {
        let running = self.fake.observed().running;
        let jobs = self.jobs.iter();
        let now = jobs.filter(|(job, _)| running.contains(job.as_str()));
        let now = now.map(|(job, placed)| {
            let running = RunningJob {
                allocation: placed.allocation,
                resources: placed.resources.clone(),
            };
            (job.clone(), running)
        });
        now.collect()
    }

    /// What the next heartbeat lists.
    pub(super) fn listing(&self) -> Listing {
        let running = self.fake.observed().running;
        let unacked = self.fake.unacked();
        let mut listing = Listing::default();
        for (job, placed) in &self.jobs {
            if unacked.contains(job.as_str()) {
                listing.held.push(HeldJob {
                    job_id: job.to_string(),
                    state: HeldState::ExitedUnacked,
                    result: Some(self.result(job, placed)),
                });
                listing.results.insert(job.clone());
            } else if running.contains(job.as_str()) {
                listing.held.push(HeldJob {
                    job_id: job.to_string(),
                    state: HeldState::Running,
                    result: None,
                });
                listing.running.insert(job.clone());
            }
        }
        listing.retired = self.retired.iter().map(JobId::to_string).collect();
        listing.retired_ids = self.retired.clone();
        listing
    }

    /// The result of `job`: `killed` when a stop ended it, else `succeeded`.
    fn result(&self, job: &JobId, placed: &Placed) -> JobResult {
        let killed = self.killed.contains(job);
        let outcome = if killed {
            Outcome::Killed
        } else {
            Outcome::Succeeded
        };
        JobResult {
            job_id: job.to_string(),
            allocation_id: placed.allocation.to_string(),
            outcome,
            exit_code: (!killed).then_some(0),
            detail: None,
            log_tail: None,
            logs: None,
            input_failure: None,
        }
    }

    /// The heartbeat of `listing` was sent as call `id`: the fake is told, and the ledger,
    /// and each result sent before counts as sent again ([`Counter::ResultResent`]).
    pub(super) fn sent(&mut self, id: CallId, listing: Listing, io: &mut Io) {
        self.fake.heartbeat();
        self.beats.sent(id, listing, io);
    }

    /// Whether the agent runs a job that no heartbeat it sent listed, so the server cannot
    /// have seen it listed (a hello lists no job).
    pub(super) fn runs_unlisted(&self) -> bool {
        let running = self.fake.observed().running;
        let mut jobs = self
            .jobs
            .keys()
            .filter(|job| running.contains(job.as_str()));
        jobs.any(|job| !self.beats.was_listed(job))
    }

    /// The connection dropped: the heartbeat before had no answer when the next was due.
    pub(super) fn disconnect(&mut self) {
        self.fake.disconnect();
    }

    /// The connection is back: an answer arrived.
    pub(super) fn reconnect(&mut self) {
        self.fake.reconnect();
    }

    /// Acts on the answer (200) to the heartbeat sent as call `id`: each start and stop of
    /// `commands` (the reply's commands that passed the wire's checks), then each verdict
    /// and retired id of `acks`, and writes what it read into the ledger.
    pub(super) fn answered(
        &mut self,
        id: CallId,
        commands: &[&Command],
        acks: &Acks<'_>,
        capacity: &Resources,
        io: &mut Io,
    ) {
        self.fake.contact();
        let mut read = self.beats.reply(id, io);
        for command in commands {
            match command {
                Command::JobStart(spec) => self.start(spec, capacity, &mut read, io),
                Command::JobStop { job_id } => self.stop(job_id, &mut read, io),
                _ => {}
            }
        }
        for (job, verdict) in verdicts(acks) {
            self.fake.ack(job.as_str());
            read.acks.push((job, verdict));
        }
        let acked = acks.retired_acks;
        self.retired
            .retain(|job| !acked.iter().any(|id| id == job.as_str()));
        io.ledger.job_reply(read);
    }

    /// A start of the job `spec` names.
    fn start(
        &mut self,
        spec: &JobStartSpec,
        capacity: &Resources,
        read: &mut JobReply,
        io: &mut Io,
    ) {
        let Some(intent) = Intent::of(spec, capacity) else {
            return io.bump(Counter::ReplyInvalid);
        };
        let job = intent.job.clone();
        self.handed.insert(job.clone());
        read.starts.push(JobStartSeen {
            job: job.clone(),
            allocation: intent.allocation,
            refused: self.stopped.contains(&job),
        });
        let before = self.fake.observed();
        let launched = before.launches.contains_key(job.as_str());
        note_start(&self.stopped, launched, &job, io);
        self.fake.start(job.as_str(), 0, 0);
        let after = self.fake.observed();
        if after.launches.get(job.as_str()) != before.launches.get(job.as_str()) {
            intent.launched(&mut self.jobs, &mut self.timers, io);
        }
    }

    /// A stop of the job `text` names.
    fn stop(&mut self, text: &str, read: &mut JobReply, io: &mut Io) {
        let Ok(job) = JobId::parse(text) else {
            return io.bump(Counter::ReplyInvalid);
        };
        read.stops.insert(job.clone());
        let first = self.stopped.insert(job.clone());
        note_stop(first, self.handed.contains(&job), io);
        if self.fake.observed().running.contains(job.as_str()) {
            self.fake.complete(job.as_str());
            self.killed.insert(job.clone());
            made(job, io);
        } else if !self.fake.unacked().contains(job.as_str()) {
            self.fake.cancel(job.as_str());
            self.retired.insert(job);
        }
    }

    /// A timer of the process fired: `true` when it was the end of a job, which completes
    /// unless it no longer runs (a stop ended it first).
    pub(super) fn timer(&mut self, tag: u64, io: &mut Io) -> bool {
        let Some(job) = self.timers.remove(&tag) else {
            return false;
        };
        if self.fake.observed().running.contains(job.as_str()) {
            self.fake.complete(job.as_str());
            made(job, io);
        }
        true
    }
}

/// Writes into the ledger that the agent made the result of `job`, now.
pub(super) fn made(job: JobId, io: &mut Io) {
    io.ledger.result_made(ResultMade {
        host: io.host.clone(),
        job,
        at: io.now,
    });
}

/// What the job `spec` starts holds on a host of `capacity`: its booked cpu, memory and
/// disk, and one GPU of the host's class per index it names.
pub(super) fn booked(spec: &JobStartSpec, capacity: &Resources) -> Resources {
    let booked = &spec.booked;
    let gpus = booked.gpu_indices.len() as u64;
    let class = capacity.gpu_class().filter(|_| gpus > 0).map(str::to_owned);
    let resources = Resources::new(booked.cpu_millis, booked.memory_bytes, gpus, class);
    let resources = resources.unwrap_or_default();
    resources.with_disk_bytes(booked.disk_bytes.unwrap_or(0))
}

#[cfg(test)]
#[path = "per_job_tests.rs"]
mod tests;
