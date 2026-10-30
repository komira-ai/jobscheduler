//! Why no live worker can run a queued request, as a value.
//!
//! [`Scheduler::unservable`](crate::Scheduler::unservable) and
//! [`WholeState::Unservable`](crate::WholeState::Unservable) carry an [`Unservable`]. An
//! embedder can match on its variants to count or group waiting work by cause. Its
//! [`Display`](std::fmt::Display) is the sentence callers are told
//! ([`Scheduler::waiting`](crate::Scheduler::waiting)): show that to people, and match on
//! the variant for anything else.

use std::fmt;

use js_types::sched::{LeaseKind, NodePhase, Resources, WorkerId};

/// Why no live worker can run a request, ignoring what is booked.
///
/// Every variant but [`Self::Cordoned`] is a reason the request is refused for if it
/// lasts [`UNSERVABLE_WAIT`](crate::UNSERVABLE_WAIT). [`Self::Cordoned`] is temporary by
/// intent: the work waits for the cordon to end and is never refused for it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unservable {
    /// No worker is connected.
    NoWorker,
    /// Workers are connected but every one is cordoned.
    EveryWorkerCordoned,
    /// Workers are connected but every one is cordoned or not serving (switching its GUI
    /// session, or quarantined).
    EveryWorkerHeldBack,
    /// No live worker reports a driver that serves the lease kind.
    NoDriver {
        /// The live workers, none of which serves `kind`.
        live: usize,
        /// The lease kind asked for.
        kind: LeaseKind,
    },
    /// No live worker that serves the lease kind satisfies the request's platform.
    PlatformMismatch {
        /// The live workers that serve `kind`.
        serving: usize,
        /// Whether some live workers do not serve `kind`: `serving` is then fewer than
        /// the live workers.
        some_cannot_serve: bool,
        /// The lease kind asked for.
        kind: LeaseKind,
        /// The worker lacking the fewest requirements (the first by name on a tie).
        closest: WorkerId,
        /// The requirements `closest` does not meet, as the platform match words them.
        lacks: Vec<String>,
    },
    /// The live workers that satisfy the platform are all smaller than the request.
    TooLarge {
        /// How many live workers satisfy the platform.
        matching: usize,
        /// What the request asked for.
        asked: Resources,
    },
    /// Every live worker that could run it is cordoned or not serving.
    Cordoned {
        /// Those workers, in name order, with their phase.
        workers: Vec<(WorkerId, NodePhase)>,
    },
}

impl fmt::Display for Unservable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorker => f.write_str("no worker is connected"),
            Self::EveryWorkerCordoned => f.write_str("every connected worker is cordoned"),
            Self::EveryWorkerHeldBack => {
                f.write_str("every connected worker is cordoned or not serving")
            }
            Self::NoDriver { live, kind } => write!(
                f,
                "none of the {live} live worker(s) serves lease kind {kind}: that needs a \
                 driver among {}",
                kind.drivers().join(", ")
            ),
            Self::PlatformMismatch {
                serving,
                some_cannot_serve,
                kind,
                closest,
                lacks,
            } => {
                let named = if *some_cannot_serve {
                    format!("{serving} live worker(s) serving lease kind {kind}")
                } else {
                    format!("{serving} live worker(s)")
                };
                write!(
                    f,
                    "none of the {named} satisfies the action's platform; the closest, \
                     {closest}, lacks {}",
                    lacks.join(", ")
                )
            }
            Self::TooLarge { matching, asked } => {
                let disk = match asked.disk_bytes {
                    0 => String::new(),
                    bytes => format!(", {bytes} bytes of disk"),
                };
                write!(
                    f,
                    "the {matching} live worker(s) that satisfy the action's platform are all \
                     smaller than its request ({} millicores, {} bytes of memory, {} GPU(s), \
                     {} VM slot(s){disk})",
                    asked.cpu_millis, asked.memory_bytes, asked.gpus, asked.vms
                )
            }
            Self::Cordoned { workers } => {
                let held = if workers
                    .iter()
                    .all(|(_, phase)| *phase == NodePhase::Serving)
                {
                    "cordoned"
                } else {
                    "cordoned or not serving"
                };
                let named: Vec<String> = workers
                    .iter()
                    .map(|(name, phase)| match phase {
                        NodePhase::Serving => name.as_str().to_owned(),
                        phase => format!("{name} ({})", phase.name()),
                    })
                    .collect();
                write!(
                    f,
                    "every live worker that can run it is {held}: {}",
                    named.join(", ")
                )
            }
        }
    }
}
