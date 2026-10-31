//! Submits a workload to a farm and watches it to the end, noting when each thing
//! happened. Times are read from the injected clock at each poll, so a measurement is
//! good to one poll interval.

use crate::clock::Clock;
use crate::farm::{Acquired, Farm, FarmError, Phase, Standing};
use crate::workload::Job;

/// When things happened to one job. Times are milliseconds on the clock's time base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTiming {
    /// The job's name.
    pub name: String,
    /// When it was planned to be submitted.
    pub submit_at_ms: u64,
    /// When the lease request was sent.
    pub requested_ms: Option<u64>,
    /// When the lease was seen held (the answer of the request, or the poll that saw it).
    pub granted_ms: Option<u64>,
    /// The first poll that saw the run running.
    pub started_ms: Option<u64>,
    /// The poll that saw the run over.
    pub finished_ms: Option<u64>,
    /// The host the lease held.
    pub host: Option<String>,
    /// How the run ended, as the server names it.
    pub outcome: Option<String>,
    /// Why the job did not get through, when it did not.
    pub failure: Option<String>,
}

/// What a drive saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// When the drive began; planned submit times count from here.
    pub t0_ms: u64,
    /// When it ended.
    pub end_ms: u64,
    /// One entry per job, in the order of the jobs given.
    pub jobs: Vec<JobTiming>,
}

enum Stage {
    Waiting,
    Pending(String),
    Running { lease: String, run: String },
    Done,
}

struct Live {
    stage: Stage,
    timing: JobTiming,
}

impl Live {
    fn fail(&mut self, why: String) {
        self.timing.failure = Some(why);
        self.stage = Stage::Done;
    }
}

struct Driver<'a> {
    farm: &'a dyn Farm,
    clock: &'a dyn Clock,
}

impl Driver<'_> {
    fn step(&self, job: &Job, live: &mut Live, now: u64) {
        match std::mem::replace(&mut live.stage, Stage::Done) {
            Stage::Waiting if now >= live.timing.submit_at_ms => self.submit(job, live, now),
            Stage::Waiting => live.stage = Stage::Waiting,
            Stage::Pending(id) => self.poll_lease(job, live, id),
            Stage::Running { lease, run } => self.poll_run(live, lease, &run, now),
            Stage::Done => {}
        }
    }

    fn submit(&self, job: &Job, live: &mut Live, now: u64) {
        live.timing.requested_ms = Some(now);
        match self.farm.acquire(&job.lease) {
            Ok(Acquired::Held { id, host }) => self.granted(job, live, id, host),
            Ok(Acquired::Pending { id }) => live.stage = Stage::Pending(id),
            Err(err) => live.fail(err.to_string()),
        }
    }

    fn poll_lease(&self, job: &Job, live: &mut Live, id: String) {
        match self.farm.lease(&id) {
            Ok(Standing::Held { host }) => self.granted(job, live, id, host),
            Ok(Standing::Pending) => live.stage = Stage::Pending(id),
            Ok(Standing::Gone) => live.fail("lease-gone".to_owned()),
            Err(err) => self.give_up(live, &id, &err),
        }
    }

    fn granted(&self, job: &Job, live: &mut Live, id: String, host: String) {
        live.timing.granted_ms = Some(self.clock.now_ms());
        live.timing.host = Some(host);
        match self.farm.start_run(&id, &job.run) {
            Ok(run) => live.stage = Stage::Running { lease: id, run },
            Err(err) => self.give_up(live, &id, &err),
        }
    }

    fn poll_run(&self, live: &mut Live, lease: String, run: &str, now: u64) {
        match self.farm.run(&lease, run) {
            Ok(standing) => match standing.phase() {
                Phase::Waiting => {
                    live.stage = Stage::Running {
                        lease,
                        run: run.to_owned(),
                    }
                }
                Phase::Running => {
                    live.timing.started_ms.get_or_insert(now);
                    live.stage = Stage::Running {
                        lease,
                        run: run.to_owned(),
                    };
                }
                Phase::Over => {
                    live.timing.finished_ms = Some(now);
                    live.timing.outcome = standing.outcome;
                    let _ = self.farm.release(&lease);
                }
            },
            Err(err) => self.give_up(live, &lease, &err),
        }
    }

    fn give_up(&self, live: &mut Live, lease: &str, err: &FarmError) {
        let _ = self.farm.release(lease);
        live.fail(err.to_string());
    }

    fn abandon(&self, live: &mut Live) {
        let lease = match &live.stage {
            Stage::Pending(lease) | Stage::Running { lease, .. } => Some(lease.clone()),
            Stage::Waiting | Stage::Done => None,
        };
        if let Some(lease) = lease {
            let _ = self.farm.release(&lease);
        }
        if !matches!(live.stage, Stage::Done) {
            live.fail("deadline".to_owned());
        }
    }
}

/// Submits `jobs` at their planned times and polls every `poll_ms` until each is over, or
/// `deadline_ms` after the start, when the jobs still open are released and marked
/// `deadline`. A job that fails does not stop the others: its `failure` says why.
pub fn drive(
    farm: &dyn Farm,
    clock: &dyn Clock,
    jobs: &[Job],
    poll_ms: u64,
    deadline_ms: u64,
) -> Observation {
    todo!("red: not implemented")
}
