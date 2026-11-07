//! The per-job half of a real agent's process: the per-job messages of the wire, with the
//! lifecycle of `js-agent` as their engine. The lifecycle already acts on `JobStart` and
//! `JobStop` and lists its jobs (`held_jobs`) and the ids it retired (`retired_jobs`); what
//! this module adds is what the cell writes down, as the fake agent's half
//! (`per_job.rs`) writes it: the same ledger entries and the same counters, for the same
//! messages.
//!
//! A real agent speaks them when its hello reports `PER_JOB_CAPABILITY` as `true`. The
//! reply is handed to the lifecycle whole, as for runs. Before that, [`RealJobs::noted`]
//! writes down what the reply carries: each start (counted after a stop or as a repeat),
//! each stop and each verdict, in a [`JobReply`] of the heartbeat the reply answers, and
//! leaves out a start or stop whose ids the cell's server would not write, which counts in
//! [`Counter::ReplyInvalid`]. After the lifecycle took its step, [`RealJobs::landed`] writes
//! each launch (a container the runtime created for the job, named `<job>/<allocation>`),
//! the reply, and each result the lifecycle now holds ([`RealJobs::settle`], also after
//! every step of the lifecycle and every end of a job: the result is made when the
//! lifecycle first lists it).
//!
//! The lifecycle lists a job from its start until its container and scratch are gone and its
//! result is acknowledged; a job it lists with a result is a result to send, one it lists
//! `running` is running. It retires a stop of a job it does not hold and lists the id until
//! an answer acknowledges it.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::{Command, HeldState};
use js_jobs::JobId;
use js_types::Resources;

use super::per_job::{Acks, Beats, Intent, Listing, Placed, made, note_start, speaks, verdicts};
use super::real::RealAgent;
use super::{AgentKind, AgentSpec, Io};
use crate::bus::CallId;
use crate::counters::Counter;
use crate::jobs::{JobReply, JobStartSeen};
use crate::observe::RunningJob;

/// What a reply carries for the jobs, written down before the lifecycle is handed it.
pub(super) struct Noted {
    /// What the answer reads.
    read: JobReply,
    /// The starts that name ids the cell's server would write.
    intents: Vec<Intent>,
    /// The ids of the starts and stops that do not: the reply is handed over without them.
    pub(super) unaccounted: BTreeSet<String>,
}

/// The per-job half of one real agent process.
pub(super) struct RealJobs {
    /// Every job launched.
    jobs: BTreeMap<JobId, Placed>,
    /// The job each armed end timer ends.
    timers: BTreeMap<u64, JobId>,
    /// Every job a stop named.
    stopped: BTreeSet<JobId>,
    /// Every job whose result the cell wrote down as made.
    made: BTreeSet<JobId>,
    beats: Beats,
}

impl RealJobs {
    /// The per-job half of a process of `spec`'s agent: `None` unless it is a real agent
    /// whose hello reports the per-job capability.
    pub(super) fn of(spec: &AgentSpec) -> Option<Self> {
        let real = matches!(spec.kind, AgentKind::Real(_));
        (real && speaks(spec)).then(|| Self {
            jobs: BTreeMap::new(),
            timers: BTreeMap::new(),
            stopped: BTreeSet::new(),
            made: BTreeSet::new(),
            beats: Beats::default(),
        })
    }

    /// The jobs the agent runs now.
    pub(super) fn truth(&self, real: &RealAgent) -> BTreeMap<JobId, RunningJob> {
        let running = real.running();
        let now = self.jobs.iter();
        let now = now.filter(|(job, _)| running.contains(job.as_str()));
        let now = now.map(|(job, placed)| {
            let running = RunningJob {
                allocation: placed.allocation,
                resources: placed.resources.clone(),
            };
            (job.clone(), running)
        });
        now.collect()
    }

    /// What the next heartbeat lists: the lifecycle's jobs and retired ids.
    pub(super) fn listing(real: &RealAgent) -> Listing {
        let held = real.held_jobs();
        let named = |ids: Vec<&str>| -> BTreeSet<JobId> {
            ids.into_iter()
                .filter_map(|id| JobId::parse(id).ok())
                .collect()
        };
        let results: Vec<&str> = held
            .iter()
            .filter(|job| job.result.is_some())
            .map(|job| job.job_id.as_str())
            .collect();
        let running: Vec<&str> = held
            .iter()
            .filter(|job| job.state == HeldState::Running)
            .map(|job| job.job_id.as_str())
            .collect();
        let retired = real.retired_jobs();
        let retired_ids = named(retired.iter().map(String::as_str).collect());
        Listing {
            running: named(running),
            results: named(results),
            retired_ids,
            held,
            retired,
        }
    }

    /// The heartbeat of `listing` was sent as call `id`.
    pub(super) fn sent(&mut self, id: CallId, listing: Listing, io: &mut Io) {
        self.beats.sent(id, listing, io);
    }

    /// Whether the agent runs a job that no heartbeat it sent listed, so the server cannot
    /// have seen it listed.
    pub(super) fn runs_unlisted(&self, real: &RealAgent) -> bool {
        let running = real.running();
        let mut jobs = self
            .jobs
            .keys()
            .filter(|job| running.contains(job.as_str()));
        jobs.any(|job| !self.beats.was_listed(job))
    }

    /// Writes down what the answer to the heartbeat sent as call `id` carries for the
    /// jobs: `commands` (the reply's commands that passed the wire's checks) and `acks`; it
    /// is written into the ledger by [`RealJobs::landed`], once the lifecycle took its
    /// step.
    pub(super) fn noted(
        &mut self,
        id: CallId,
        commands: &[&Command],
        acks: &Acks<'_>,
        capacity: &Resources,
        io: &mut Io,
    ) -> Noted {
        let read = self.beats.reply(id, io);
        let mut noted = Noted {
            read,
            intents: Vec::new(),
            unaccounted: BTreeSet::new(),
        };
        for command in commands {
            match command {
                Command::JobStart(spec) => match Intent::of(spec, capacity) {
                    Some(intent) => self.start(intent, &mut noted, io),
                    None => {
                        io.bump(Counter::ReplyInvalid);
                        noted.unaccounted.insert(spec.job_id.clone());
                    }
                },
                Command::JobStop { job_id } => match JobId::parse(job_id) {
                    Ok(job) => {
                        noted.read.stops.insert(job.clone());
                        self.stopped.insert(job);
                    }
                    Err(_) => {
                        io.bump(Counter::ReplyInvalid);
                        noted.unaccounted.insert(job_id.clone());
                    }
                },
                _ => {}
            }
        }
        noted.read.acks.extend(verdicts(acks));
        noted
    }

    /// A start of the job `intent` names, counted as the fake counts it.
    fn start(&mut self, intent: Intent, noted: &mut Noted, io: &mut Io) {
        let launched = self.jobs.contains_key(&intent.job);
        note_start(&self.stopped, launched, &intent.job, io);
        noted.read.starts.push(JobStartSeen {
            job: intent.job.clone(),
            allocation: intent.allocation,
        });
        noted.intents.push(intent);
    }

    /// The lifecycle was handed the reply and took its step, and the runtime created the
    /// containers of `created` (run ids): writes down each launch of a start of `noted`,
    /// the reply, and the results the lifecycle now holds.
    pub(super) fn landed(
        &mut self,
        noted: Noted,
        created: &[String],
        real: &RealAgent,
        io: &mut Io,
    ) {
        let Noted { read, intents, .. } = noted;
        let launched = intents
            .into_iter()
            .filter(|intent| created.iter().any(|run| run == intent.job.as_str()));
        for intent in launched {
            intent.launched(&mut self.jobs, &mut self.timers, io);
        }
        io.ledger.job_reply(read);
        self.settle(real, io);
    }

    /// A timer of the process fired: `true` when it was the end of a job; the job's
    /// container exits with status 0 unless it no longer runs.
    pub(super) fn timer(&mut self, tag: u64, real: &mut RealAgent, io: &mut Io) -> bool {
        let Some(job) = self.timers.remove(&tag) else {
            return false;
        };
        real.run_ends(job.as_str());
        self.settle(real, io);
        true
    }

    /// Writes into the ledger, once for each job, that the agent made its result, now:
    /// the first time the lifecycle lists the job with one.
    pub(super) fn settle(&mut self, real: &RealAgent, io: &mut Io) {
        let held = real.held_jobs();
        let done = held.iter().filter(|job| job.result.is_some());
        let done = done.filter_map(|job| JobId::parse(&job.job_id).ok());
        for job in done {
            if self.made.insert(job.clone()) {
                made(job, io);
            }
        }
    }
}

#[cfg(test)]
#[path = "real_jobs_tests.rs"]
mod tests;
