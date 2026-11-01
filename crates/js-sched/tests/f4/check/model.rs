//! The shadow's records of the workers, leases and operations: plain data the checker
//! keeps and updates as it feeds the scheduler.

use std::collections::{BTreeMap, BTreeSet};

use js_caps::NodeCaps;
use js_sched::fence::LEASE_GRACE;
use js_sched::{DaemonInstance, OpState, Request};
use js_types::sched::{FarmRun, LeaseId, OperationId, Resources, WaiterId, WorkKey, WorkerId};

use super::ms;
use crate::f4::reference::Verdict;

#[derive(Clone, Debug)]
pub(super) struct Worker {
    pub(super) capacity: Resources,
    pub(super) caps: NodeCaps,
    pub(super) booked: Resources,
    /// A lease holds the node alone: an action at its whole memory.
    pub(super) whole: bool,
    pub(super) last_heard: u64,
    pub(super) session: u64,
    /// The daemon process of the current session, how many times it changed, and until
    /// when the leases of an earlier one are kept.
    pub(super) instance: DaemonInstance,
    pub(super) process: u64,
    pub(super) handover_ends: u64,
    pub(super) leases: BTreeSet<LeaseId>,
    /// Each lease it holds, with the action key and the cores it was charged when placed
    /// (the load rule's ledger).
    pub(super) charged: BTreeMap<LeaseId, (WorkKey, u64)>,
}

impl Worker {
    pub(super) fn alive(&self, now: u64) -> bool {
        now < self.last_heard + ms(LEASE_GRACE)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Lease {
    pub(super) op: OperationId,
    pub(super) worker: WorkerId,
    pub(super) committed: bool,
    pub(super) running: bool,
    /// When its `Start` was emitted, and to which session and daemon process.
    pub(super) sent: Option<(u64, u64, u64)>,
}

/// A queued operation's unbroken wait with no worker able to run it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Wait {
    pub(super) since: u64,
    pub(super) verdict: Verdict,
}

#[derive(Clone, Debug)]
pub(super) struct Op {
    pub(super) request: Request,
    pub(super) needs: usize,
    pub(super) waiters: Vec<WaiterId>,
    /// `None` while queued (or waiting for its refusal to commit).
    pub(super) holding: Option<LeaseId>,
    pub(super) done: Option<OpState>,
    pub(super) newest: Option<LeaseId>,
    pub(super) proposed: bool,
    /// Its result was proposed by a give-up past the farm budget, not by a worker.
    pub(super) silenced: bool,
    /// Its sent leases given up for silence or replacement: the farm runs it was
    /// charged (the workers placement avoids while another fits).
    pub(super) farm_runs: Vec<FarmRun>,
    /// Its leases given up as not started (the node heartbeats but never listed them):
    /// their own budget, charged nothing to the farm's.
    pub(super) not_started: u32,
    /// The workers placement avoids while another fits: those of its farm runs and of
    /// its not-started give-ups.
    pub(super) avoid: BTreeSet<WorkerId>,
    pub(super) wait: Option<Wait>,
    /// The last reason its callers were told, while it waits.
    pub(super) told: Option<String>,
    /// Finished, and dropped once the finished retention was up (I16).
    pub(super) dropped: bool,
}
