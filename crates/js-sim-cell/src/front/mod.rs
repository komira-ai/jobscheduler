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
//! dropped, delayed or reordered. A scenario about lost client answers (design section 8,
//! row 1: a retried acquire books a second host) does not belong here.
//!
//! # What a front reports
//!
//! A [`Front`] turns a [`Work`] into calls and reports what it hears as the checker's
//! [`ClientEvent`]s, item by item. Nothing outside a front names a route, a status code or
//! a body: the cell records each event in [`crate::Observations::items`] and the
//! invariants read only those. The cell writes [`ClientEvent::Submitted`] itself when it
//! submits an item.
//!
//! [`LeaseFront`] is the one front here. A front is chosen by a [`FrontFactory`] (the
//! scenario builder takes one; [`LeaseFront::factory`] is the default), so a second front
//! is added beside the first without editing it.

use std::rc::Rc;

use axum::body::Body;
use axum::http::{Request, Response};
use js_types::sched::FarmTime;

use crate::observe::{ClientEvent, ItemId};
use crate::server::ServerSpec;
use crate::work::Work;

mod lease;

pub use lease::{LeaseFront, POLL_EVERY};

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
}

/// Makes the front of each seed's cell for the server it runs.
#[derive(Clone)]
pub struct FrontFactory(Rc<dyn Fn(&ServerSpec) -> Box<dyn Front>>);

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
