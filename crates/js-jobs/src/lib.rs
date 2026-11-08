//! The job, allocation, mark and taint tables, as typed records over the `js-state`
//! [`Store`](js_state::Store) (design sections 3.2, 5.3, 5.4, 6.2 and 10).
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
//! end in one transaction ([`JobStore::end_and_unmark`]), with its cancel when it is queued
//! ([`JobStore::request_cancel`]), with its placement ([`JobStore::place_with`]) or its failed
//! prepare ([`JobStore::fail_prepare_with`]), or on its own ([`JobStore::delete_mark`]).
//!
//! A **whole-host job** that is placed books the whole host and waits for its agent to report
//! the host ready. Its allocation row holds the instant the agent must have done so
//! ([`AllocationRecord::prepare_deadline`], written by [`JobStore::place_with`] and moved by
//! [`JobStore::restart_prepare`] when the agent says hello again) and, for a placement on a
//! host a mark holds, its own wall limit ([`AllocationRecord::wall_limit_secs`]). Its job row
//! holds how many of its prepares failed ([`JobRecord::failed_prepares`]) and the hosts they
//! keep it off ([`JobRecord::prepare_bars`]), written by [`JobStore::fail_prepare_with`] in the
//! transaction that frees the allocation and puts the job back to queued, or ends it failed to
//! start when the placement engine says its prepares are used up.
//!
//! A **taint** is the mark a host-isolation job's placement puts on its host ([`HostTaint`],
//! reason [`HOST_JOB_TAINT`]): [`JobStore::place`] and [`JobStore::place_with`] write it in the
//! transaction that places the job, and the caller deletes it once it holds the taint where it
//! keeps every host's taint ([`JobStore::drop_taints`]); nothing else in this crate deletes one.
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
//! each mark's deadline to at least the boot plus [`BOOT_GRACE`], so a mark restored after an
//! outage does not show a deadline already past (reaching a deadline gives nothing up).
//!
//! # Keys
//!
//! | key | value |
//! |---|---|
//! | `job/<id>` | the job's [`JobRecord`] |
//! | `alloc/<id>` | the [`AllocationRecord`] of the job `<id>`: one allocation per job at a time, found from its job without a scan |
//! | `mark/<id>` | the [`MarkRecord`] of the queued whole-host job `<id>`: one mark per job at a time, found from its job without a scan; a store written before marks has none and opens unchanged |
//! | `taint/<host>` | the [`HostTaint`] a host-isolation job's placement put on `<host>` (reason [`HOST_JOB_TAINT`]): the job that placed there; written in the placement's transaction, deleted by [`JobStore::drop_taints`] once the caller holds the taint elsewhere and by nothing else (the retention tick does not read it), listed whole by [`JobStore::host_taints`]; a store written before taints has none and opens unchanged |
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
//! | [`JobStore::place`] | put `job/<id>` at its version (queued to placed), put `alloc/<id>` if absent or at the version of a freed one (booked, with the job's declared host ports); for a host-isolation job ([`Isolation::Host`]) also put `taint/<host>` if absent, so a host already tainted makes the placement [`JobStoreError::Stale`] |
//! | [`JobStore::place_with`] | as [`JobStore::place`], with the allocation's prepare deadline (the store's time now plus [`Placement::prepare_timeout`]) and wall limit, and delete `mark/<id>` at its version, or check it absent |
//! | [`JobStore::drop_taints`] | delete each `taint/<host>` it is given, whatever it holds, in one transaction; nothing for no hosts |
//! | [`JobStore::transition`] | put `job/<id>` at its version (a move that does not end the job) |
//! | [`JobStore::end_job`] | put `job/<id>` at its version, with its result and retention end; also put a booked `alloc/<id>` at its version (to releasing); nothing for an ended job |
//! | [`JobStore::requeue_failed_prepare`] | put `job/<id>` at its version (placed to queued), put `alloc/<id>` at its version (to freed) |
//! | [`JobStore::request_cancel`] | put `job/<id>` at its version (for a queued job, its end as [`JobStore::end_job`] writes it, and delete `mark/<id>` at its version or check it absent), or nothing for an ended job or a second cancel |
//! | [`JobStore::fail_prepare_with`] | put `job/<id>` at its version (placed to queued with the failed-prepare count and bars, or ended failed to start), put `alloc/<id>` at its version (to freed), put the caller's rows beside it, and delete `mark/<id>` at its version or check it absent |
//! | [`JobStore::restart_prepare`] | put `alloc/<id>` at its version (the new prepare deadline), check `job/<id>` at its version |
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
mod queue;
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
pub use queue::{JobEvent, MAX_JOB_EVENTS, QueueTrail};
pub use record::{
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, Exclusion,
    FileDigest, HOST_JOB_TAINT, HostBooking, HostTaint, Isolation, JobFiles, JobRecord, JobSpec,
    JobState, MarkRecord, Needs, NetworkPolicy, StartFailure,
};
pub use result::{
    BOOT_GRACE, JobEnd, JobResult, JobTiming, LogLengths, Output, ResourceUsage, ResultReason,
    Retention, RetentionError, SWEEP_BATCH, sweep,
};
pub use store::{
    Backfill, Beside, FailedPrepare, Inserted, JobPage, JobStore, MAX_PAGE, MINT_TRIES, NewJob,
    Placement, Row, SCHEMA_VERSION, queue_order,
};
