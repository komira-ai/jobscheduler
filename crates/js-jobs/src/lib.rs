//! The job, allocation, mark, taint, cordon and node tables and the dispatch queue, as typed
//! records over the `js-state`
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
//! A start that a **held host** refused (the agent's code `host-held`: the host is held for
//! maintenance and made nothing) or that a host refused for **capacity** (the agent's code
//! `no-capacity`: the job's booking did not fit what the host holds) or that a host refused
//! while it **fetches the job's bundle** (the agent's code `bundle-pending`) sends its job back
//! to the queue: [`JobStore::requeue_held`] frees the allocation, keeps the job off the host
//! with a bar in [`JobRecord::prepare_bars`] and appends the host to
//! [`JobRecord::held_refusals`] or [`JobRecord::capacity_refusals`] (as [`RequeuedFor`] says;
//! a bundle-pending refusal to neither), in one transaction, counting no failure.
//!
//! A **taint** is the mark a host-isolation job's placement puts on its host ([`HostTaint`],
//! reason [`HOST_JOB_TAINT`]): [`JobStore::place`] and [`JobStore::place_with`] write it in the
//! transaction that places the job, and the caller deletes it once it holds the taint where it
//! keeps every host's taint ([`JobStore::drop_taints`], at the version read and only while it
//! names that job); nothing else in this crate deletes one. Every placement's commit checks
//! that no taint row is there for a contained job (and puts it absent for a host job);
//! [`JobStore::place_claimed`] also reads the host's binding (`hosts/binding/<host>`, the
//! server's) and refuses a host whose taint row is there or whose binding holds a taint
//! ([`JobStoreError::HostTainted`]).
//!
//! A **scale-down mark** (`hostmark/<host>`, [`ScaleMark`]) takes a host that is leaving the pool
//! out of placement: written at the version read by whoever marks it ([`JobStore::mark_host`],
//! [`JobStore::unmark_host`], [`JobStore::host_marks`]), and checked absent by every placement's
//! commit; [`JobStore::place_claimed`] also refuses it ([`JobStoreError::HostMarked`]). A placement made through
//! [`JobStore::place_claimed`] is also refused when a live allocation on the host holds a host
//! port the job declares ([`JobStoreError::PortTaken`]).
//!
//! A **cordon** takes a host out of placement. It is a row with a time to live
//! (`cordon/<host>`) that holds who holds the cordon, when the request that placed it named a
//! holder ([`JobStore::cordon_host`], [`JobStore::uncordon_host`], [`JobStore::cordons`],
//! [`JobStore::restore_cordon`]), so every server that shares the store sees a cordon any
//! server took, and every placement's transaction checks the row absent: a placement on a
//! cordoned host commits nothing.
//!
//! A **node** row is a host's presence ([`NodeRecord`]): its session, what its hello announced
//! and what its last applied beat left, with `heard_at` on the store's clock, so a beat that
//! reaches any server sharing the store is judged on the same row ([`JobStore::hello_node`],
//! [`JobStore::beat_node`], [`JobStore::read_node`], [`JobStore::live_nodes`],
//! [`JobStore::forget_node`]), and listed by [`JobStore::live_nodes`] and [`JobStore::nodes`]: every
//! server places over the live rows, whichever server took the host's hello. A hello of a host
//! new to its pool is counted against the pool's cap over every server's rows, and a hello of a
//! host whose row is live is refused unless it names the row's agent process
//! ([`JobStore::hello_node_in_pool`]), and a row silent long enough that holds nothing is
//! deleted ([`JobStore::forget_silent_nodes`]), or sooner, when the operator retires the host,
//! once it is silent past the grace and holds nothing ([`JobStore::retire_node`]).
//!
//! The **dispatch queue** holds what a host's agent is to do, one [`DispatchMessage`] per row
//! (`dispatch/<host>/<seq>`): a start, queued by the transaction that books the host (by the
//! move to starting for a whole-host job), and a stop, queued by a cancel or by an end the
//! server decided. Any server hands a host's due messages out ([`JobStore::claim_dispatch`]),
//! holding each for a lease on the store's clock, and deletes them when the agent acknowledges
//! them ([`JobStore::ack_dispatch`]); an end deletes its job's start.
//!
//! A job's **attempts** and its **completion callback**
//! (`docs/design/job-retry-and-completion-callback.md`) have their operations here:
//! [`JobStore::retry_txn`] queues a job's next attempt after one ended ([`AttemptRecord`]),
//! [`JobStore::attempt_row`] reads where and when an attempt ran ([`AttemptRow`]),
//! [`JobStore::claim_callbacks`] and [`JobStore::answer_callback`] hand out a due callback
//! to one instance ([`InstanceId`], [`CallbackClaim`]) and record its answer, and
//! [`JobStore::callback_status`] reads where a job's delivery stands ([`CallbackStatus`]).
//! A job's record keeps its current attempt and the attempts that ended ([`AttemptTrail`]: the
//! newest [`MAX_ATTEMPT_HISTORY`], the rest counted), and every attempt that ran has a row of
//! its own, written in the commit that ends or retries it. A retry is the one move of a job
//! that ran back to queued, open only to a job whose retry policy ([`JobSpec::retry`]) leaves
//! it an attempt after the current one.
//!
//! A job that asks for a callback ([`JobSpec::callback`], [`JobCallback`]) has its delivery in
//! the row `callback/<job id>`, the one row that holds the callback's signing key: the job row
//! holds the key's SHA-256 ([`CallbackKey::Digest`]), so a resubmit compares the callback by
//! it. The row is written armed in the commit that records the job, made due in the commit
//! that ends it (whichever operation ends it), claimed by one instance at a time, erases its key
//! when the delivery is delivered, abandoned or expired, has its deadline raised at boot, and is
//! deleted with the job's row at its retention end.
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
//! [`JobStore::raise_at_boot`] commits that raise, and gives every queued, placed or starting
//! job a deadline floor ([`JobRecord::deadline_floor`]) of at least the boot plus
//! [`BOOT_GRACE`]: the earliest instant the server's queue wait or start timeout may end it, so
//! an outage longer than either fails no job at the first tick; and it raises each mark's
//! deadline to at least the boot plus [`BOOT_GRACE`], so a mark restored after an outage does
//! not show a deadline already past (reaching a deadline gives nothing up). Each row is raised
//! in a transaction of its own, so another server writing the tables meanwhile costs the raise
//! only the rows it changed, which are read again.
//!
//! # Keys
//!
//! | key | value |
//! |---|---|
//! | `job/<id>` | the job's [`JobRecord`] |
//! | `alloc/<id>` | the [`AllocationRecord`] of the job `<id>`: one allocation per job at a time, found from its job without a scan |
//! | `mark/<id>` | the [`MarkRecord`] of the queued whole-host job `<id>`: one mark per job at a time, found from its job without a scan; a store written before marks has none and opens unchanged |
//! | `taint/<host>` | the [`HostTaint`] a host-isolation job's placement put on `<host>` (reason [`HOST_JOB_TAINT`]): the job that placed there; written in the placement's transaction, deleted by [`JobStore::drop_taints`] at the version read once the caller holds the taint elsewhere and by nothing else (the retention tick does not read it), listed whole by [`JobStore::host_taints`]; every placement's transaction refuses the host while it is there; a store written before taints has none and opens unchanged |
//! | `hostmark/<host>` | the [`ScaleMark`] of `<host>`: written at the version read by [`JobStore::mark_host`], deleted at it by [`JobStore::unmark_host`], listed whole by [`JobStore::host_marks`]; checked absent by every placement's transaction; a store written before marks of hosts has none and opens unchanged |
//! | `pool/<pool>` | empty: the admission row of a pool, written at the version read by the hello of a host new to the pool, beside its `node/` row ([`JobStore::hello_node_in_pool`]), so two such hellos are serialized; a store written before it has none |
//! | `hosts/binding/<host>` | the server's binding of `<host>` (its own JSON, not this crate's): read by the placement's transaction for its `taint_reason` and checked at the version read |
//! | `cordon/<host>` | the cordon of `<host>`: the holder it names, if any (the time it lapses is the row's expiry, so a lapsed cordon is an absent row); written by [`JobStore::cordon_host`] and [`JobStore::restore_cordon`], deleted by [`JobStore::uncordon_host`] or by its expiry, listed whole by [`JobStore::cordons`]; checked absent by every placement's transaction; a store written before cordons has none and opens unchanged |
//! | `hostbook/<host>` | the reservation row of `<host>`: the allocation id of the last placement made with [`JobStore::place_reserving`], whose version is what a placement's picture of the host is checked against; an earlier build has none, and a host with none is placed on as one whose row is absent |
//! | `node/<host>` | the [`NodeRecord`] of `<host>`: its session (the epoch of its last hello and the digest of that hello's secret), what the hello announced, its phase, load and listing flags, the epoch and sequence of the last applied beat and `heard_at`, the store's clock at the last hello or applied beat; written by [`JobStore::hello_node`] and [`JobStore::beat_node`], deleted by [`JobStore::forget_node`], [`JobStore::forget_silent_nodes`] and [`JobStore::retire_node`]; a store written before nodes has none and opens unchanged |
//! | `attempt/<id>/<n>` | the [`AttemptRow`] of attempt `<n>` (ten digits) of the job `<id>`, an attempt that ran: written in the commit that ends or retries it, read by [`JobStore::attempt_row`], deleted by the retention sweep before the job's row and, when the job's row is gone without it (an older build's sweep), by the tick's search ([`JobStore::tick`]); a store written before attempts has none and opens unchanged |
//! | `meta/cordons/restored` | empty: the cordons an earlier release kept only in a server's host bindings were carried into `cordon/` ([`JobStore::restore_cordons`]); no later start carries any again |
//! | `meta/jobs/schema` | the schema version, four bytes big-endian, written once with `Cond::Absent` by the first store to open; a higher one is accepted |
//! | `idx/job/active/<id>` | empty: the job has not ended |
//! | `idx/job/queued/<id>` | empty: the job is queued (it is under `idx/job/active/` too); a store an earlier build wrote has none until a build that names them opens it |
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
//! | [`JobStore::place`] | put `job/<id>` at its version (queued to placed), put `alloc/<id>` if absent or at the version of a freed one (booked, with the job's declared host ports); for a host-isolation job ([`Isolation::Host`]) also put `taint/<host>` if absent, and for a contained job check it absent, so a host already tainted makes the placement [`JobStoreError::Stale`]; and check `cordon/<host>` and `hostmark/<host>` absent, so a cordoned or marked host makes it [`JobStoreError::Stale`] |
//! | [`JobStore::place_with`] | as [`JobStore::place`], with the allocation's prepare deadline (the store's time now plus [`Placement::prepare_timeout`]) and wall limit, and delete `mark/<id>` at its version, or check it absent |
//! | [`JobStore::place_reserving`] | as [`JobStore::place_with`], and put `hostbook/<host>` if absent or at the version the caller's picture of the host was read at ([`JobStore::host_books`]), so a placement made since, by this server or another, makes it stale |
//! | [`JobStore::place_claimed`] | as [`JobStore::place_with`], decided on the rows the transaction locked; it reads `cordon/<host>`, `taint/<host>`, `hosts/binding/<host>`, `hostmark/<host>` and the host's live allocations, answers [`JobStoreError::HostCordoned`], [`JobStoreError::HostTainted`], [`JobStoreError::HostMarked`] or [`JobStoreError::PortTaken`] when one closes the host to the job, and checks the cordon, the taint and the mark absent and the binding at the version read in the commit |
//! | [`JobStore::cordon_host`] | put `cordon/<host>` with the time left as its time to live, if absent or at the version read; nothing when the cordon in force is another holder's |
//! | [`JobStore::uncordon_host`] | delete `cordon/<host>` at the version read when the rule lifts it; nothing otherwise |
//! | [`JobStore::restore_cordon`] | put `cordon/<host>` with the time given as its time to live, if absent |
//! | [`JobStore::restore_cordons`] | nothing when `meta/cordons/restored` is there; else, as [`JobStore::restore_cordon`] for each cordon given (a transaction each), then put `meta/cordons/restored` if absent |
//! | [`JobStore::hello_node`] | put `node/<host>` if absent or at the version read, with the next epoch; a lost race reads the row again |
//! | [`JobStore::beat_node`] | put `node/<host>` at the version read, when the beat's (epoch, sequence) is the row's to take; nothing otherwise; a lost race reads the row again and judges the beat on it |
//! | [`JobStore::hello_node_in_pool`] | for a host that has a row: nothing when the row was heard within the grace and names another agent process than the hello's (or either names none), else put `node/<host>` at the version read, with the next epoch, a lost race reading the row again and judging it again; for one that has none, put `node/<host>` if absent and `pool/<pool>` if absent or at the version read, when the pool's rows are fewer than its cap; nothing otherwise |
//! | [`JobStore::forget_node`] | delete `node/<host>` at the version given; nothing when the row is not at it |
//! | [`JobStore::forget_silent_nodes`] | per silent host, claim `node/<host>` and delete it at the version read when the host holds no live allocation, taint, cordon or mark; nothing otherwise |
//! | [`JobStore::retire_node`] | when the host's row is silent past the grace, claim `node/<host>` and delete it at the version read when the host holds no live allocation, taint, cordon or mark; nothing otherwise; a row that moved or went first is read again |
//! | [`JobStore::mark_host`] | put `hostmark/<host>` if absent or at the version read |
//! | [`JobStore::unmark_host`] | delete `hostmark/<host>` at the version given |
//! | [`JobStore::drop_taints`] | delete each `taint/<host>` it is given at the version read, while it names the job given, in one transaction; nothing when none is left |
//! | [`JobStore::transition`] | put `job/<id>` at its version (a move that does not end the job) |
//! | [`JobStore::end_job`] | put `job/<id>` at its version, with its result and retention end and, for a job that holds a host, the attempt it closes, and put that attempt's `attempt/<id>/<n>`; also put a booked `alloc/<id>` at its version (to releasing); nothing for an ended job |
//! | [`JobStore::retry_txn`] | put `job/<id>` at its version (placed, starting or running to queued, with the attempt appended, the counts of its requeues reset, its host barred and its next attempt's not-before), put `attempt/<id>/<n>`, put a booked `alloc/<id>` at its version (to releasing), and delete the allocation's start from `dispatch/<host>/`, queueing a stop as [`JobStore::end_job`] does |
//! | [`JobStore::requeue_failed_prepare`] | put `job/<id>` at its version (placed to queued), put `alloc/<id>` at its version (to freed) |
//! | [`JobStore::request_cancel`] | put `job/<id>` at its version (for a queued job, its end as [`JobStore::end_job`] writes it, and delete `mark/<id>` at its version or check it absent), or nothing for an ended job or a second cancel |
//! | [`JobStore::requeue_held`] | put `job/<id>` at its version (placed or starting to queued, with the bars, the queue note and the host appended to the held or the capacity refusals, or to neither for a bundle-pending refusal), put `alloc/<id>` at its version (to freed), put the caller's rows beside it, and delete the allocation's start from `dispatch/<host>/` at its version |
//! | [`JobStore::fail_prepare_with`] | put `job/<id>` at its version (placed to queued with the failed-prepare count and bars, or ended failed to start), put `alloc/<id>` at its version (to freed), put the caller's rows beside it, and delete `mark/<id>` at its version or check it absent |
//! | [`JobStore::restart_prepare`] | put `alloc/<id>` at its version (the new prepare deadline), check `job/<id>` at its version |
//! | [`JobStore::mark_releasing`] | put `alloc/<id>` at its version, or nothing when it is not booked |
//! | [`JobStore::free`] | put `alloc/<id>` at its version, or nothing when it is freed |
//! | [`JobStore::free_with`] | as [`JobStore::free`], with the caller's rows put beside it (the usage history row of the allocation) |
//! | [`JobStore::end_and_free`] | put `job/<id>` at its version (ended as [`JobStore::end_job`] ends it) unless it has ended, and put `alloc/<id>` at its version (to freed) unless it is freed; nothing when both are so |
//! | [`JobStore::end_and_free_with`] | as [`JobStore::end_and_free`], with the caller's rows put beside the free when it frees |
//! | [`JobStore::delete_swept`] | delete each `job/<id>` past its retention end at its version, with its `attempt/<id>/` rows (a job with more than a page of [`SWEEP_BATCH`] of them has each full page deleted first in a transaction of its own that checks `job/<id>` at its version), or nothing when none is |
//! | [`JobStore::tick`] | per batch of [`SWEEP_BATCH`] rows, delete each `job/<id>` past its retention end at its version, with its `attempt/<id>/` rows as [`JobStore::delete_swept`] deletes them; then delete up to [`SWEEP_BATCH`] freed `alloc/<id>` rows past the allocation retention, at their versions, in one transaction; then delete, at their versions, the `attempt/` rows of the next page of the search whose `job/<id>` is absent, checking it absent, in one transaction |
//! | [`JobStore::raise_deadlines_at_boot`] | adds to the caller's transaction a put of each `job/<id>` whose retention end it raises, at its version |
//! | [`JobStore::raise_at_boot`] | put each `job/<id>` whose retention end or deadline floor it raises, and each `mark/<id>` whose deadline it raises, at its version, each in a transaction of its own, or nothing when none is |
//! | [`JobStore::put_mark`] | check `job/<id>` at its version (queued, whole host), put `mark/<id>` if absent or at its version |
//! | [`JobStore::delete_mark`] | delete `mark/<id>` at its version, or nothing when there is none |
//! | [`JobStore::end_and_unmark`] | as [`JobStore::end_job`] (nothing for an ended job), and delete `mark/<id>` at its version, or check it absent; nothing when the job has ended and has no mark |
//!
//! Two reads serve the placement engine: [`JobStore::queued`], every queued job in queue
//! order ([`queue_order`]: the higher priority first, then the earlier submit, then the job
//! id), read through the `idx/job/queued/` entries, so the rows of the jobs placed or running
//! are not read; and [`JobStore::live_allocations`], every booked or releasing allocation,
//! read through the `idx/alloc/host/` entries.
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

mod attempt;
mod callback;
mod codec;
mod error;
mod ids;
#[cfg(any(test, feature = "test-support"))]
pub mod interfere;
pub mod keys;
mod node;
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

pub use attempt::{
    Attempt, AttemptOutcome, AttemptRecord, AttemptRow, AttemptTrail, MAX_ATTEMPT_HISTORY,
    RETRY_HOST_BAR,
};
pub use callback::{
    CallbackClaim, CallbackKey, CallbackState, CallbackStatus, JobCallback, KeyDigest,
};
pub use codec::DecodeError;
pub use error::JobStoreError;
pub use ids::{
    AllocationId, ClientName, ClientNameError, DEFAULT_CLIENT_NAME, IdSource, InstanceId, JobId,
    JobIdError, MAX_CLIENT_NAME_LEN, MAX_JOB_ID_LEN, MintError, RandomIds,
};
pub use node::{
    AppliedBeat, Beat, Kept, LoadReading, NodeHello, NodeRecord, NodeRow, NodeState, Retire,
    SessionDigest,
};
pub use queue::{JobEvent, MAX_JOB_EVENTS, QueueTrail};
pub use record::{
    AllocationRecord, AllocationStep, Booked, Capacity, Constraints, DeclaredPort, DispatchKind,
    DispatchMessage, EndReason, Exclusion, FileDigest, HOST_JOB_TAINT, HostBooking, HostTaint,
    Isolation, JobFiles, JobRecord, JobSpec, JobState, MarkRecord, Needs, NetworkPolicy, ScaleMark,
    SizedFrom, StartFailure,
};
pub use result::{
    BOOT_GRACE, CpuPeak, CpuThrottle, JobEnd, JobResult, JobTiming, LogLengths, Output,
    ResourceUsage, ResultReason, Retention, RetentionError, SWEEP_BATCH, sweep,
};
pub use store::{
    Backfill, Beside, CordonWrite, FailedPrepare, FreedPage, HeldRequeue, Inserted, JobPage,
    JobStore, MAX_PAGE, MINT_TRIES, MOST_TABLE_CHANGES, NewJob, Placement, RequeuedFor, Row,
    SCHEMA_VERSION, Step, TableChanges, UncordonWrite, Written, queue_order,
};
