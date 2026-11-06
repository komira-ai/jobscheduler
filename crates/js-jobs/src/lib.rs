//! The job table and the allocation table, as typed records over the `js-state`
//! [`Store`](js_state::Store) (design sections 3.2, 5.3, 6.2 and 10).
//!
//! A **job** is what a client submitted and where it is now ([`JobRecord`]); an
//! **allocation** is the scheduler's private record that resources on one host are booked
//! for one placement of one job ([`AllocationRecord`]). Both are kept after they end: an
//! ended job, with its [`JobResult`], until its retention end; a freed allocation as
//! history, as long as the placement estimates need it (design question 14), which no job
//! retention deletes.
//!
//! # Results and retention
//!
//! A job ends only through [`JobStore::end_job`], which writes the end state, the result and
//! the retention end (the store's clock plus the store's [`Retention`], seven days unless
//! [`JobStore::with_retention`] sets another) in the transaction that ends it; so a restart
//! loses no result, and no job is ever ended without them. The window is read when the store
//! is made and applied when a job ends: changing it affects only the jobs that end
//! afterwards. [`sweep`] lists the jobs past their retention end at a given time and reads no
//! clock; [`JobStore::tick`] runs it over the table and deletes those jobs' rows,
//! [`SWEEP_BATCH`] rows to a transaction; [`JobStore::raise_deadlines_at_boot`] raises every
//! stored retention end to at least the boot plus [`BOOT_GRACE`] before the first tick, so an
//! outage longer than the window deletes no result before a client could read it.
//! [`JobStore::raise_at_boot`] commits that raise, and in the same transaction gives every
//! queued, placed or starting job a deadline floor ([`JobRecord::deadline_floor`]) of at least
//! the boot plus [`BOOT_GRACE`]: the earliest instant the server's queue wait or start timeout
//! may end it, so an outage longer than either fails no job at the first tick.
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
//! | [`JobStore::place`] | put `job/<id>` at its version (queued to placed), put `alloc/<id>` if absent or at the version of a freed one (booked, with the job's declared host ports) |
//! | [`JobStore::transition`] | put `job/<id>` at its version (a move that does not end the job) |
//! | [`JobStore::end_job`] | put `job/<id>` at its version, with its result and retention end; also put a booked `alloc/<id>` at its version (to releasing); nothing for an ended job |
//! | [`JobStore::requeue_failed_prepare`] | put `job/<id>` at its version (placed to queued), put `alloc/<id>` at its version (to freed) |
//! | [`JobStore::request_cancel`] | put `job/<id>` at its version (for a queued job, its end as [`JobStore::end_job`] writes it), or nothing for an ended job or a second cancel |
//! | [`JobStore::mark_releasing`] | put `alloc/<id>` at its version, or nothing when it is not booked |
//! | [`JobStore::free`] | put `alloc/<id>` at its version, or nothing when it is freed |
//! | [`JobStore::free_with`] | as [`JobStore::free`], with the caller's rows put beside it (the usage history row of the allocation) |
//! | [`JobStore::end_and_free`] | put `job/<id>` at its version (ended as [`JobStore::end_job`] ends it) unless it has ended, and put `alloc/<id>` at its version (to freed) unless it is freed; nothing when both are so |
//! | [`JobStore::end_and_free_with`] | as [`JobStore::end_and_free`], with the caller's rows put beside the free when it frees |
//! | [`JobStore::delete_swept`] | delete each `job/<id>` past its retention end at its version, or nothing when none is |
//! | [`JobStore::tick`] | per batch of [`SWEEP_BATCH`] rows, delete each `job/<id>` past its retention end at its version |
//! | [`JobStore::raise_deadlines_at_boot`] | adds to the caller's transaction a put of each `job/<id>` whose retention end it raises, at its version |
//! | [`JobStore::raise_at_boot`] | put each `job/<id>` whose retention end or deadline floor it raises, at its version, or nothing when none is |
//!
//! Two reads serve the placement engine: [`JobStore::queued`], every queued job in queue
//! order ([`queue_order`]: the higher priority first, then the earlier submit, then the job
//! id), and [`JobStore::live_allocations`], every booked or releasing allocation.
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
mod result;
mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod suite;

#[cfg(any(test, feature = "test-support"))]
pub use ids::ScriptedIds;

pub use codec::DecodeError;
pub use error::JobStoreError;
pub use ids::{
    AllocationId, ClientName, ClientNameError, DEFAULT_CLIENT_NAME, IdSource, JobId, JobIdError,
    MAX_CLIENT_NAME_LEN, MAX_JOB_ID_LEN, MintError, RandomIds,
};
pub use record::{
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, HostBooking,
    JobRecord, JobSpec, JobState, Needs, NetworkPolicy, StartFailure,
};
pub use result::{
    BOOT_GRACE, JobEnd, JobResult, JobTiming, Output, ResourceUsage, ResultReason, Retention,
    RetentionError, SWEEP_BATCH, sweep,
};
pub use store::{
    Beside, Inserted, JobPage, JobStore, MAX_PAGE, MINT_TRIES, NewJob, Row, SCHEMA_VERSION,
    queue_order,
};
