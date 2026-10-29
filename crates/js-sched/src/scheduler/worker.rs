//! A registered worker as the scheduler holds it: its capacity and bookings, what it
//! offers, and the bookkeeping of its sessions.

use std::collections::BTreeSet;

use js_caps::NodeCaps;
use js_types::sched::{FarmTime, LeaseId, LeaseKind, NodePhase, Resources};

use crate::admission::{Left, WorkerLoad};
use crate::fence::LEASE_GRACE;
use crate::input::{DaemonInstance, Request};
use crate::load::Load;

#[derive(Clone, Debug)]
pub(crate) struct Worker {
    pub(crate) capacity: Resources,
    /// The memory the worker reported, before the cut to the share admission books
    /// ([`crate::MemoryPercent`]). Only the plausibility of a reading is judged against it:
    /// a working set is measured on the whole machine.
    pub(crate) raw_memory_bytes: u64,
    pub(crate) caps: NodeCaps,
    pub(crate) booked: Resources,
    /// How many leases it holds.
    pub(crate) leases: usize,
    /// A whole-machine lease holds it: nothing else is placed on it.
    pub(crate) whole: bool,
    /// The iOS devices its leases booked, by id. A booking lasts until its lease ends,
    /// whatever the worker's reports say meanwhile ([`crate::devices`]).
    pub(crate) devices: BTreeSet<String>,
    /// The readings it sent and the cost of each lease on it ([`crate::admission`]).
    pub(crate) load_state: WorkerLoad,
    pub(super) last_heard: FarmTime,
    /// Counts the worker's registrations: the session a `Start` emitted now goes to.
    pub(super) session: u64,
    /// The daemon process of the current session.
    pub(super) instance: DaemonInstance,
    /// Counts the registrations that changed the daemon process: the process a `Start`
    /// emitted now goes to.
    pub(super) process: u64,
    /// Until when leases whose `Start` went to an earlier process are kept: by then that
    /// process has fenced ([`HANDOVER_GRACE`] after it was last heard).
    pub(super) handover_ends: FarmTime,
    /// The earlier process the newest change of process was proven to replace: its
    /// successor's drivers killed its runs before registering, so its leases the worker
    /// leaves out go at once ([`Event::WorkerUp`]'s `predecessor_stopped`).
    pub(super) stopped_process: Option<u64>,
    /// What its newest `Hello` says it is doing.
    pub(crate) phase: NodePhase,
    /// Until when the lease of a `Preparing` or `Restoring` phase is kept while the
    /// worker is silent ([`crate::fence::SESSION_SWITCH_GRACE`] after that phase was
    /// first reported).
    pub(super) switch_ends: FarmTime,
}

impl Worker {
    pub(crate) fn free(&self) -> Resources {
        self.capacity.saturating_sub(self.booked)
    }

    /// How loaded it is by what is booked: what placement ranks the workers that have
    /// room by when the load is not read.
    pub(crate) fn load(&self) -> Load {
        Load::new(self.booked, self.capacity, self.leases)
    }

    /// How loaded it is by what the predicate charges it before a new lease (measured
    /// and predicted): what placement ranks by when the load is read.
    pub(crate) fn load_charged(&self, left: &Left) -> Load {
        Load::new(
            Resources::new(left.cpu, left.mem),
            self.capacity,
            self.leases,
        )
    }

    pub(crate) fn alive(&self, now: FarmTime) -> bool {
        now < self.last_heard.saturating_add(LEASE_GRACE)
    }

    /// Whether `lease`, which it holds, is kept at `now` although it is silent: it is
    /// switching the GUI session for that lease, within the switch grace.
    pub(super) fn switching(&self, lease: LeaseId, now: FarmTime) -> bool {
        self.phase.switching() == Some(lease) && now < self.switch_ends
    }

    /// Whether it holds fewer leases than its lease cap; a node that reports no cap
    /// always does.
    pub(super) fn under_lease_cap(&self) -> bool {
        self.capacity
            .leases
            .is_none_or(|cap| u64::try_from(self.leases).is_ok_and(|held| held < cap))
    }

    /// Whether `request`, an action or a VM, is at the whole host's memory (the last step
    /// of the memory ladder, which the cap is): it runs with no other lease on the node,
    /// and none is placed beside it, as a whole-machine lease does. A request for no
    /// memory is not.
    pub(crate) fn exclusive(&self, request: &Request) -> bool {
        request.resources.memory_bytes > 0
            && request.resources.memory_bytes >= self.capacity.memory_bytes
    }

    /// Whether `request` can be placed on it now, platform and kind aside: an action
    /// if no whole-machine lease holds it, its free CPUs, GPUs and VM slots hold the
    /// request, it holds fewer leases (of any kind) than its lease cap (if it reports
    /// one), and its memory capacity is at least the request's memory; a VM if no
    /// whole-machine lease holds it, it holds fewer leases than its cap, and its free CPUs,
    /// memory, GPUs and VM slots hold the request; a whole-machine lease if it holds no
    /// lease and its capacity holds the request. A
    /// request for an iOS device also needs a free device that satisfies it
    /// ([`Worker::free_device`]).
    ///
    /// With `by_load`, an action's CPU is not checked against what is booked: the load
    /// predicate decides it ([`crate::admission`]), and only its GPUs and VM slots are.
    pub(crate) fn has_room(&self, request: &Request, by_load: bool) -> bool {
        let room = match request.kind {
            LeaseKind::WholeMachine => self.leases == 0 && self.capacity.fits(&request.resources),
            // An action's memory is the cgroup limit it first runs under, not a booking:
            // what other leases hold of the node's memory never keeps it out. The node
            // bounds how many run at once by CPUs (its lease count), and its memory
            // pressure backstop pauses starts. The limit must still be one the node can
            // give. The one limit that is not a share is the whole host ([`Worker::exclusive`]):
            // that action runs alone.
            LeaseKind::Action => {
                // Under the load predicate the booked CPUs are not summed, but the
                // request must still be one the worker's CPUs could ever hold.
                let needs = if by_load {
                    Resources {
                        cpu_millis: 0,
                        ..request.resources.without_memory()
                    }
                } else {
                    request.resources.without_memory()
                };
                !self.whole
                    && self.free().fits(&needs)
                    && request.resources.cpu_millis <= self.capacity.cpu_millis
                    && request.resources.memory_bytes <= self.capacity.memory_bytes
                    && self.under_lease_cap()
                    && (!self.exclusive(request) || self.leases == 0)
            }
            // A VM's memory is a booking: the guest holds it whole for as long as it runs,
            // so what other leases hold of the node's memory keeps it out: a VM is placed
            // only if its full request, memory included, fits the node's free capacity.
            // Actions are not checked against memory, so a VM can wait behind a stream of
            // them; nothing reserves a node for a queued VM.
            LeaseKind::Vm => {
                !self.whole
                    && self.free().fits(&request.resources)
                    && self.under_lease_cap()
                    && (!self.exclusive(request) || self.leases == 0)
            }
        };
        room && (!request.needs.wants_device() || self.free_device(&request.needs).is_some())
    }
}
