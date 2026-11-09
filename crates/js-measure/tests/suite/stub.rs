//! A farm whose every answer is a value the test sets, for the paths a simulated farm
//! does not take: refusals, a job that never ends, a cancel the server refuses.

use std::cell::RefCell;

use js_measure::{Farm, FarmError, HostShape, JobAsk, JobStanding, UsageRow};

/// The answers, and the jobs cancelled so far.
pub struct Stub {
    pub submit: Result<String, FarmError>,
    pub job: Result<JobStanding, FarmError>,
    pub cancelled: RefCell<Vec<String>>,
    pub cancel_fails: bool,
}

pub fn refused() -> FarmError {
    FarmError::Refused {
        status: 409,
        code: "no-capacity".to_owned(),
    }
}

/// A job in `state` on `host`, with no outcome and no output.
pub fn standing(state: &str, host: Option<&str>) -> JobStanding {
    JobStanding {
        state: state.to_owned(),
        host: host.map(str::to_owned),
        outcome: None,
        log_tail: None,
    }
}

/// A job that ended in `state` with `outcome` and no output.
pub fn ended(state: &str, outcome: &str) -> JobStanding {
    JobStanding {
        outcome: Some(outcome.to_owned()),
        ..standing(state, Some("h1"))
    }
}

impl Stub {
    /// A farm whose job is placed at once and runs for ever.
    pub fn running() -> Self {
        Self {
            submit: Ok("j1".to_owned()),
            job: Ok(standing("JOB_STATE_RUNNING", Some("h1"))),
            cancelled: RefCell::new(Vec::new()),
            cancel_fails: false,
        }
    }

    /// A farm whose job stays queued.
    pub fn queued() -> Self {
        Self {
            job: Ok(standing("JOB_STATE_QUEUED", None)),
            ..Self::running()
        }
    }
}

impl Farm for Stub {
    fn submit(&self, _ask: &JobAsk) -> Result<String, FarmError> {
        self.submit.clone()
    }

    fn job(&self, _id: &str) -> Result<JobStanding, FarmError> {
        self.job.clone()
    }

    fn cancel(&self, id: &str) -> Result<(), FarmError> {
        self.cancelled.borrow_mut().push(id.to_owned());
        match self.cancel_fails {
            true => Err(refused()),
            false => Ok(()),
        }
    }

    fn host(&self, _id: &str) -> Result<HostShape, FarmError> {
        Err(refused())
    }

    fn usage_rows(&self, _host: &str, _from: u64, _to: u64) -> Result<Vec<UsageRow>, FarmError> {
        Err(refused())
    }
}
