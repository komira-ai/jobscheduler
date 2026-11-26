//! Why the load predicate refused a host a request, with the figures it compared.
//!
//! The predicate ([`crate::LoadConfig`]) answers yes or no per host. Here a no carries
//! its kind and the numbers behind it, so a caller can say why a host is not filled.
//! The scheduler keeps the refusals of a round in a bounded buffer
//! ([`crate::Scheduler::take_refusals`]) and counts them by kind in
//! [`crate::LoadCounters`]. The kind [`RefusalKind::CpuCurve`] is the caller's alone: the
//! scheduler makes no CPU test on a worker whose CPU the caller judges.

use js_types::sched::WorkerId;

/// Which inequality of the predicate refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefusalKind {
    /// The CPU side of the load predicate: what the host is charged plus the request's
    /// cores is above the capacity times the CPU slack.
    Cpu,
    /// The memory side of the load predicate: the memory counted on the host plus the
    /// request's is above the capacity times the memory slack.
    PredictedMemory,
    /// The flat memory guard: the sum of the flat memory costs of every lease on the
    /// host plus the request's is above the capacity times one plus the guard's slack.
    BookedMemory,
    /// The cold cap: the cold leases admitted in this window reach the cap.
    ColdCap,
    /// The slot-by-slot CPU check a caller makes on a worker whose CPU it judges
    /// ([`crate::Scheduler::set_cpu_judged`]); the scheduler itself never makes it.
    CpuCurve,
}

impl RefusalKind {
    /// Every kind, in a fixed order.
    pub const ALL: [Self; 5] = [
        Self::Cpu,
        Self::PredictedMemory,
        Self::BookedMemory,
        Self::ColdCap,
        Self::CpuCurve,
    ];

    /// The kind's name as logs and metrics spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::PredictedMemory => "predicted-memory",
            Self::BookedMemory => "booked-memory",
            Self::ColdCap => "cold-cap",
            Self::CpuCurve => "cpu-curve",
        }
    }
}

/// One refusal: the kind and the figures of the inequality that failed
/// (`counted + new > limit`). CPU is in millicores, memory in bytes; for the cold cap
/// `counted` is the cold leases admitted this window, `new` is 1 and `limit` is the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Which inequality failed.
    pub kind: RefusalKind,
    /// What the host is charged before the request: for the CPU kind the larger of the
    /// measurement and the covered leases' charge, plus the other leases' charge.
    pub counted: u64,
    /// What the request adds.
    pub new: u64,
    /// The most the inequality allows.
    pub limit: u64,
    /// The cores the host's newest windows measured demanded (busy and waiting for a CPU,
    /// [`crate::LoadView::measured_cpu_millis`]), in millicores, for the CPU kind on a
    /// host charged by measurement; `None` for a host charged by prediction alone (it has no
    /// fresh reading: none yet, a dropped one, a stale one or a CPU-limited actions cgroup)
    /// and for the kinds other than CPU.
    pub measured_cpu_millis: Option<u64>,
}

impl Refusal {
    pub(crate) const fn new(kind: RefusalKind, counted: u64, new: u64, limit: u64) -> Self {
        Self {
            kind,
            counted,
            new,
            limit,
            measured_cpu_millis: None,
        }
    }

    pub(crate) const fn measured(self, measured_cpu_millis: Option<u64>) -> Self {
        Self {
            measured_cpu_millis,
            ..self
        }
    }
}

/// A refusal of one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRefusal {
    /// The host that was refused the request.
    pub worker: WorkerId,
    /// Why, with the figures.
    pub refusal: Refusal,
}

/// The refusals since the last [`crate::Scheduler::take_refusals`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refusals {
    /// The refusals kept, oldest first; at most [`MAX_KEPT`].
    pub kept: Vec<HostRefusal>,
    /// Refusals that did not fit the buffer and were only counted.
    pub dropped: u64,
}

/// The most refusals the scheduler keeps between two takes.
pub const MAX_KEPT: usize = 1024;

/// The bounded buffer behind [`Refusals`].
#[derive(Clone, Debug, Default)]
pub(crate) struct RefusalLog {
    kept: Vec<HostRefusal>,
    dropped: u64,
}

impl RefusalLog {
    pub(crate) fn push(&mut self, worker: &WorkerId, refusal: Refusal) {
        if self.kept.len() < MAX_KEPT {
            self.kept.push(HostRefusal {
                worker: worker.clone(),
                refusal,
            });
        } else {
            self.dropped += 1;
        }
    }

    pub(crate) fn take(&mut self) -> Refusals {
        Refusals {
            kept: std::mem::take(&mut self.kept),
            dropped: std::mem::take(&mut self.dropped),
        }
    }
}
