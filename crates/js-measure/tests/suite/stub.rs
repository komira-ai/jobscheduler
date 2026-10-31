//! A farm whose every answer is a value the test sets, for the paths a simulated farm
//! does not take: refusals, lost leases, a run that never ends.

use std::cell::RefCell;

use js_measure::farm::RunStanding;
use js_measure::{Acquired, Curve, Farm, FarmError, HostShape, LeaseAsk, RunAsk, UsageRow};

use js_measure::farm::Standing;

/// The answers, and the leases released so far.
pub struct Stub {
    pub acquire: Result<Acquired, FarmError>,
    pub lease: Result<Standing, FarmError>,
    pub start: Result<String, FarmError>,
    pub run: Result<RunStanding, FarmError>,
    pub released: RefCell<Vec<String>>,
}

pub fn refused() -> FarmError {
    FarmError::Refused {
        status: 409,
        code: "no-capacity".to_owned(),
    }
}

pub fn running(state: &str) -> RunStanding {
    RunStanding {
        state: state.to_owned(),
        outcome: None,
    }
}

impl Stub {
    /// A farm that holds a lease at once and whose run runs for ever.
    pub fn held() -> Self {
        Self {
            acquire: Ok(Acquired::Held {
                id: "l1".to_owned(),
                host: "h1".to_owned(),
            }),
            lease: Ok(Standing::Gone),
            start: Ok("r1".to_owned()),
            run: Ok(running("running")),
            released: RefCell::new(Vec::new()),
        }
    }

    /// A farm that leaves every lease pending.
    pub fn pending() -> Self {
        Self {
            acquire: Ok(Acquired::Pending {
                id: "l1".to_owned(),
            }),
            lease: Ok(Standing::Pending),
            ..Self::held()
        }
    }
}

impl Farm for Stub {
    fn acquire(&self, _ask: &LeaseAsk) -> Result<Acquired, FarmError> {
        self.acquire.clone()
    }

    fn lease(&self, _id: &str) -> Result<Standing, FarmError> {
        self.lease.clone()
    }

    fn start_run(&self, _lease: &str, _ask: &RunAsk) -> Result<String, FarmError> {
        self.start.clone()
    }

    fn run(&self, _lease: &str, _run: &str) -> Result<RunStanding, FarmError> {
        self.run.clone()
    }

    fn release(&self, lease: &str) -> Result<(), FarmError> {
        self.released.borrow_mut().push(lease.to_owned());
        Ok(())
    }

    fn host(&self, _id: &str) -> Result<HostShape, FarmError> {
        Err(refused())
    }

    fn usage_rows(&self, _host: &str, _from: u64, _to: u64) -> Result<Vec<UsageRow>, FarmError> {
        Err(refused())
    }

    fn curve(&self, _profile: &str) -> Result<Option<Curve>, FarmError> {
        Err(refused())
    }
}
