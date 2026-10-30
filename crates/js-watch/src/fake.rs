//! Fakes for tests of the watcher and of abort: a cloud, a status page and an abort target
//! that hold what the test gives them and answer from memory. Nothing here dials anything.

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::abort::{Doomed, Plan, Target, TargetError};
use crate::status::{Status, StatusError, StatusSource};
use crate::view::{CloudView, Seen, Stray, ViewError};

fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct CloudState {
    instances: Vec<Seen>,
    strays: Vec<Stray>,
    instances_fail: bool,
    strays_fail: bool,
}

/// A cloud in memory.
#[derive(Default)]
pub struct FakeCloud {
    state: Mutex<CloudState>,
}

impl FakeCloud {
    /// A cloud with nothing in it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the running instances.
    pub fn set_instances(&self, instances: Vec<Seen>) {
        lock(&self.state).instances = instances;
    }

    /// Sets the leftovers.
    pub fn set_strays(&self, strays: Vec<Stray>) {
        lock(&self.state).strays = strays;
    }

    /// Makes the list of instances fail (or work again).
    pub fn fail_instances(&self, fail: bool) {
        lock(&self.state).instances_fail = fail;
    }

    /// Makes the list of leftovers fail (or work again).
    pub fn fail_strays(&self, fail: bool) {
        lock(&self.state).strays_fail = fail;
    }
}

impl CloudView for FakeCloud {
    fn cloud(&self) -> &str {
        "fakecloud"
    }

    fn instances(&self) -> Result<Vec<Seen>, ViewError> {
        let state = lock(&self.state);
        match state.instances_fail {
            true => Err(ViewError::new("the instance list is down")),
            false => Ok(state.instances.clone()),
        }
    }

    fn strays(&self) -> Result<Vec<Stray>, ViewError> {
        let state = lock(&self.state);
        match state.strays_fail {
            true => Err(ViewError::new("the leftover list is down")),
            false => Ok(state.strays.clone()),
        }
    }
}

/// A status page in memory.
pub struct FakeStatus {
    state: Mutex<Result<Status, StatusError>>,
}

impl FakeStatus {
    /// A page that shows `status`.
    pub fn new(status: Status) -> Self {
        Self {
            state: Mutex::new(Ok(status)),
        }
    }

    /// Shows `status` from now on.
    pub fn show(&self, status: Status) {
        *lock(&self.state) = Ok(status);
    }

    /// Makes the page unreachable from now on.
    pub fn go_down(&self) {
        *lock(&self.state) = Err(StatusError::new("the status page is down"));
    }
}

impl StatusSource for FakeStatus {
    fn read(&self) -> Result<Status, StatusError> {
        lock(&self.state).clone()
    }
}

/// An abort target in memory that records every call it gets.
pub struct FakeTarget {
    plan: Plan,
    refuse: Vec<String>,
    calls: Mutex<Vec<String>>,
}

impl FakeTarget {
    /// A target whose plan is `plan` and that refuses to stop the ids in `refuse`.
    pub fn new(plan: Plan, refuse: &[&str]) -> Self {
        Self {
            plan,
            refuse: refuse.iter().map(|id| (*id).to_owned()).collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls received so far, in order: `plan`, and `stop <id>` for each stop.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
}

impl Target for FakeTarget {
    fn cloud(&self) -> &str {
        "fakecloud"
    }

    fn plan(&self) -> Result<Plan, TargetError> {
        lock(&self.calls).push("plan".to_owned());
        Ok(Plan {
            doomed: self.plan.doomed.clone(),
            left_alone: self.plan.left_alone.clone(),
        })
    }

    fn stop(&self, id: &str) -> Result<(), TargetError> {
        lock(&self.calls).push(format!("stop {id}"));
        match self.refuse.iter().any(|refused| refused == id) {
            true => Err(TargetError::new("the fake refuses this one")),
            false => Ok(()),
        }
    }
}

/// A doomed instance for a test.
pub fn doomed(id: &str, region: &str, cents_per_hour: Option<u64>) -> Doomed {
    Doomed {
        id: id.to_owned(),
        kind: "gpu_1x_fake".to_owned(),
        region: region.to_owned(),
        cents_per_hour,
    }
}
