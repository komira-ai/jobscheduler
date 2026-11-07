//! Submits a workload to a farm and watches it to the end, noting when each thing
//! happened. Times are read from the injected clock at each poll, so a measurement is
//! good to one poll interval.

use crate::clock::Clock;
use crate::farm::{Farm, FarmError, LogTail, Phase};
use crate::workload::Job;

/// When things happened to one job. Times are milliseconds on the clock's time base, each
/// good to one poll interval (a job is submitted at the first poll at or after its planned
/// time, and its placing, start and end are seen at later polls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTiming {
    /// The job's name.
    pub name: String,
    /// When it was planned to be submitted.
    pub submit_at_ms: u64,
    /// When the job was submitted.
    pub requested_ms: Option<u64>,
    /// The first poll that saw the job placed on a host (or past that).
    pub placed_ms: Option<u64>,
    /// The first poll that saw the job running.
    pub started_ms: Option<u64>,
    /// The poll that saw the job over.
    pub finished_ms: Option<u64>,
    /// The host the job was placed on.
    pub host: Option<String>,
    /// How the job ended, as the harness names it.
    pub outcome: Option<String>,
    /// Why the job did not get through, when it did not; also a cancel the server refused.
    pub failure: Option<String>,
    /// The end of the job's output, read when the poll saw the job over.
    pub log_tail: Option<LogTail>,
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
    Live(String),
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

/// `why` with the `note` of a cancel that failed, when there is one.
fn with_note(why: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{why}; {note}"),
        None => why,
    }
}

struct Driver<'a> {
    farm: &'a dyn Farm,
}

impl Driver<'_> {
    fn step(&self, job: &Job, live: &mut Live, now: u64) {
        match std::mem::replace(&mut live.stage, Stage::Done) {
            Stage::Waiting if now >= live.timing.submit_at_ms => self.submit(job, live, now),
            Stage::Waiting => live.stage = Stage::Waiting,
            Stage::Live(id) => self.poll(live, id, now),
            Stage::Done => {}
        }
    }

    fn submit(&self, job: &Job, live: &mut Live, now: u64) {
        live.timing.requested_ms = Some(now);
        match self.farm.submit(&job.ask) {
            Ok(id) => live.stage = Stage::Live(id),
            Err(err) => live.fail(err.to_string()),
        }
    }

    fn poll(&self, live: &mut Live, id: String, now: u64) {
        match self.farm.job(&id) {
            Ok(standing) => {
                if let Some(host) = &standing.host {
                    live.timing.placed_ms.get_or_insert(now);
                    live.timing.host.get_or_insert_with(|| host.clone());
                }
                match standing.phase() {
                    Phase::Waiting => live.stage = Stage::Live(id),
                    Phase::Running => {
                        live.timing.started_ms.get_or_insert(now);
                        live.stage = Stage::Live(id);
                    }
                    Phase::Over => {
                        live.timing.finished_ms = Some(now);
                        live.timing.outcome = standing.outcome;
                        live.timing.log_tail = standing.log_tail;
                    }
                }
            }
            Err(err) => self.give_up(live, &id, &err),
        }
    }

    /// Cancels a job; a cancel the server refuses comes back as a note for the job's
    /// failure, since the job may still hold its host and the next run's minutes on it
    /// are shared.
    fn cancel(&self, id: &str) -> Option<String> {
        let refused = self.farm.cancel(id).err()?;
        Some(format!("cancel of {id} failed: {refused}"))
    }

    fn give_up(&self, live: &mut Live, id: &str, err: &FarmError) {
        let note = self.cancel(id);
        live.fail(with_note(err.to_string(), note));
    }

    fn abandon(&self, live: &mut Live) {
        let note = match &live.stage {
            Stage::Live(id) => self.cancel(id),
            Stage::Waiting | Stage::Done => None,
        };
        if !matches!(live.stage, Stage::Done) {
            live.fail(with_note("deadline".to_owned(), note));
        }
    }
}

/// Submits `jobs` at their planned times and polls every `poll_ms` until each is over, or
/// `deadline_ms` after the start, when the jobs still open are cancelled and marked
/// `deadline`. A job that fails does not stop the others: its `failure` says why.
pub fn drive(
    farm: &dyn Farm,
    clock: &dyn Clock,
    jobs: &[Job],
    poll_ms: u64,
    deadline_ms: u64,
) -> Observation {
    let driver = Driver { farm };
    let t0_ms = clock.now_ms();
    let mut live: Vec<Live> = jobs
        .iter()
        .map(|job| Live {
            stage: Stage::Waiting,
            timing: JobTiming {
                name: job.name.clone(),
                submit_at_ms: t0_ms + job.submit_at_ms,
                requested_ms: None,
                placed_ms: None,
                started_ms: None,
                finished_ms: None,
                host: None,
                outcome: None,
                failure: None,
                log_tail: None,
            },
        })
        .collect();
    loop {
        let now = clock.now_ms();
        for (job, one) in jobs.iter().zip(live.iter_mut()) {
            driver.step(job, one, now);
        }
        if live.iter().all(|one| matches!(one.stage, Stage::Done)) {
            break;
        }
        if now.saturating_sub(t0_ms) >= deadline_ms {
            live.iter_mut().for_each(|one| driver.abandon(one));
            break;
        }
        clock.sleep_ms(poll_ms);
    }
    Observation {
        t0_ms,
        end_ms: clock.now_ms(),
        jobs: live.into_iter().map(|one| one.timing).collect(),
    }
}
