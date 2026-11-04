//! The job table and the allocation table, as typed records over the `js-state`
//! [`Store`](js_state::Store) (design sections 3.2, 5.3, 6.2 and 10).
//!
//! A **job** is what a client submitted and where it is now ([`JobRecord`]); an
//! **allocation** is the scheduler's private record that resources on one host are booked
//! for one placement of one job ([`AllocationRecord`]). Both are kept after they end: an
//! ended job until its retention end (set by a later step), a freed allocation as history.
//!
//! # Keys
//!
//! | key | value |
//! |---|---|
//! | `job/<id>` | the job's [`JobRecord`] |
//! | `alloc/<id>` | the [`AllocationRecord`] of the job `<id>`: one allocation per job at a time, found from its job without a scan |
//! | `meta/jobs/schema` | the schema version, four bytes big-endian, written once with `Cond::Absent` by the first store to open; a higher one is accepted |
//!
//! A job id is 1 to 63 bytes of letters, digits, `.`, `_` and `-` ([`JobId::parse`]), so a
//! key never needs escaping. A minted job id is 128 random bits as 32 lowercase hex digits;
//! every placement mints a new [`AllocationId`] of 128 bits, so the job id with the
//! allocation id names one placement.
//!
//! # Rows
//!
//! A row is a format byte and tag-length-value fields (the `codec` module documents the
//! bytes). Rows change only by adding fields (B8): a tag this build does not know is kept
//! and written back unchanged when the row is rewritten, a known tag the row lacks reads as
//! its default, and a row with a higher format byte, or an enum value this build does not
//! know, is [`JobStoreError::UnknownRow`] (a list skips and logs it), never a missing row and
//! never a panic.
//!
//! # Transactions
//!
//! Each operation of [`JobStore`] that writes is one `Store::txn` whose preconditions are the
//! versions of the rows it read: it commits fully or answers an error having written nothing,
//! and a row that changed in between makes it [`JobStoreError::Stale`].
//!
//! | operation | transaction |
//! |---|---|
//! | [`JobStore::insert_queued`] | put `job/<id>` if absent |
//! | [`JobStore::place`] | put `job/<id>` at its version (queued to placed), put `alloc/<id>` if absent or at the version of a freed one (booked) |
//! | [`JobStore::transition`] | put `job/<id>` at its version; on an end, also put a booked `alloc/<id>` at its version (to releasing) |
//! | [`JobStore::requeue_failed_prepare`] | put `job/<id>` at its version (placed to queued), put `alloc/<id>` at its version (to freed) |
//! | [`JobStore::request_cancel`] | put `job/<id>` at its version, or nothing for an ended job or a second cancel |
//! | [`JobStore::mark_releasing`] | put `alloc/<id>` at its version, or nothing when it is not booked |
//! | [`JobStore::free`] | put `alloc/<id>` at its version, or nothing when it is freed |
//!
//! Times are the store's own clock ([`Store::now`](js_state::Store::now)); nothing here reads
//! a system clock.
//!
//! With the `test-support` feature, `suite` states each behaviour as a function generic over
//! a `js_state::conformance::Backend`, run on the fake, SQLite and Postgres
//! ([`job_store_tests!`]), and `interfere` is the store wrapper its failure and race checks
//! use.

mod codec;
mod error;
mod ids;
#[cfg(any(test, feature = "test-support"))]
pub mod interfere;
pub mod keys;
mod record;
mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod suite;

#[cfg(any(test, feature = "test-support"))]
pub use ids::ScriptedIds;

pub use codec::DecodeError;
pub use error::JobStoreError;
pub use ids::{AllocationId, IdSource, JobId, JobIdError, MAX_JOB_ID_LEN, MintError, RandomIds};
pub use record::{
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, HostBooking,
    JobRecord, JobSpec, JobState, Needs, NetworkPolicy, StartFailure,
};
pub use store::{Inserted, JobPage, JobStore, MAX_PAGE, MINT_TRIES, NewJob, SCHEMA_VERSION};
