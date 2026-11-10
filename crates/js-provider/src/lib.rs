//! The booking table the hosts of the scheduler are kept in.
//!
//! [`HostBook`] is the table of hosts a server lists: each host's shape, the leases it holds
//! and what each books there, and every lease it has stopped. The registry of
//! self-registered hosts (`js-registry`) lists its hosts in one, and so does the test fake
//! `js-pool-fake`, so neither copies the rules of a host's shape and its memory. The registry reads it for which hosts are listed and what they offer; the
//! booking calls ([`HostBook::start_where`], [`HostBook::start_by`], [`HostBook::start_over`],
//! [`HostBook::seat`], [`HostBook::stop`]) are kept with their tests and no server code calls
//! them: the scheduler books work on a host through its job tables, not through this table.
//!
//! The value types a booking is made of (`StartSpec`, `Candidate`, `HostCapacity`,
//! `RefusalCode`) are defined in `js-types`, which the placement engine reads them from, and
//! are re-exported here so the paths of the booking calls stay as they were.
//!
//! What a booking call answers: [`StartAck`] for a start, and for a read of one lease
//! [`Observed::Present`] or [`Observed::Absent`]. Only `Absent` frees a booking
//! ([`Observed::frees_booking`]); [`Observed::Unknown`] is the lack of an answer and never
//! does.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod ack;
mod book;

pub use ack::{Observed, StartAck};
pub use book::{ChooseOver, HostBook, SeatError};
pub use js_types::{Candidate, HostCapacity, RefusalCode, StartSpec};
