//! The job table, the allocation table and the mark table, as typed records over the
//! `js-state` [`Store`](js_state::Store) (design sections 3.2, 5.3, 5.4, 6.2 and 10).
//!
//! A **job** is what a client submitted and where it is now ([`JobRecord`]); an
//! **allocation** is the scheduler's private record that resources on one host are booked
//! for one placement of one job ([`AllocationRecord`]). Both are kept after they end: an
//! ended job, with its [`JobResult`], until its retention end; a freed allocation for the
//! store's allocation retention after its free (fourteen days unless
//! [`JobStore::with_allocation_retention`] sets another), after which [`JobStore::tick`]
//! deletes its row. The usage history of an allocation is its `usage/ended` row, which
//! neither retention deletes. A **mark** is the host a queued whole-host job holds while
//! that host drains, with the hosts it is kept off ([`MarkRecord`]): written only for a job
//! that is queued and needs the whole host ([`JobStore::put_mark`]), and deleted with the job's
//! end in one transaction ([`JobStore::end_and_unmark`]) or on its own
//! ([`JobStore::delete_mark`]).
//!
//! # Results and retention
//!
//! A result keeps, beside the tails, the length of each output stream and the bytes each
//! dropped ([`LogLengths`]) when the agent reported them, so that a reader of the job's live
//! logs knows where each stream ends; they go with the result and its retention. A job ends
//! only through [`JobStore::end_job`], which writes the end state, the result and
//! the retention end (the store's clock plus the store's [`Retention`], seven days unless
//! [`JobStore::with_retention`] sets another) in the transaction that ends it; so a restart
//! loses no result, and no job is ever ended without them. The window is read when the store
//! is made and applied when a job ends: changing it affects only the jobs that end
//! afterwards. The same transaction replaces the contents of the job's files, which are
//! secret, by their SHA-256 ([`JobFiles`]): an ended job's row, kept for the window, holds
//! the file names and no contents, and [`JobStore::raise_at_boot`] does the same to a row of
//! an ended job that a build before this one left holding them. [`sweep`] lists the jobs past
//! their retention end at a given time and reads no clock; [`JobStore::tick`] runs it over the
//! table and deletes those jobs' rows,
//! [`SWEEP_BATCH`] rows to a transaction; [`JobStore::raise_deadlines_at_boot`] raises every
//! stored retention end to at least the boot plus [`BOOT_GRACE`] before the first tick, so an
//! outage longer than the window deletes no result before a client could read it.
//! [`JobStore::raise_at_boot`] commits that raise, and in the same transaction gives every
//! queued, placed or starting job a deadline floor ([`JobRecord::deadline_floor`]) of at least
//! the boot plus [`BOOT_GRACE`]: the earliest instant the server's queue wait or start timeout
//! may end it, so an outage longer than either fails no job at the first tick; and it raises
//! each mark's deadline to at least the boot plus [`BOOT_GRACE`], so an outage longer than a
//! hold gives up none at the first round.
//!
//! # Keys
//!
//! | key | value |
//! |---|---|
//! | `job/<id>` | the job's [`JobRecord`] |
//! | `alloc/<id>` | the [`AllocationRecord`] of the job `<id>`: one allocation per job at a time, found from its job without a scan |
//! | `mark/<id>` | the [`MarkRecord`] of the queued whole-host job `<id>`: one mark per job at a time, found from its job without a scan; a store written before marks has none and opens unchanged |
//! | `meta/jobs/schema` | the schema version, four bytes big-endian, written once with `Cond::Absent` by the first store to open; a higher one is accepted |
//! | `idx/job/active/<id>` | empty: the job has not ended |
//! | `idx/job/due/<end>/<id>` | empty: the job has ended, `<end>` its retention end's nanoseconds on the store's clock as twenty digits (zero for an ended row without one) |
//! | `idx/alloc/host/<host>/<id>` | empty: the job's allocation is live (booked or releasing) on `<host>` |
//! | `idx/alloc/freed/<at>/<id>` | empty: the job's allocation was freed at `<at>`, as twenty digits |
//! | `idx/job/unread/<id>`, `idx/alloc/unread/<id>` | empty: the row is one this build cannot read (a later build's, or one that does not parse) |
//!
//! The `idx/` keys are secondary indexes: each write of a job or allocation row moves the
//! row's entries in the same transaction, and every read the tick, the placement engine and a
//! heartbeat make goes through them, so none scans a table whose size grows with history. A
//! read judges the row an entry points at, never the entry alone. [`JobStore::new`] runs
//! [`JobStore::backfill_indexes`], which makes the entries agree with the rows the tables hold
//! (rows an earlier release wrote included), so every open, a server's start among them,
//! begins from indexes that agree. A key under `idx/` this build does not name is left as it
//! is.
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
//! | [`JobStore::tick`] | per batch of [`SWEEP_BATCH`] rows, delete each `job/<id>` past its retention end at its version; then delete up to [`SWEEP_BATCH`] freed `alloc/<id>` rows past the allocation retention, at their versions, in one transaction |
//! | [`JobStore::raise_deadlines_at_boot`] | adds to the caller's transaction a put of each `job/<id>` whose retention end it raises, at its version |
//! | [`JobStore::raise_at_boot`] | put each `job/<id>` whose retention end or deadline floor it raises, and each `mark/<id>` whose deadline it raises, at its version, or nothing when none is |
//! | [`JobStore::put_mark`] | check `job/<id>` at its version (queued, whole host), put `mark/<id>` if absent or at its version |
//! | [`JobStore::delete_mark`] | delete `mark/<id>` at its version, or nothing when there is none |
//! | [`JobStore::end_and_unmark`] | as [`JobStore::end_job`] (nothing for an ended job), and delete `mark/<id>` at its version, or check it absent; nothing when the job has ended and has no mark |
//!
//! Two reads serve the placement engine: [`JobStore::queued`], every queued job in queue
//! order ([`queue_order`]: the higher priority first, then the earlier submit, then the job
//! id), and [`JobStore::live_allocations`], every booked or releasing allocation. Both read
//! through the `idx/` entries.
//!
//! Every operation that writes also moves the `idx/` entries of the rows it writes, in the
//! same transaction (the table above names only the rows).
//!
//! Times are the store's own clock ([`Store::now`](js_state::Store::now)); nothing here reads
//! a system clock.
//!
//! With the `test-support` feature, `suite` states each behaviour as a function generic over
//! a `js_state::conformance::Backend`, run on the fake, SQLite and Postgres
//! ([`job_store_tests!`]), `interfere` is the store wrapper its failure and race checks
//! use, and `rows` reads and writes a stored row's bytes for the fuzz targets.

mod codec;
mod error;
mod ids;
#[cfg(any(test, feature = "test-support"))]
pub mod interfere;
pub mod keys;
mod record;
mod result;
#[cfg(any(test, feature = "test-support"))]
pub mod rows;
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
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, Exclusion,
    FileDigest, HostBooking, JobFiles, JobRecord, JobSpec, JobState, MarkRecord, Needs,
    NetworkPolicy, StartFailure,
};
pub use result::{
    BOOT_GRACE, JobEnd, JobResult, JobTiming, LogLengths, Output, ResourceUsage, ResultReason,
    Retention, RetentionError, SWEEP_BATCH, sweep,
};
pub use store::{
    Backfill, Beside, Inserted, JobPage, JobStore, MAX_PAGE, MINT_TRIES, NewJob, Row,
    SCHEMA_VERSION, queue_order,
};
