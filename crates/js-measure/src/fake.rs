//! A simulated farm and its clock, for tests and for a dry run of a plan.
//!
//! The farm has whole-host leases only: a lease is placed on a host no other open lease
//! holds, in request order. A run starts the moment it is queued and lasts its wall limit
//! (the synthetic job does nothing but take that long); the usage store shows the host
//! using `use_percent` of what the run was limited to.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crate::clock::Clock;
use crate::farm::{
    Acquired, Curve, Farm, FarmError, HostShape, LeaseAsk, RunAsk, RunStanding, Standing, UsageRow,
};

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

struct FakeLease {
    id: String,
    host: Option<String>,
    open: bool,
}

struct FakeRun {
    id: String,
    lease: String,
    host: String,
    start_ms: u64,
    end_ms: u64,
    ask: RunAsk,
}

#[derive(Default)]
struct State {
    leases: Vec<FakeLease>,
    runs: Vec<FakeRun>,
}

/// The simulated farm.
pub struct FakeFarm<'a> {
    clock: &'a FakeClock,
    hosts: Vec<(String, HostShape)>,
    curves: BTreeMap<String, Curve>,
    outcome: String,
    use_percent: u64,
    acquire_latency_ms: u64,
    state: RefCell<State>,
}

impl<'a> FakeFarm<'a> {
    /// A farm of `hosts`, on `clock`. Runs end `succeeded`, use half of their limits, and
    /// a lease request is answered at once.
    #[must_use]
    pub fn new(clock: &'a FakeClock, hosts: Vec<(String, HostShape)>) -> Self {
        Self {
            clock,
            hosts,
            curves: BTreeMap::new(),
            outcome: "succeeded".to_owned(),
            use_percent: 50,
            acquire_latency_ms: 0,
            state: RefCell::new(State::default()),
        }
    }

    /// The farm knows a curve for `profile`.
    #[must_use]
    pub fn with_curve(mut self, profile: &str, curve: Curve) -> Self {
        self.curves.insert(profile.to_owned(), curve);
        self
    }

    /// Every run ends with this outcome.
    #[must_use]
    pub fn with_outcome(mut self, outcome: &str) -> Self {
        outcome.clone_into(&mut self.outcome);
        self
    }

    /// Runs use this percent of their limits.
    #[must_use]
    pub fn with_use_percent(mut self, percent: u64) -> Self {
        self.use_percent = percent;
        self
    }

    /// A lease request takes this long to answer.
    #[must_use]
    pub fn with_acquire_latency_ms(mut self, ms: u64) -> Self {
        self.acquire_latency_ms = ms;
        self
    }

    /// Places each unplaced open lease, in order, on a host no open lease holds.
    fn settle(&self) {
        let mut state = self.state.borrow_mut();
        let mut held: BTreeSet<String> = state
            .leases
            .iter()
            .filter(|lease| lease.open)
            .filter_map(|lease| lease.host.clone())
            .collect();
        for lease in state
            .leases
            .iter_mut()
            .filter(|l| l.open && l.host.is_none())
        {
            if let Some((host, _)) = self.hosts.iter().find(|(id, _)| !held.contains(id)) {
                held.insert(host.clone());
                lease.host = Some(host.clone());
            }
        }
    }

    /// Where a lease stands after placing whatever can be placed.
    fn standing(&self, id: &str) -> Standing {
        self.settle();
        let state = self.state.borrow();
        let lease = state.leases.iter().find(|l| l.id == id && l.open);
        match lease {
            None => Standing::Gone,
            Some(FakeLease { host: None, .. }) => Standing::Pending,
            Some(FakeLease {
                host: Some(host), ..
            }) => Standing::Held { host: host.clone() },
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
    fn acquire(&self, _ask: &LeaseAsk) -> Result<Acquired, FarmError> {
        self.clock.sleep_ms(self.acquire_latency_ms);
        let id = {
            let mut state = self.state.borrow_mut();
            let id = format!("lease-{}", state.leases.len() + 1);
            state.leases.push(FakeLease {
                id: id.clone(),
                host: None,
                open: true,
            });
            id
        };
        Ok(match self.standing(&id) {
            Standing::Held { host } => Acquired::Held { id, host },
            _ => Acquired::Pending { id },
        })
    }

    fn lease(&self, id: &str) -> Result<Standing, FarmError> {
        Ok(self.standing(id))
    }

    fn start_run(&self, lease: &str, ask: &RunAsk) -> Result<String, FarmError> {
        let Standing::Held { host } = self.standing(lease) else {
            return Err(Self::refused(409, "not-held"));
        };
        let mut state = self.state.borrow_mut();
        let now = self.clock.now_ms();
        let id = format!("run-{}", state.runs.len() + 1);
        state.runs.push(FakeRun {
            id: id.clone(),
            lease: lease.to_owned(),
            host,
            start_ms: now,
            end_ms: now + ask.wall_ms,
            ask: ask.clone(),
        });
        Ok(id)
    }

    fn run(&self, lease: &str, run: &str) -> Result<RunStanding, FarmError> {
        let state = self.state.borrow();
        let found = state.runs.iter().find(|r| r.lease == lease && r.id == run);
        let found = found.ok_or_else(|| Self::refused(404, "unknown-run"))?;
        Ok(match self.clock.now_ms() < found.end_ms {
            true => RunStanding {
                state: "running".to_owned(),
                outcome: None,
            },
            false => RunStanding {
                state: "exited".to_owned(),
                outcome: Some(self.outcome.clone()),
            },
        })
    }

    fn release(&self, lease: &str) -> Result<(), FarmError> {
        let mut state = self.state.borrow_mut();
        let found = state.leases.iter_mut().find(|l| l.id == lease);
        found
            .map(|l| l.open = false)
            .ok_or_else(|| Self::refused(404, "unknown-lease"))
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
            let mut leases = BTreeSet::new();
            for run in state.runs.iter().filter(|r| r.host == host) {
                let overlap = run.end_ms.min(end).saturating_sub(run.start_ms.max(begin));
                if overlap > 0 {
                    leases.insert(&run.lease);
                    row.cpu_millis +=
                        run.ask.cpu_millis * self.use_percent / 100 * overlap / MINUTE_MS;
                    row.ws_bytes = row
                        .ws_bytes
                        .max(run.ask.memory_bytes * self.use_percent / 100);
                }
            }
            row.leases = leases.len() as u64;
            if row.leases > 0 {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    fn curve(&self, profile: &str) -> Result<Option<Curve>, FarmError> {
        Ok(self.curves.get(profile).cloned())
    }
}
