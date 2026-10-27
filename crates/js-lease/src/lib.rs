//! The lease table a lease service serves: leases a client acquires on a provider,
//! keeps alive by renewing, and gives back by releasing or letting them lapse.
//!
//! This is design section 3 (a lease is the scheduler's grant of resources on a
//! provider), 5.2 and 5.6 (start, stop, observe and what Unknown means) and 12.1 (the
//! maximum wall time) as a table with a file behind it. The API server and the CLI are
//! later changes; this crate is the state they serve.
//!
//! # Rules
//!
//! - **Commit before start.** [`LeaseTable::acquire`] writes the lease to the file as
//!   `pending` before it calls [`js_provider::Provider::start`]. A crash between the
//!   two leaves a record that the next boot resolves by observing, never a booking
//!   nobody knows about.
//! - **Unknown never frees.** A start that answers Unknown leaves the lease `pending`;
//!   a stop that answers Unknown leaves it `releasing`. Only a stop that read back
//!   `Absent`, or an observation of `Absent`, removes a lease. [`LeaseTable::tick`]
//!   retries both.
//! - **Time acts in one place.** Only [`LeaseTable::tick`] expires a lease. Between
//!   ticks a lapsed lease is still listed, and a renew of it is refused as expired.
//! - **Wall time bounds a lease's life.** A lease is never renewed past its
//!   acquisition plus the request's maximum wall time.
//! - **Ids never repeat.** An id is (term, sequence). Every boot takes the next term
//!   and the sequence only grows, both persisted.
//! - **Time is wall time.** [`Clock::now`] is a [`Timestamp`], milliseconds since the
//!   Unix epoch, because an expiry written to the file has to mean the same moment
//!   after a restart. Tests drive a [`ManualClock`]; nothing here sleeps.
//!
//! # The file
//!
//! One TOML document, replaced whole through a temporary file next to it (written,
//! fsynced, renamed, directory fsynced) after every change. Formats are additive: a
//! reader ignores the version number and every field it does not know, so a file
//! written by a later build still loads. A file that cannot be parsed, or holds a lease
//! that is not valid, is an [`OpenError::Corrupt`] naming the path; the table never
//! starts empty over it.
//!
//! ```toml
//! version = 1
//! term = 3
//! next_sequence = 5
//!
//! [[lease]]
//! term = 3
//! sequence = 4
//! state = "held"            # pending | held | releasing
//! host = "gpu-01"           # absent until the provider names it
//! acquired_at_ms = 1000000
//! expires_at_ms = 1060000
//! ends_at_ms = 4600000      # acquired_at plus the request's maximum wall time
//! validation_run = "val-7"  # optional
//!
//! [lease.labels]            # the caller's labels
//! team = "infra"
//! ```
//!
//! On open, a lease stored as `held` is observed once: present stays held (on the host
//! the provider names), absent is dropped, unknown is kept.

mod book;
mod clock;
mod error;
mod state;
mod store;
mod table;
mod types;

pub use clock::{Clock, ManualClock, SystemClock, Timestamp};
pub use error::{AcquireError, OpenError, ReleaseError, RenewError, SaveError, SettingsError};
pub use state::LeaseState;
pub use table::LeaseTable;
pub use types::{Lease, LeaseInfo, ReleaseOutcome, Settings, TickReport};
