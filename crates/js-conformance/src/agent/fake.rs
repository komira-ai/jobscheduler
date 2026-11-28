use std::collections::{BTreeMap, BTreeSet};

use super::{AgentHarness, Answer, Cleanup, Delivery, Flaw, HostCheck, Observed};

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
    /// The agent stops its running leases once it has not heard the scheduler for its fence
    /// window.
    StopsRunningOnSilence,
    /// A result not yet acknowledged is forgotten when the connection drops.
    ForgetsUnackedResult,
    /// A start whose fence was retired runs the job again.
    RunsRetiredFence,
    /// A retired fence is kept by the job alone, so the same job under a new allocation is
    /// refused too.
    FenceKeyedByJobAlone,
    /// A result answered with a known verdict other than accepted is kept and sent again.
    KeepsRefusedResult,
    /// A result answered with a verdict the agent does not know is dropped.
    DropsUnrecognisedAnswer,
    /// The result of a job that failed carries no usage.
    DropsUsageOnFailure,
    /// The result of a job killed by a stop carries no usage.
    DropsUsageOnKill,
    /// A start that breaks this rule of the start contract is launched.
    RunsFlawedStart(Flaw),
    /// A run's slot is freed before its container and scratch are gone.
    FreesSlotBeforeCleanup,
    /// A container that could not be removed frees its slot.
    FreesSlotOnFailedRemoval,
    /// A container that could not be removed leaves the host untainted.
    FailedRemovalTaintsNothing,
    /// A start whose job may change the host is launched without the operator's consent.
    RunsHostWithoutConsent,
    /// A job that may change the host taints it only when it ends on its own.
    HostJobTaintsOnlyOnSuccess,
    /// A process of the job's user left after the job taints nothing.
    LeftoverProcessTaintsNothing,
    /// A check that could not run is taken as passed.
    UnrunCheckPasses,
    /// A taint is reported in one heartbeat, acknowledged or not.
    ForgetsUnackedTaint,
    /// The acknowledgement of a taint clears it.
    AckClearsTaint,
    /// A restart forgets the taint.
    ForgetsTaintAtRestart,
    /// A tainted host still launches a start.
    StartsOnTaintedHost,
    /// A restart forgets the runs that kept running.
    ForgetsRunsAtRestart,
    /// A restart forgets the results not yet acknowledged.
    ForgetsResultsAtRestart,
    /// A host held whole is reported ready while a run is held on it.
    ReadyWhileBusy,
    /// A host held whole still launches a run of another lease.
    RunsBesideWholeLease,
}

/// How a running job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// On its own, and passed.
    Completed,
    /// On its own, with a failure.
    Failed,
    /// Stopped by a Cancel.
    Killed,
}

/// A correct in-process agent, or with one [`Defect`] a broken one.
///
/// It models the cleanup, the checks, the taint, a restart and a whole-node hold as far as
/// the scenarios read them: a run's cleanup is done when the run ends, a container that
/// cannot be removed stays and holds its slot, a taint refuses every later run start (not a
/// fenced, host or whole-node one), and a restart keeps the runs, the results and the taint.
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
    /// The allocation each fenced job that runs was started under.
    fences: BTreeMap<String, String>,
    /// The fences of the fenced jobs that ended: a job with its allocation.
    retired_fences: BTreeSet<(String, String)>,
    refused: BTreeSet<String>,
    usage: BTreeSet<String>,
    cleanup: Vec<(String, Cleanup)>,
    tainted: bool,
    taint_acked: bool,
    taint_reports: u32,
    findings: BTreeSet<String>,
    /// The leases whose container cannot be removed.
    stuck: BTreeSet<String>,
    /// What the post-job check finds after each lease that it finds something after.
    checks: BTreeMap<String, HostCheck>,
    /// Whether the operator consented to jobs that may change the host.
    host_jobs: bool,
    /// The running jobs that may change the host.
    host_runs: BTreeSet<String>,
    /// Whether the host is held whole for a lease, and whether it was found ready for it.
    held_whole: bool,
    whole_ready: bool,
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
            fences: BTreeMap::new(),
            retired_fences: BTreeSet::new(),
            refused: BTreeSet::new(),
            usage: BTreeSet::new(),
            cleanup: Vec::new(),
            tainted: false,
            taint_acked: false,
            taint_reports: 0,
            findings: BTreeSet::new(),
            stuck: BTreeSet::new(),
            checks: BTreeMap::new(),
            host_jobs: false,
            host_runs: BTreeSet::new(),
            held_whole: false,
            whole_ready: false,
        }
    }

    /// An agent that lacks the behaviour `defect` names.
    pub fn broken(defect: Defect) -> Self {
        Self {
            defect: Some(defect),
            ..Self::new()
        }
    }

    /// The leases whose result the agent holds and the scheduler has not yet
    /// acknowledged: what its next heartbeat carries. Unlike [`Observed::results_sent`],
    /// which lists every transmission so far, this is the current set.
    ///
    /// [`Observed::results_sent`]: super::Observed::results_sent
    pub fn unacked(&self) -> &BTreeSet<String> {
        &self.unacked
    }

    fn lacks(&self, defect: Defect) -> bool {
        self.defect == Some(defect)
    }

    /// Launches the job of `lease` and lists it as running.
    fn launch(&mut self, lease: &str) {
        *self.launches.entry(lease.to_string()).or_insert(0) += 1;
        self.running.insert(lease.to_string());
    }

    /// The job of `lease` ends as `ending` says: its usage is read and its cleanup done.
    fn end(&mut self, lease: &str, ending: Ending) {
        self.running.remove(lease);
        let drops_usage = match ending {
            Ending::Completed => false,
            Ending::Failed => self.lacks(Defect::DropsUsageOnFailure),
            Ending::Killed => self.lacks(Defect::DropsUsageOnKill),
        };
        if !drops_usage {
            self.usage.insert(lease.to_string());
        }
        // A host job taints the host at its start; the broken agent only once it passed.
        if self.host_runs.remove(lease) & (ending == Ending::Completed) {
            self.tainted = true;
        }
        self.clean_up(lease);
    }

    /// Removes the container and scratch of `lease`, runs the post-job check and frees the
    /// slot, reporting each step; a container that cannot be removed stops the cleanup there
    /// and taints the host, and a check that finds something taints it and keeps the slot.
    fn clean_up(&mut self, lease: &str) {
        let steps = match self.stuck.contains(lease) {
            true => self.stuck_removal(),
            false => self.removal(lease),
        };
        let steps = steps.into_iter().map(|step| (lease.to_string(), step));
        self.cleanup.extend(steps);
    }

    /// The cleanup steps of a run whose container cannot be removed.
    fn stuck_removal(&mut self) -> Vec<Cleanup> {
        self.tainted |= !self.lacks(Defect::FailedRemovalTaintsNothing);
        match self.lacks(Defect::FreesSlotOnFailedRemoval) {
            true => vec![Cleanup::SlotFreed],
            false => Vec::new(),
        }
    }

    /// The cleanup steps of a run whose container is removed, after its check.
    fn removal(&mut self, lease: &str) -> Vec<Cleanup> {
        let found = match self.checks.get(lease) {
            None => false,
            Some(HostCheck::LeftoverProcess) => !self.lacks(Defect::LeftoverProcessTaintsNothing),
            Some(HostCheck::CannotRun) => !self.lacks(Defect::UnrunCheckPasses),
        };
        if found {
            self.tainted = true;
            self.findings.insert(lease.to_string());
        }
        let early = self.lacks(Defect::FreesSlotBeforeCleanup);
        let (gone, scratch, freed) = (
            Cleanup::ContainerGone,
            Cleanup::ScratchGone,
            Cleanup::SlotFreed,
        );
        match (found, early) {
            (true, _) => vec![gone, scratch],
            (false, false) => vec![gone, scratch, freed],
            (false, true) => vec![freed, gone, scratch],
        }
    }

    /// A job of `lease` ended on its own as `ending` says: its result is held and sent.
    fn finish(&mut self, lease: &str, ending: Ending) {
        self.end(lease, ending);
        if let Some(allocation) = self.fences.remove(lease) {
            self.retired_fences.insert((lease.to_string(), allocation));
        }
        self.unacked.insert(lease.to_string());
        self.heartbeat();
    }

    /// Puts the taint in the heartbeat being sent, unless it was acknowledged.
    fn report_taint(&mut self) {
        if self.tainted & !self.taint_acked {
            self.taint_reports += 1;
            self.taint_acked |= self.lacks(Defect::ForgetsUnackedTaint);
        }
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
        if !self.running.is_empty() && silent && self.lacks(Defect::StopsRunningOnSilence) {
            self.fenced.append(&mut self.running);
        }
    }

    fn start(&mut self, lease: &str, valid_for: u64, waited: u64) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        let tainted = self.tainted & !self.lacks(Defect::StartsOnTaintedHost);
        let beside = self.held_whole & !self.lacks(Defect::RunsBesideWholeLease);
        if tainted | beside {
            self.refused.insert(lease.to_string());
            return Delivery::Delivered;
        }
        let fresh = self.known.insert(lease.to_string());
        let repeat = !fresh && !self.lacks(Defect::StartRunsTwice);
        let bounded = valid_for != 0 && waited > valid_for;
        let late = bounded && !self.lacks(Defect::RunsLateStart);
        let retired = self.retired.contains(lease);
        if !repeat && !retired {
            if late {
                self.never_started.insert(lease.to_string());
            } else {
                self.launch(lease);
            }
        }
        Delivery::Delivered
    }

    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        let fence = (job.to_string(), allocation.to_string());
        let fenced = match self.lacks(Defect::FenceKeyedByJobAlone) {
            true => self
                .retired_fences
                .iter()
                .any(|(retired, _)| retired == job),
            false => self.retired_fences.contains(&fence),
        };
        let refused = fenced && !self.lacks(Defect::RunsRetiredFence);
        if !refused && !self.running.contains(job) {
            self.launch(job);
            self.fences.insert(fence.0, fence.1);
        }
        Delivery::Delivered
    }

    fn cancel(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        if self.running.contains(lease) {
            self.end(lease, Ending::Killed);
            if self.lacks(Defect::CancelKeepsListed) {
                self.running.insert(lease.to_string());
            }
        }
        if self.lacks(Defect::ForgetsRetiredIds) {
            self.known.remove(lease);
        } else {
            self.retired.insert(lease.to_string());
        }
        Delivery::Delivered
    }

    fn answer(&mut self, lease: &str, answer: Answer) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        let drops = match answer {
            Answer::Accepted => true,
            Answer::Unrecognised => self.lacks(Defect::DropsUnrecognisedAnswer),
            _ => !self.lacks(Defect::KeepsRefusedResult),
        };
        if drops {
            self.unacked.remove(lease);
        }
        Delivery::Delivered
    }

    fn complete(&mut self, lease: &str) {
        self.finish(lease, Ending::Completed);
    }

    fn fail(&mut self, lease: &str) {
        self.finish(lease, Ending::Failed);
    }

    fn start_flawed(&mut self, lease: &str, flaw: Flaw) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        match self.lacks(Defect::RunsFlawedStart(flaw)) {
            true => self.launch(lease),
            false => {
                self.refused.insert(lease.to_string());
            }
        }
        Delivery::Delivered
    }

    fn remove_fails(&mut self, lease: &str) {
        self.stuck.insert(lease.to_string());
    }

    fn check_finds(&mut self, lease: &str, check: HostCheck) {
        self.checks.insert(lease.to_string(), check);
    }

    fn consent_to_host_jobs(&mut self) {
        self.host_jobs = true;
    }

    fn start_host(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        match self.host_jobs | self.lacks(Defect::RunsHostWithoutConsent) {
            true => {
                self.launch(lease);
                self.host_runs.insert(lease.to_string());
                self.tainted |= !self.lacks(Defect::HostJobTaintsOnlyOnSuccess);
            }
            false => {
                self.refused.insert(lease.to_string());
            }
        }
        Delivery::Delivered
    }

    fn ack_taint(&mut self) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        self.taint_acked = true;
        self.tainted &= !self.lacks(Defect::AckClearsTaint);
        Delivery::Delivered
    }

    fn restart(&mut self) {
        if self.lacks(Defect::ForgetsRunsAtRestart) {
            self.running.clear();
        }
        if self.lacks(Defect::ForgetsResultsAtRestart) {
            self.unacked.clear();
        }
        self.tainted &= !self.lacks(Defect::ForgetsTaintAtRestart);
        self.taint_acked = false;
        self.connected = true;
        self.heartbeat();
    }

    fn hold_whole(&mut self, _lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        self.held_whole = true;
        Delivery::Delivered
    }

    fn whole_ready(&mut self) -> bool {
        let quiet = self.running.len() + self.unacked.len() == 0;
        let ready = quiet | self.lacks(Defect::ReadyWhileBusy);
        self.whole_ready |= self.held_whole & ready;
        self.whole_ready
    }

    fn start_whole(&mut self, lease: &str) -> Delivery {
        if !self.connected {
            return Delivery::Lost;
        }
        self.last_contact = self.now;
        match self.whole_ready {
            true => self.launch(lease),
            false => {
                self.refused.insert(lease.to_string());
            }
        }
        Delivery::Delivered
    }

    fn heartbeat(&mut self) {
        if self.connected {
            self.sent.extend(self.unacked.iter().cloned());
            self.report_taint();
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
            refused: self.refused.clone(),
            usage: self.usage.clone(),
            cleanup: self.cleanup.clone(),
            tainted: self.tainted,
            taint_reports: self.taint_reports,
            findings: self.findings.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `unacked` names a result from the moment its job ends until it is
    /// acknowledged, and nothing else; and that a broken agent that forgets its results
    /// when the connection drops has an empty set after the drop while a correct one keeps
    /// it. It catches an accessor that reads the transmissions instead of the current set.
    #[test]
    fn unacked_lists_the_results_not_yet_acknowledged() {
        let held = |names: &[&str]| -> BTreeSet<String> {
            names.iter().map(|name| (*name).to_string()).collect()
        };
        let mut agent = FakeAgent::new();
        agent.start("a", 0, 0);
        agent.start("b", 0, 0);
        assert_eq!(agent.unacked(), &held(&[]));
        agent.complete("a");
        agent.complete("b");
        assert_eq!(agent.unacked(), &held(&["a", "b"]));
        agent.ack("a");
        agent.heartbeat();
        assert_eq!(agent.unacked(), &held(&["b"]));
        assert_eq!(agent.observed().results_sent, ["a", "a", "b", "b"]);
        agent.disconnect();
        assert_eq!(agent.unacked(), &held(&["b"]));

        let mut broken = FakeAgent::broken(Defect::ForgetsUnackedResult);
        broken.start("a", 0, 0);
        broken.complete("a");
        assert_eq!(broken.unacked(), &held(&["a"]));
        broken.disconnect();
        assert_eq!(broken.unacked(), &held(&[]));
    }
}
