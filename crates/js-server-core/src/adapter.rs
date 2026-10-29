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

use js_sched::{DaemonInstance, NodePhase, Prediction, Request, Requeue, Scheduler};
use js_store::Ending;
use js_types::sched::{
    Answer, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, Refusal, StartLease, WaiterId,
    WorkerId,
};

use crate::host::Stream;
use crate::report::{Report, Taken};

/// Work to do once the core's lock is released: what settling an answer leaves, such as
/// writing a result to a cache and then telling the callers.
pub type Delivery = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What the callers of an operation that has not started are shown.
///
/// The core decides this much of what a caller of an operation this core holds is shown;
/// the stage once it runs or ends here is the adapter's ([`Adapter::start`],
/// [`Adapter::settle`]). The callers of an operation another frontend runs are shown
/// [`Stage::Executing`] and [`Stage::Done`], which the core reads from the state store.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Waiting for room.
    Queued,
    /// No live worker can run it now, for this reason. It never started.
    Waiting(String),
    /// As [`Stage::Waiting`], but it has started before, so a drain keeps its streams.
    WaitingKept(String),
    /// Running on a worker of another frontend, as the state store says
    /// ([`Core::intake`](crate::Core::intake), [`Core::follow`](crate::Core::follow)).
    Executing,
    /// Ended, as the state store says: the value of the operation's `done/` row, which is
    /// the adapter's own bytes ([`Adapter::ending`]).
    Done(Vec<u8>),
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

/// A key floor the adapter's estimator has decided to lower ([`Adapter::lowered`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Lowered {
    /// The size key whose floor is lowered.
    pub size_key: u64,
    /// The floor the estimator saw before the decay, in bytes: a scheduler floor above it was
    /// raised by a kill the estimator never saw, and is left alone.
    pub before: u64,
    /// The floor asked, in bytes; 0 forgets the floor.
    pub ask: u64,
}

impl Lowered {
    /// The floor of `size_key` lowered from `before` to `ask` bytes.
    #[must_use]
    pub const fn new(size_key: u64, before: u64, ask: u64) -> Self {
        Self {
            size_key,
            before,
            ask,
        }
    }
}

/// Everything the core asks of the server around it. See the module documentation.
pub trait Adapter: Send {
    /// The farm time of the feed about to start, called at the top of every feed that is
    /// carried out (a fenced core feeds nothing, so it is not called), before the scheduler
    /// sees the input and before any other hook of the feed. An adapter that keeps a
    /// clock for work it does later reads it here. Does nothing by default.
    fn clock(&mut self, _now: FarmTime) {}

    /// `worker`'s `report` is about to be fed, under the same hold of the lock as the feed
    /// and its [`Adapter::after_report`], so no tick or heartbeat can finish the operation
    /// between them. The `view` shows the operation as it stands before the feed: still
    /// open, with its waiters, which a report that finishes it hands to the answer and
    /// drops. `now` is the report's farm time; [`Adapter::clock`] is told it by the feed
    /// that follows, so an adapter's own clock may still hold the previous feed's time
    /// here. Called once for each [`Core::report`](crate::Core::report), whatever becomes
    /// of the report: a stale or duplicate one (that a [`Core::standing`](crate::Core::standing)
    /// check let through before a concurrent change) is seen here too, and is
    /// [`Taken::Refused`] or [`Taken::Dropped`] in `after_report`. A report that `standing`
    /// refused is not passed to `Core::report` by the host, and calls neither. A fenced core
    /// ([`Core::fence`](crate::Core::fence)) is hooked too, with no [`Adapter::clock`] call
    /// before it or between the hooks: it feeds nothing, so `after_report` gets no answers
    /// and `Taken::Refused(Reason::Stale)`. Does nothing by default.
    fn before_report(
        &mut self,
        _view: &View<'_>,
        _now: FarmTime,
        _worker: &WorkerId,
        _report: &Report,
    ) {
    }

    /// `worker`'s `report` was fed, and `taken` says how the scheduler took it; `answers`
    /// are the operations it finished (this one, or others a held answer released), not
    /// yet settled: [`Adapter::settle`] follows for each. `now` is the same farm time
    /// `before_report` was given. On a fenced core nothing was fed, `answers` is empty and
    /// `taken` is `Taken::Refused(Reason::Stale)`. Called once for each
    /// [`Core::report`](crate::Core::report), after the matching
    /// [`Adapter::before_report`]. Does nothing by default.
    fn after_report(
        &mut self,
        _view: &View<'_>,
        _now: FarmTime,
        _worker: &WorkerId,
        _report: &Report,
        _answers: &[Answer],
        _taken: Taken,
    ) {
    }

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

    /// The name of the size summary the key floor of `size_key` decays with, as the
    /// `keyfloor/` row keeps it, if the adapter has one. Asked whenever a key floor is
    /// written, with the scheduler's floor already raised or lowered. Does nothing (no name)
    /// by default.
    fn summary_name(&self, _size_key: u64) -> Option<String> {
        None
    }

    /// The start read the size summary each stored key floor decays with, as `(size key,
    /// name)` pairs; told once, from [`Core::open`](crate::Core::open), and only when size
    /// keys are on ([`Config::with_size_keys`](crate::Config::with_size_keys)). A key whose
    /// operations have all ended still has a floor to decay. Does nothing by default.
    fn key_names(&mut self, _names: &[(u64, String)]) {}

    /// The scheduler gave up `requeue`'s lease, before the store is written: the adapter
    /// may learn from the attempt. For a kill for memory, the scheduler's key floor of the
    /// operation is already raised ([`View::sched`]). Whatever floors the estimator then
    /// wants lowered it names in [`Adapter::lowered`], which the core asks for right after.
    /// No I/O: the hook runs under the lock. Does nothing by default.
    fn learned_requeue(&mut self, _view: &View<'_>, _requeue: &Requeue) {}

    /// The key floors the adapter's estimator has decided to lower since the last call;
    /// asked after [`Adapter::learned_requeue`] and after [`Adapter::after_report`]. The core
    /// lowers each in the scheduler (unless a kill raised it above `before`) and writes the
    /// row, again at a later tick if the store refuses it. Nothing by default.
    fn lowered(&mut self) -> Vec<Lowered> {
        Vec::new()
    }

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
    /// ([`Core::submit`](crate::Core::submit)), if it kept any. `request`, `lease` and
    /// `size_key` are what the store holds of the operation, which the core has already
    /// decoded: what it runs, the lease it holds (none for one that waits for a lease) and
    /// its size key (none when it has none, or when size keys are not read). An adapter
    /// needs no read of the store for them.
    fn adopted(
        &mut self,
        operation: OperationId,
        waiter: WaiterId,
        parcel: Option<&[u8]>,
        request: &Request,
        lease: Option<&LeaseGrant>,
        size_key: Option<u64>,
    );

    /// The cost prediction for the queued `operation`, which the core takes up from the state
    /// store: at the start, from another frontend's queue at an intake poll, or when a
    /// request joins it. The scheduler charges it the prediction at its next placement round.
    /// `None` is the booked cost: the request's cores, no memory. `request` and `size_key`
    /// are as for [`Adapter::adopted`].
    ///
    /// Runs under the core lock, after `adopted`: every other submit, report and tick waits
    /// for it. It must not read the store, do I/O, block, or take any other lock: the server
    /// predicts outside the lock (as for [`Core::submit_with`](crate::Core::submit_with))
    /// and answers here from what it has kept. The type does not enforce this: the adapter
    /// is a trait, free to hold a store handle or a mutex, and one that does stalls the
    /// whole core.
    ///
    /// The prediction is not stored with the operation: the stored row is the same with or
    /// without one. An operation that holds a lease keeps the cost it was charged, and is
    /// not asked about.
    fn predicted(
        &mut self,
        _operation: OperationId,
        _request: &Request,
        _size_key: Option<u64>,
    ) -> Option<Prediction> {
        None
    }

    /// The callers among `callers` still waiting: not answered, and still listening (a caller
    /// that gave up is not).
    fn waiting_among(&self, callers: &[WaiterId]) -> Vec<WaiterId>;

    /// Shows `stage` to `callers`, waking whoever watches them only if it changed. The
    /// callers of an operation another frontend took are shown [`Stage::Executing`] while
    /// it holds a lease, and [`Stage::Done`] once its result is in the store.
    fn show_callers(&mut self, callers: &[WaiterId], stage: &Stage);

    /// Keeps `callers` of the finished `operation` until the core says it no longer holds the
    /// operation ([`Adapter::forget_dropped`]): the operation was finished by another
    /// frontend, and its retention starts when the core saw it end.
    fn keep_finished(&mut self, operation: OperationId, callers: Vec<WaiterId>);

    /// The operation `operation`, which another frontend ran, ended; `ended` is the value of
    /// its `done/` row. Called once, as its callers are shown [`Stage::Done`].
    fn followed(&mut self, operation: OperationId, ended: &[u8]);

    /// The operations clients ask for by name that the core does not hold, whose channels
    /// someone still watches and which have not been shown a result.
    fn watched(&mut self) -> Vec<OperationId>;

    /// Shows `stage` to the watchers of `operation`, one [`Adapter::watched`] named.
    fn show_watched(&mut self, operation: OperationId, stage: &Stage);
}
