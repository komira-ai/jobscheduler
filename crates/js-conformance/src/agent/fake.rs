use std::collections::{BTreeMap, BTreeSet};

use super::{AgentHarness, Delivery, Observed};

/// The fence window of [`FakeAgent`], in milliseconds.
pub const FENCE_WINDOW: u64 = 30_000;

/// The one behaviour a broken [`FakeAgent`] lacks. Each is the cause of exactly one
/// scenario failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// A repeated Start for a lease launches its job again.
    StartRunsTwice,
    /// A Cancel stops the job but the lease stays listed as running.
    CancelKeepsListed,
    /// A Start past its validity bound is run.
    RunsLateStart,
    /// The agent never stops its leases for want of the scheduler.
    NeverFences,
    /// A result not yet acknowledged is forgotten when the connection drops.
    ForgetsUnackedResult,
}

/// A correct in-process agent, or with one [`Defect`] a broken one.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    defect: Option<Defect>,
    now: u64,
    last_contact: u64,
    connected: bool,
    known: BTreeSet<String>,
    running: BTreeSet<String>,
    fenced: BTreeSet<String>,
    never_started: BTreeSet<String>,
    launches: BTreeMap<String, u32>,
    unacked: BTreeSet<String>,
    sent: Vec<String>,
}

impl FakeAgent {
    /// A correct agent, connected, at time 0.
    pub fn new() -> Self {
        Self {
            defect: None,
            now: 0,
            last_contact: 0,
            connected: true,
            known: BTreeSet::new(),
            running: BTreeSet::new(),
            fenced: BTreeSet::new(),
            never_started: BTreeSet::new(),
            launches: BTreeMap::new(),
            unacked: BTreeSet::new(),
            sent: Vec::new(),
        }
    }

    /// An agent that lacks the behaviour `defect` names.
    pub fn broken(defect: Defect) -> Self {
        Self {
            defect: Some(defect),
            ..Self::new()
        }
    }

    fn lacks(&self, defect: Defect) -> bool {
        self.defect == Some(defect)
    }

    /// Sends the result of `lease` if the connection is up.
    fn send(&mut self, lease: &str) {
        if self.connected {
            self.sent.push(lease.to_string());
        }
    }
}

impl Default for FakeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHarness for FakeAgent {
    fn fence_window(&self) -> u64 {
        FENCE_WINDOW
    }

    fn now(&self) -> u64 {
        self.now
    }

    fn advance(&mut self, millis: u64) {
        self.now += millis;
        let silent = self.now - self.last_contact >= FENCE_WINDOW;
        if !self.running.is_empty() && silent && !self.lacks(Defect::NeverFences) {
            self.fenced.append(&mut self.running);
        }
    }

    fn start(&mut self, lease: &str, not_after: u64) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        let fresh = self.known.insert(lease.to_string());
        let repeat = !fresh && !self.lacks(Defect::StartRunsTwice);
        let late = not_after < self.now && !self.lacks(Defect::RunsLateStart);
        if !repeat {
            if late {
                self.never_started.insert(lease.to_string());
            } else {
                *self.launches.entry(lease.to_string()).or_insert(0) += 1;
                self.running.insert(lease.to_string());
            }
        }
        Delivery::Delivered
    }

    fn cancel(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        if !self.lacks(Defect::CancelKeepsListed) {
            self.running.remove(lease);
        }
        Delivery::Delivered
    }

    fn ack(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        self.unacked.remove(lease);
        Delivery::Delivered
    }

    fn complete(&mut self, lease: &str) {
        self.running.remove(lease);
        self.unacked.insert(lease.to_string());
        self.send(lease);
    }

    fn contact(&mut self) {
        self.last_contact = self.now;
    }

    fn disconnect(&mut self) {
        self.connected = false;
        if self.lacks(Defect::ForgetsUnackedResult) {
            self.unacked.clear();
        }
    }

    fn reconnect(&mut self) {
        self.connected = true;
        self.sent.extend(self.unacked.iter().cloned());
    }

    fn observed(&self) -> Observed {
        Observed {
            running: self.running.clone(),
            fenced: self.fenced.clone(),
            never_started: self.never_started.clone(),
            launches: self.launches.clone(),
            results_sent: self.sent.clone(),
        }
    }
}
