//! A simulated farm and its clock, for tests and for a dry run of a plan.
//!
//! The farm gives every job a host to itself: a job is placed, in submission order, on a
//! host no other live job holds, when it is submitted or at the first query after one is
//! free. A placed job starts at once and lasts its wall limit (the synthetic job does
//! nothing but take that long) or until it is cancelled; the usage store shows the host
//! using `use_percent` of what the job asked for.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use crate::clock::Clock;
use crate::farm::{Farm, FarmError, HostShape, JobAsk, JobStanding, LogTail, UsageRow};

const MINUTE_MS: u64 = 60_000;

/// A clock that moves only when something waits on it.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: Cell<u64>,
}

impl FakeClock {
    /// A clock at `ms`.
    #[must_use]
    pub fn at(ms: u64) -> Self {
        Self { now: Cell::new(ms) }
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn sleep_ms(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
}

struct FakeJob {
    id: String,
    host: Option<String>,
    start_ms: u64,
    end_ms: u64,
    cancelled: bool,
    ask: JobAsk,
}

impl FakeJob {
    /// Whether the job holds its host at `now`.
    fn holds(&self, now: u64) -> bool {
        self.host.is_some() && !self.cancelled && now < self.end_ms
    }
}

#[derive(Default)]
struct State {
    jobs: Vec<FakeJob>,
}

/// The simulated farm.
pub struct FakeFarm<'a> {
    clock: &'a FakeClock,
    hosts: Vec<(String, HostShape)>,
    outcome: String,
    log_tail: Option<LogTail>,
    use_percent: u64,
    submit_latency_ms: u64,
    state: RefCell<State>,
}

impl<'a> FakeFarm<'a> {
    /// A farm of `hosts`, on `clock`. Jobs end `succeeded`, use half of what they asked for,
    /// and a submit is answered at once.
    #[must_use]
    pub fn new(clock: &'a FakeClock, hosts: Vec<(String, HostShape)>) -> Self {
        Self {
            clock,
            hosts,
            outcome: "succeeded".to_owned(),
            log_tail: None,
            use_percent: 50,
            submit_latency_ms: 0,
            state: RefCell::new(State::default()),
        }
    }

    /// Every job that runs to its wall limit ends with this outcome (`succeeded` is a
    /// succeeded job; any other is a failed one).
    #[must_use]
    pub fn with_outcome(mut self, outcome: &str) -> Self {
        outcome.clone_into(&mut self.outcome);
        self
    }

    /// Every ended job holds this log tail. Without it a job holds an empty one.
    #[must_use]
    pub fn with_log_tail(mut self, tail: LogTail) -> Self {
        self.log_tail = Some(tail);
        self
    }

    /// Jobs use this percent of what they asked for.
    #[must_use]
    pub fn with_use_percent(mut self, percent: u64) -> Self {
        self.use_percent = percent;
        self
    }

    /// A submit takes this long to answer.
    #[must_use]
    pub fn with_submit_latency_ms(mut self, ms: u64) -> Self {
        self.submit_latency_ms = ms;
        self
    }

    /// Places each unplaced job, in order, on a host no live job holds.
    fn settle(&self) {
        let now = self.clock.now_ms();
        let mut state = self.state.borrow_mut();
        let mut held: BTreeSet<String> = state
            .jobs
            .iter()
            .filter(|job| job.holds(now))
            .filter_map(|job| job.host.clone())
            .collect();
        for job in state
            .jobs
            .iter_mut()
            .filter(|job| !job.cancelled && job.host.is_none())
        {
            if let Some((host, _)) = self.hosts.iter().find(|(id, _)| !held.contains(id)) {
                held.insert(host.clone());
                job.host = Some(host.clone());
                job.start_ms = now;
                job.end_ms = now + job.ask.wall_limit_seconds * 1000;
            }
        }
    }

    fn refused(status: u16, code: &str) -> FarmError {
        FarmError::Refused {
            status,
            code: code.to_owned(),
        }
    }
}

impl Farm for FakeFarm<'_> {
    fn submit(&self, ask: &JobAsk) -> Result<String, FarmError> {
        self.clock.sleep_ms(self.submit_latency_ms);
        let id = {
            let mut state = self.state.borrow_mut();
            let id = format!("job-{}", state.jobs.len() + 1);
            state.jobs.push(FakeJob {
                id: id.clone(),
                host: None,
                start_ms: 0,
                end_ms: 0,
                cancelled: false,
                ask: ask.clone(),
            });
            id
        };
        self.settle();
        Ok(id)
    }

    fn job(&self, id: &str) -> Result<JobStanding, FarmError> {
        self.settle();
        let now = self.clock.now_ms();
        let state = self.state.borrow();
        let found = state.jobs.iter().find(|job| job.id == id);
        let found = found.ok_or_else(|| Self::refused(404, "unknown-job"))?;
        let standing = |state: &str, outcome: Option<&str>, tail: Option<LogTail>| JobStanding {
            state: state.to_owned(),
            host: found.host.clone(),
            outcome: outcome.map(str::to_owned),
            log_tail: tail,
        };
        Ok(
            match (found.cancelled, found.host.is_some(), now < found.end_ms) {
                (true, ..) => standing(
                    "JOB_STATE_CANCELLED",
                    Some("cancelled"),
                    Some(LogTail::default()),
                ),
                (false, false, _) => standing("JOB_STATE_QUEUED", None, None),
                (false, true, true) => standing("JOB_STATE_RUNNING", None, None),
                (false, true, false) => {
                    let tail = self.log_tail.clone().unwrap_or_default();
                    let state = match self.outcome.as_str() {
                        "succeeded" => "JOB_STATE_SUCCEEDED",
                        _ => "JOB_STATE_FAILED",
                    };
                    standing(state, Some(&self.outcome), Some(tail))
                }
            },
        )
    }

    fn cancel(&self, id: &str) -> Result<(), FarmError> {
        let now = self.clock.now_ms();
        let mut state = self.state.borrow_mut();
        let found = state.jobs.iter_mut().find(|job| job.id == id);
        let found = found.ok_or_else(|| Self::refused(404, "unknown-job"))?;
        if found.host.is_none() || now < found.end_ms {
            found.cancelled = true;
            found.end_ms = found.end_ms.min(now);
        }
        Ok(())
    }

    fn host(&self, id: &str) -> Result<HostShape, FarmError> {
        let found = self.hosts.iter().find(|(host, _)| host == id);
        found
            .map(|(_, shape)| *shape)
            .ok_or_else(|| Self::refused(404, "unknown-host"))
    }

    fn usage_rows(&self, host: &str, from: u64, to: u64) -> Result<Vec<UsageRow>, FarmError> {
        let state = self.state.borrow();
        let mut rows = Vec::new();
        for minute in from..=to {
            let (begin, end) = (minute * MINUTE_MS, (minute + 1) * MINUTE_MS);
            let mut row = UsageRow {
                minute,
                cpu_millis: 0,
                ws_bytes: 0,
                leases: 0,
            };
            let mut jobs = BTreeSet::new();
            for job in state
                .jobs
                .iter()
                .filter(|job| job.host.as_deref() == Some(host))
            {
                let overlap = job.end_ms.min(end).saturating_sub(job.start_ms.max(begin));
                if overlap > 0 {
                    jobs.insert(&job.id);
                    row.cpu_millis +=
                        job.ask.cpu_millis * self.use_percent / 100 * overlap / MINUTE_MS;
                    row.ws_bytes = row
                        .ws_bytes
                        .max(job.ask.memory_bytes * self.use_percent / 100);
                }
            }
            row.leases = jobs.len() as u64;
            if row.leases > 0 {
                rows.push(row);
            }
        }
        Ok(rows)
    }
}
