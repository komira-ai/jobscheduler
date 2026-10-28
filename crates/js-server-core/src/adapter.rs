//! What the core asks of the server around it.
//!
//! The core decides when each effect of the scheduler is carried out and tells the
//! [`Adapter`] what to do; the adapter owns everything that is the embedding server's:
//! the messages to a host, the callers waiting on an operation and what they are told, the
//! names an operation is logged by. Its hooks run under the core's lock, so one neither
//! blocks nor calls back into the core: it sends on a channel, changes a map, writes a log
//! line. What must wait for the lock to be released (answering callers, a cache write) is
//! the [`Delivery`] a hook returns.
//!
//! Only ids, the scheduler's own types and the core's types appear in the signatures. A
//! caller's channel, a host's connection and a result's bytes are the adapter's, and reach
//! it through the closures of [`Core::register`](crate::Core::register),
//! [`Core::submit`](crate::Core::submit) and the others, never through the core.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use js_sched::{DaemonInstance, NodePhase, Requeue, Scheduler};
use js_store::Ending;
use js_types::sched::{
    Answer, Effect, LeaseGrant, LeaseId, OperationId, Refusal, StartLease, WaiterId, WorkerId,
};

use crate::host::Stream;

/// Work to do once the core's lock is released: what settling an answer leaves, such as
/// writing a result to a cache and then telling the callers.
pub type Delivery = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What the callers of an operation that has not started are shown.
///
/// The core decides this much; the stage a caller sees once the operation runs, or ends,
/// is the adapter's ([`Adapter::start`], [`Adapter::settle`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Waiting for room.
    Queued,
    /// No live worker can run it now, for this reason. It never started.
    Waiting(String),
    /// As [`Stage::Waiting`], but it has started before, so a drain keeps its streams.
    WaitingKept(String),
}

/// A worker's newest stream.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Link {
    /// The stream.
    pub stream: Stream,
    /// The daemon process that opened it.
    pub instance: DaemonInstance,
    /// The seq of the newest heartbeat taken on the stream; 0 before the first. A `Start`
    /// names it, and a host acts on the `Start` only within the scheduler's start
    /// validity of having sent that heartbeat (or, for 0, its registration).
    pub newest_beat: u64,
}

/// What a hook may read of the core while it runs: the scheduler as it stands, and the
/// newest stream of each worker. A hook cannot change either.
#[derive(Clone, Copy)]
pub struct View<'a> {
    sched: &'a Scheduler,
    links: &'a BTreeMap<WorkerId, Link>,
}

impl<'a> View<'a> {
    pub(crate) const fn new(sched: &'a Scheduler, links: &'a BTreeMap<WorkerId, Link>) -> Self {
        Self { sched, links }
    }

    /// The scheduler.
    #[must_use]
    pub const fn sched(&self) -> &'a Scheduler {
        self.sched
    }

    /// `worker`'s newest stream, if it has registered.
    #[must_use]
    pub fn link(&self, worker: &WorkerId) -> Option<&'a Link> {
        self.links.get(worker)
    }

    /// Whether `stream` is `worker`'s newest.
    #[must_use]
    pub fn is_current(&self, worker: &WorkerId, stream: Stream) -> bool {
        self.link(worker).is_some_and(|link| link.stream == stream)
    }
}

/// Everything the core asks of the server around it. See the module documentation.
pub trait Adapter: Send {
    /// Whether the connection of `worker`'s stream `stream` is still open. Asked when the
    /// worker registers again, to tell a daemon that restarted from one that is still
    /// connected.
    fn stream_open(&self, worker: &WorkerId, stream: Stream) -> bool;

    /// The name of `operation` as the log writes it.
    fn name(&self, view: &View<'_>, operation: OperationId) -> String;

    /// `worker` reported a phase after `before`, with every `Hello` it sends.
    fn phased(&mut self, worker: &WorkerId, before: &NodePhase, after: &NodePhase);

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&mut self, view: &View<'_>, grant: &LeaseGrant);

    /// Sends the `Start` of a committed lease on its worker's newest stream
    /// ([`View::link`] names it and the heartbeat the `Start` counts against), and marks
    /// the callers of the operation executing.
    fn start(&mut self, view: &View<'_>, start: &StartLease);

    /// Tells `worker` to stop the run of `lease`, which the scheduler does not hold there.
    /// A stream that has just ended drops the message; the next heartbeat that lists the
    /// lease asks again.
    fn cancel(&mut self, view: &View<'_>, worker: &WorkerId, lease: LeaseId);

    /// Shows `stage` to the callers of `operation`, if it changes what they see.
    fn show(&mut self, view: &View<'_>, operation: OperationId, stage: &Stage);

    /// Answers the callers of an operation the scheduler refused, and keeps them for the
    /// finished retention.
    fn refuse(&mut self, view: &View<'_>, refusal: &Refusal);

    /// Notes that the scheduler gave up `requeue`'s lease.
    fn requeued(&mut self, requeue: &Requeue);

    /// Settles the answer of a finished operation: works out what its callers are told,
    /// and keeps them until the scheduler drops the operation. `ack` is the worker owed the
    /// acknowledgement of the result that finished it, if the finish had to wait. Returns
    /// what remains to do once the lock is released.
    fn settle(&mut self, view: &View<'_>, answer: &Answer, ack: Option<&WorkerId>) -> Delivery;

    /// Forgets the callers, and their channels, of every finished operation `held` no
    /// longer says the scheduler holds.
    fn forget_dropped(&mut self, held: &dyn Fn(OperationId) -> bool);

    /// Drops what is kept for the report of `lease`, whose answer will never be written.
    fn forget_parcel(&mut self, lease: LeaseId);

    /// Keeps what is kept for the reports whose lease `waits` says still wait for their
    /// answer to be written.
    fn retain_parcels(&mut self, waits: &dyn Fn(LeaseId) -> bool);

    /// How the operation `effect` finishes ended, as its `done/` row keeps it for at least
    /// `retention` (the finished retention): what its callers are told, in the adapter's own
    /// bytes. `None` for an effect that finishes nothing, or one the adapter cannot say how
    /// it ended: the write is then refused as stale, and nothing is carried out for it.
    fn ending(&self, effect: &Effect, retention: Duration) -> Option<Ending>;

    /// The operation `operation`, which the state store holds, was taken up for the caller
    /// `waiter` the core numbered, who no client waits on: a request for the same work
    /// joins it. `parcel` is what the process that queued it kept with it
    /// ([`Core::submit`](crate::Core::submit)), if it kept any.
    fn adopted(&mut self, operation: OperationId, waiter: WaiterId, parcel: Option<&[u8]>);

    /// The finish of `operation` was not written, because the state store holds another
    /// writer's: its callers `waiters` are answered from the store, not by this core.
    fn followed(&mut self, operation: OperationId, waiters: &[WaiterId]);
}
