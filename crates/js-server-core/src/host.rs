//! A host's session with the core: what it says when it registers and on each heartbeat,
//! and how the core takes them.
//!
//! The types are transport-neutral: a host's `Hello` becomes [`Reported`], its heartbeat
//! [`Beat`], and the connection it speaks on is a [`Stream`], a number the core gives it.
//! Nothing here names a wire message. The rules are the scheduler's contract with a host
//! (`js_sched::Event`): a registration opens a session, only the newest stream of a worker
//! is heard, and a `Start` goes to the stream whose heartbeat it counts against.

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, NodePhase};
use js_types::sched::usage::NodeLoad;
use js_types::sched::{Answer, FarmTime, LeaseId, Resources, StateMachine, WorkerId};

use crate::adapter::{Adapter, Link};
use crate::engine::Engine;

/// One connection of a worker to the core. The core numbers them as workers register; a
/// worker's newest stream replaces its earlier one, and a message that names a replaced
/// stream is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stream(u64);

/// What a host says about itself when it registers, and again when its report changes.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Reported {
    /// What placement may book on it.
    pub capacity: Resources,
    /// What it offers: placement gives it only work whose platform it satisfies.
    pub caps: NodeCaps,
    /// What it is doing besides running leases.
    pub phase: NodePhase,
}

impl Reported {
    /// A serving host with `capacity` and `caps`.
    #[must_use]
    pub const fn serving(capacity: Resources, caps: NodeCaps) -> Self {
        Self {
            capacity,
            caps,
            phase: NodePhase::Serving,
        }
    }

    /// This report, with `phase`.
    #[must_use]
    pub fn in_phase(mut self, phase: NodePhase) -> Self {
        self.phase = phase;
        self
    }
}

/// A heartbeat of a host.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Beat {
    /// Its number on the stream, which grows.
    pub seq: u64,
    /// The leases it holds: running, or ended with a result not yet acknowledged.
    pub running: Vec<LeaseId>,
    /// The load it measured, when it sent one.
    pub load: Option<NodeLoad>,
}

impl Beat {
    /// Heartbeat `seq`, listing `running`.
    #[must_use]
    pub const fn new(seq: u64, running: Vec<LeaseId>) -> Self {
        Self {
            seq,
            running,
            load: None,
        }
    }

    /// This heartbeat, carrying `load`.
    #[must_use]
    pub fn with_load(mut self, load: NodeLoad) -> Self {
        self.load = Some(load);
        self
    }
}

/// A stream the core has numbered for a registering worker, and whether the daemon process
/// it replaces is proven stopped.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opened {
    pub(crate) stream: Stream,
    pub(crate) stopped: bool,
}

impl Engine {
    /// Numbers the stream `worker` registers on, and finds whether the daemon process of
    /// its earlier stream is proven stopped: the new daemon names it as its `predecessor`
    /// and the earlier stream has closed (a process still connected may be another
    /// machine's, with a copy of this one's disk).
    pub(crate) fn open_stream(
        &mut self,
        a: &dyn Adapter,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor: &DaemonInstance,
    ) -> Opened {
        let stream = Stream(self.next_stream);
        self.next_stream += 1;
        let replaced = self.links.get(worker);
        let stopped = replaced.is_some_and(|old| {
            old.instance.same_as(predecessor) && !a.stream_open(worker, old.stream)
        });
        if stopped {
            // Its runs were killed: its leases go at once, as farm faults.
            let predecessor = predecessor.as_str();
            tracing::info!(%worker, predecessor, "the daemon restarted and names its predecessor stopped");
        } else if replaced.is_some_and(|old| !old.instance.same_as(instance)) {
            // The earlier process's leases are kept until it has fenced.
            let cause = "the daemon restarted, or two daemons hold this node's certificate";
            tracing::warn!(%worker, cause, "another daemon process registered as this node");
        }
        Opened { stream, stopped }
    }

    /// Registers `worker` on the stream `opened`, with what it `reported`. Its phase is
    /// taken before any placement, and may send a `Start` again or a `Cancel`.
    pub(crate) fn register(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        instance: DaemonInstance,
        opened: Opened,
        reported: Reported,
    ) {
        let link = Link {
            stream: opened.stream,
            instance: instance.clone(),
            newest_beat: 0,
        };
        self.links.insert(worker.clone(), link);
        let event = Event::WorkerUp {
            worker: worker.clone(),
            instance,
            predecessor_stopped: opened.stopped,
            capacity: reported.capacity,
            caps: reported.caps,
        };
        // No placement between the registration and the phase: a quarantined node must
        // not be offered a lease in between.
        let none = self.sched.apply(Input::new(now, event));
        debug_assert!(none.is_empty(), "a registration has no effect: {none:?}");
        self.phase(a, now, worker, reported.phase);
    }

    /// A report resent on the current stream: the host's capacity, capabilities or phase
    /// changed. Opens no session.
    pub(crate) fn resize(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        reported: Reported,
    ) {
        let event = Event::Capacity {
            worker: worker.clone(),
            capacity: reported.capacity,
            caps: reported.caps,
        };
        let none = self.sched.apply(Input::new(now, event));
        debug_assert!(none.is_empty(), "a capacity change has no effect: {none:?}");
        self.phase(a, now, worker, reported.phase);
    }

    /// Takes `worker`'s phase, then places: a switching lease's `Start` is sent again, or
    /// a `Cancel` for one the scheduler does not hold there.
    fn phase(&mut self, a: &mut dyn Adapter, now: FarmTime, worker: &WorkerId, phase: NodePhase) {
        let before = self.sched.phase(worker).cloned().unwrap_or_default();
        a.phased(worker, &before, &phase);
        let switching = phase.switching();
        let event = Event::Phase {
            worker: worker.clone(),
            phase,
        };
        self.feed_quiet(a, now, event);
        let Some(lease) = switching else {
            return;
        };
        if self.sched.not_held(worker, &[lease]).next().is_some() {
            tracing::info!(%worker, %lease, "the node switches its session for a lease not held here: cancelled");
            a.cancel(&self.view(), worker, lease);
        }
    }

    /// Whether `stream` is `worker`'s newest.
    pub(crate) fn is_current(&self, worker: &WorkerId, stream: Stream) -> bool {
        self.view().is_current(worker, stream)
    }

    /// A heartbeat on `stream`: `None` if the stream was replaced, else the answers it
    /// led to (a lease it leaves out that a proven restart lost, past its operation's farm
    /// reruns, finishes the operation). Each lease it lists that the scheduler no longer
    /// holds on `worker` is told to stop.
    pub(crate) fn take_heartbeat(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        stream: Stream,
        beat: Beat,
    ) -> Option<Vec<Answer>> {
        let link = self.links.get_mut(worker).filter(|l| l.stream == stream)?;
        link.newest_beat = link.newest_beat.max(beat.seq);
        let Beat { running, load, .. } = beat;
        let event = Event::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
            load,
        };
        let answers = self.feed(a, now, event);
        let not_held: Vec<LeaseId> = self.sched.not_held(worker, &running).collect();
        for lease in not_held {
            tracing::info!(%worker, %lease, "a lease not held here is listed: cancelled");
            a.cancel(&self.view(), worker, lease);
        }
        Some(answers)
    }
}
