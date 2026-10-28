//! The server's core: the loop around [`js_sched::Scheduler`] that carries its effects out,
//! and the session a host has with it.
//!
//! The scheduler is a pure state machine: it takes an input and returns effects (commit this
//! record, send this `Start`, answer these callers). What makes a server correct, and the
//! machine cannot see, is the loop that carries those effects out:
//!
//! - a record is committed, and fed back as committed, before anything that depends on it
//!   happens; the `Start` of a lease and the answer of an operation are held until the rows
//!   they settle are written ([`Writer`]), oldest first, and a lease the scheduler gives up
//!   meanwhile loses its held `Start`;
//! - only the newest stream of a worker is heard, and a `Start` goes to the stream whose
//!   heartbeat it counts against ([`Stream`], [`Core::heartbeat`]);
//! - a core that is handed over takes nothing more ([`Core::fence`]), and one that is
//!   draining places nothing ([`Core::close_intake`]).
//!
//! [`Core`] is that loop behind one lock. What reaches outside it, the messages to a host,
//! the callers waiting on an operation, the result's bytes, is an [`Adapter`] call, made
//! under the same lock; which hosts are heard at all is an [`Admission`]. The core knows
//! only the scheduler's own vocabulary (`js_types::sched`): operations, leases, workers,
//! work keys and result references. It keeps no state store yet: a core starts with a
//! [`Writer`] that takes every row, and a process without a store carries effects out as
//! they commit.
//!
//! Time is an input: methods that feed the scheduler take the farm time. The crate reads a
//! clock in one place, the lock wait a submit reports to its [`Observer`].

mod adapter;
mod admission;
mod core;
mod engine;
mod host;
mod observer;
mod write;

pub use self::core::{Admitted, Config, Core, Fenced, NodeAction, UnknownNode};
pub use adapter::{Adapter, Delivery, Link, Stage, View};
pub use admission::{Admission, AllowAll, Denied, SelfFencedOnly};
pub use engine::HELD_CAP;
pub use host::{Beat, Reported, Stream};
pub use observer::Observer;
pub use write::{Attempt, Writer};
