use std::collections::{BTreeMap, BTreeSet};

use super::{AgentHarness, Delivery, Observed};

/// The fence window [`FakeAgent::new`] uses, in milliseconds.
pub const FENCE_WINDOW: u64 = 30_000;

/// The one behaviour a broken [`FakeAgent`] lacks. Each is the cause of exactly one
/// scenario failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// A repeated Start for a lease launches its job again.
    StartRunsTwice,
    /// A Cancel stops the job but the lease stays listed as running.
    CancelKeepsListed,
    /// A Cancel does not retire the lease id, so a later Start for it runs.
    ForgetsRetiredIds,
    /// A Start that waited longer than its validity is run.
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
    window: Option<u64>,
    now: u64,
    last_contact: u64,
    connected: bool,
    known: BTreeSet<String>,
    retired: BTreeSet<String>,
    running: BTreeSet<String>,
    fenced: BTreeSet<String>,
    never_started: BTreeSet<String>,
    launches: BTreeMap<String, u32>,
    unacked: BTreeSet<String>,
    sent: Vec<String>,
}

impl FakeAgent {
    /// A correct agent with a [`FENCE_WINDOW`] fence, connected, at time 0.
    pub fn new() -> Self {
        Self::with_window(Some(FENCE_WINDOW))
    }

    /// A correct agent whose fence window is `window`, or with `None` an agent that has
    /// no self-fence at all, as an agent whose wire carries none.
    pub fn with_window(window: Option<u64>) -> Self {
        Self {
            defect: None,
            window,
            now: 0,
            last_contact: 0,
            connected: true,
            known: BTreeSet::new(),
            retired: BTreeSet::new(),
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
}

impl Default for FakeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHarness for FakeAgent {
    fn fence_window(&self) -> Option<u64> {
        self.window
    }

    fn now(&self) -> u64 {
        self.now
    }

    fn advance(&mut self, millis: u64) {
        self.now += millis;
        let silence = self.now - self.last_contact;
        let silent = self.window.is_some_and(|window| silence >= window);
        if !self.running.is_empty() && silent && !self.lacks(Defect::NeverFences) {
            self.fenced.append(&mut self.running);
        }
    }

    fn start(&mut self, lease: &str, valid_for: u64, waited: u64) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        let fresh = self.known.insert(lease.to_string());
        let repeat = !fresh && !self.lacks(Defect::StartRunsTwice);
        let bounded = valid_for != 0 && waited > valid_for;
        let late = bounded && !self.lacks(Defect::RunsLateStart);
        let retired = self.retired.contains(lease);
        if !repeat && !retired {
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
        if self.lacks(Defect::ForgetsRetiredIds) {
            self.known.remove(lease);
        } else {
            self.retired.insert(lease.to_string());
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
        self.heartbeat();
    }

    fn heartbeat(&mut self) {
        if self.connected {
            self.sent.extend(self.unacked.iter().cloned());
        }
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
        self.heartbeat();
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
