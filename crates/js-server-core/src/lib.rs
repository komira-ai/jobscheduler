//! The server's core: the loop around [`js_sched::Scheduler`] that carries its effects out,
//! and the session a host has with it.
//!
//! The scheduler is a pure state machine: it takes an input and returns effects (commit this
//! record, send this `Start`, answer these callers). What makes a server correct, and the
//! machine cannot see, is the loop that carries those effects out:
//!
//! - a record is committed, and fed back as committed, before anything that depends on it
//!   happens; the `Start` of a lease and the answer of a result are held until the rows
//!   they settle are written ([`Writer`]), oldest first, and a lease the scheduler gives up
//!   meanwhile loses its held `Start`;
//! - only the newest stream of a worker is heard, and a `Start` goes to the stream whose
//!   heartbeat it counts against ([`Stream`], [`Core::heartbeat`]);
//! - a `Result` is taken from the node that holds its operation's current lease, and each
//!   operation is answered once ([`Standing`], [`Taken`]);
//! - a core that is handed over takes nothing more ([`Core::fence`]), and one that is
//!   draining places nothing ([`Core::close_intake`]).
//!
//! [`Core`] is that loop behind one lock. What reaches outside it, the messages to a host,
//! the callers waiting on an operation, the result's bytes, is an [`Adapter`] call, made
//! under the same lock; which hosts are heard at all is an [`Admission`]. The core knows
//! only the scheduler's own vocabulary (`js_types::sched`): operations, leases, workers,
//! work keys and result references.
//!
//! A core has a state store or none. [`Core::new`] starts with a [`Writer`] that takes every
//! row, so a process without a store carries effects out as they commit. [`Core::open`]
//! takes up what a store holds and writes every change of the scheduler to it through
//! [`js_store::Rows`] before acting on it: a request before it is queued, a grant before its
//! `Start` leaves, a result before its callers are answered, a lease given up, a daemon's
//! session and the claim of its node. A store that cannot be reached holds the effects
//! behind the row it refused and tells the scheduler, which gives nothing up and places
//! nothing until a write takes again; a write that found another writer's work in its way
//! is dropped and undone.
//!
//! Time is an input: methods that feed the scheduler take the farm time. The crate reads a
//! clock in one place, the lock wait a submit reports to its [`Observer`].

mod adapter;
mod admission;
mod core;
mod engine;
mod host;
mod observer;
mod owner;
mod report;
mod store;
mod write;

pub use self::core::{
    Admitted, Config, Core, Held, NodeAction, NodeUnread, NotQueued, PlaceError, UnknownNode,
};
pub use adapter::{Adapter, Delivery, Link, Stage, View};
pub use admission::{Admission, AllowAll, Denied};
pub use engine::HELD_CAP;
pub use host::{Beat, Reported, Stream};
pub use js_sched::{LoadConfig, PlacementLoad};
pub use observer::Observer;
pub use report::{Reason, Report, Standing, Taken};
pub use store::Storage;
pub use write::{Attempt, Settles, Writer};
