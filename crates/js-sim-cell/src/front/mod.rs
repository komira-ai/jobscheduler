//! The client driver: how units of work reach the server and what the client hears back.
//!
//! # A driver, not a node
//!
//! The client is not a node on the kernel's bus. The cell's run loop ([`crate::Cell`])
//! polls it between kernel steps, at the exact virtual time it asks for
//! ([`Front::next_poll`]): after every kernel event and every scripted action of that time.
//! Each call it makes goes to the server's client router at that time through a
//! [`ClientPort`] (the cell's goes through [`crate::ServerNode::client_call`]), and is run
//! to its end before the next step. The bus's faults ([`js_sim::Faults`], partitions)
//! apply to the agent wire only, which is built to be resent; a client call is never
//! dropped, delayed or reordered by the bus. A lost answer is the client's own: a
//! [`crate::JobFront`] made with a [`crate::LostAnswer`] switch reads the answer to its
//! next submit as none once an action throws the switch, and submits again under the same
//! job id (design section 8, row 1).
//!
//! # What a front reports
//!
//! A [`Front`] turns a [`Work`] into calls and reports what it hears as the checker's
//! [`ClientEvent`]s, item by item. Nothing outside a front names a route, a status code or
//! a body: the cell records each event in [`crate::Observations::items`] and the
//! invariants read only those. The cell writes [`ClientEvent::Submitted`] itself when it
//! submits an item.
//!
//! A front also keeps every answer it was given, raw ([`Answer`]: the call, its time, its
//! status and its body), for one purpose only: an expectation that carries what a test
//! of the server's HTTP surface asserted about a status or an error body
//! ([`crate::Expect::answer`]). No invariant reads them.
//!
//! [`LeaseFront`] is the front of the lease routes and [`crate::JobFront`] that of the job
//! service. A front is chosen by a [`FrontFactory`] (the scenario builder takes one;
//! [`LeaseFront::factory`] is the default), so a front is added beside the others without
//! editing them.

use std::rc::Rc;

use axum::body::Body;
use axum::http::{Request, Response};
use js_types::sched::FarmTime;

use crate::observe::{ClientEvent, ItemId};
use crate::server::ServerSpec;
use crate::work::Work;

mod lease;

pub(crate) use lease::listed;
pub use lease::{LeaseFront, POLL_EVERY, REQUEST_PREFIX};

/// What a front's call was for, as its [`Answer`] names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallKind {
    /// The acquire of a lease.
    Acquire,
    /// A read of the lease list, for a pending lease.
    List,
    /// The run queued on a held lease.
    Queue,
    /// A read of the run.
    Read,
    /// The stop of the run, for a cancel.
    Cancel,
    /// The release of the lease.
    Release,
}

impl CallKind {
    /// A stable kebab-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        ["acquire", "list", "queue", "read", "cancel", "release"][self as usize]
    }
}

/// One answer a front was given, as it came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The item the call was made for.
    pub item: ItemId,
    /// What the call was for.
    pub kind: CallKind,
    /// The virtual time of the call.
    pub at: FarmTime,
    /// The status.
    pub status: u16,
    /// The body, as text.
    pub body: String,
}

/// The server's client router, as a front calls it: one request, answered at once.
pub trait ClientPort {
    /// Sends `request` and returns the answer, its body read as text.
    fn call(&mut self, request: Request<Body>) -> Response<String>;
}

/// How a client turns units of work into calls to the server.
///
/// The cell calls [`Front::submit`] and [`Front::cancel`] when a scenario submits or
/// cancels an item, and [`Front::poll`] at the time [`Front::next_poll`] names (never twice
/// at one virtual time). A front makes its calls only in [`Front::poll`].
pub trait Front {
    /// Takes `item`, of `work`, submitted at `now`.
    fn submit(&mut self, item: ItemId, work: &Work, now: FarmTime);

    /// Asks for `item` to be cancelled, at `now`. An item the front does not hold, or
    /// one that has ended, is left as it is.
    fn cancel(&mut self, item: ItemId, now: FarmTime);

    /// When the front wants its next poll, or `None` while it has nothing to do.
    fn next_poll(&self) -> Option<FarmTime>;

    /// Makes the calls due at `now` through `server`, and returns every event heard, in
    /// the order heard.
    fn poll(&mut self, now: FarmTime, server: &mut dyn ClientPort) -> Vec<(ItemId, ClientEvent)>;

    /// Every answer the front was given, in the order given.
    fn answers(&self) -> &[Answer];
}

/// What a [`FrontFactory`] calls: a front for a cell of the server a spec describes.
type MakeFront = dyn Fn(&ServerSpec) -> Box<dyn Front>;

/// Makes the front of each seed's cell for the server it runs.
#[derive(Clone)]
pub struct FrontFactory(Rc<MakeFront>);

impl FrontFactory {
    /// A factory that makes each front with `make`.
    pub fn new(make: impl Fn(&ServerSpec) -> Box<dyn Front> + 'static) -> Self {
        Self(Rc::new(make))
    }

    /// A front for a cell of the server `spec` describes.
    #[must_use]
    pub fn make(&self, spec: &ServerSpec) -> Box<dyn Front> {
        (self.0)(spec)
    }
}
