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
//!   The server's tick (`js-lease-server`) ticks the table first and then advances the
//!   agent registry, so a registry that reaches its own deadline (a reservation's drain
//!   deadline, a prepare deadline) acts on the same tick and not at the next poll.
//! - **Wall time bounds a lease's life, as the clock reads it.** A lease is never
//!   renewed past its acquisition plus the request's maximum wall time. The bound is a
//!   wall-clock instant, so a system clock stepped back (a time sync, a resumed VM)
//!   stretches a lease by the step, and one stepped forwards cuts it short. It is not
//!   a guarantee against a clock that moves: keep the host's clock from stepping back
//!   under a server, or rely on the provider's own limits.
//! - **Ids never repeat.** An id is (term, sequence). Every boot takes the next term
//!   and the sequence only grows, both persisted. That holds while the file does, so a
//!   missing file is an error unless the caller says it is a first boot ([`Boot`]), and
//!   an exclusive lock beside the file keeps a second table from taking the same term.
//! - **Time is wall time.** [`Clock::now`] is a [`Timestamp`], milliseconds since the
//!   Unix epoch, because an expiry written to the file has to mean the same moment
//!   after a restart. Tests drive a [`ManualClock`]; nothing here sleeps.
//!
//! # The file
//!
//! One TOML document, replaced whole through a temporary file next to it (written,
//! fsynced, renamed, directory fsynced) after every change. Formats are additive: a
//! reader ignores the version number and every field it does not know, so a file
//! written by a later build still loads, and it writes those fields back (with the
//! higher version) so an older build does not destroy them. `lease_count` is the number
//! of `[[lease]]` tables; a file with a different number of leases (cut short), a
//! lease listed twice, a file that cannot be parsed, or a lease that is not valid, is an
//! [`OpenError::Corrupt`] naming the path; the table never starts empty over it.
//!
//! ```toml
//! version = 1
//! term = 3
//! next_sequence = 5
//! lease_count = 1
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
//! whole = true              # optional: a whole-node class; absent means not whole
//! ttl_ms = 60000            # a pending whole lease only: the granted ttl ...
//! wall_ms = 3600000         # ... and maximum wall time, both counted from ready
//!
//! [lease.labels]            # the caller's labels
//! team = "infra"
//!
//! [lease.shares]            # optional: only a lease of a class that books part of a host
//! cpu_millis = 4000         # ... what it books there (gpus, gpu_class and disk_bytes
//! memory_bytes = 8589934592 # are left out when zero); absent means the whole host
//! ```
//!
//! [`read_bookings`] reports `shares` as [`Booking::shares`], so a provider that lets
//! leases share a host puts the lease back with what it booked. The field is read
//! leniently: a file from before it, or a row whose resources are not a valid vector,
//! reads as a lease that books the whole host, and a field of the row that this build does
//! not know is ignored.
//!
//! A lease of a whole-node class (admission `Whole`) waits differently while pending: its
//! expiry is the settings' prepare timeout, not its ttl, and when the provider is first
//! seen to have it ready (a start answered `Started`, or an observation of `Present`)
//! its expiry becomes that moment plus the ttl and its end that moment plus the maximum
//! wall time. A file without these fields loads as not whole.
//!
//! The prepare timeout is also the provider's own deadline for a failed prepare, and the
//! provider decides what a missed one means (it counts the failure and holds it against
//! the host). So a tick that finds a pending whole lease at its deadline observes it
//! first, which lets the provider end it; only a lease the provider still has no answer
//! for after that observation is expired, by the tick after. A ready report that reaches
//! the provider within one tick of the deadline can therefore lose to the deadline; that
//! is benign, since the host is restored when the lease is stopped. The note that the
//! deadline has been observed is kept in memory: after a restart the lease is observed
//! once more.
//!
//! On open, a lease stored as `held` is observed once: present stays held (on the host
//! the provider names), absent is dropped, unknown is kept.
//!
//! A provider that keeps its bookings only in memory has forgotten every one after a
//! restart, and would answer absent for all of them. [`read_bookings`] reads the hosts
//! the file names (a lease with a host, whatever its state) without opening the table,
//! so the caller can seat them in that provider first.
//!
//! # When a save fails
//!
//! A change is applied in memory first and written second, so after a [`SaveError`]
//! the table holds the change and the file is behind it: it still says what the last
//! successful save wrote. The table does not roll back. The next successful save, which
//! [`LeaseTable::tick`] retries while the file is behind, writes the whole table and
//! brings the file level. A caller that wants to return to what the file says instead
//! drops the table and opens the file again; the open observes every held lease and a
//! tick every pending one, so the
//! provider settles anything the dropped changes touched. A crash while the file is
//! behind loses the changes after the last save: a lease the provider started but the
//! file never held is found pending (or missing) and resolved by observing it, which is
//! why a start is never called before its pending record is written.
//!
//! # Slow steps
//!
//! Every save writes, fsyncs and renames the file, and a caller holds the table's lock
//! meanwhile, so a slow disk stalls every call on the table. [`Timings`] times each save
//! on a monotonic clock ([`SystemMonotonic`] unless [`LeaseTable::watch_saves`] gives
//! another) under the step that made it (`open`, `acquire`, `acquire-settled`, `renew`,
//! `release`, `release-settled`, `tick`), and the server's handlers time their wait for
//! the table's lock under `wait-` and the call's name. A step that takes over one second
//! (exactly one second is not) is logged as a warning, `slow lease step`, naming the
//! operation and `took_ms`. [`Timings::report`] gives, per operation, how many steps were
//! timed, how many took over a second and the longest, whether or not any was slow, and
//! how many have begun and not ended (a step stuck at this moment).
//!
//! # The lock
//!
//! The lock beside the file is an advisory lock (`flock`) on `<file>.lock`. It keeps
//! two tables of one host from sharing a file, and it is only as good as the
//! filesystem: on a network filesystem `flock` may not be enforced between hosts, or may
//! be lost on a reconnect, and two tables could then issue the same ids. Keep the file
//! on a local disk that one server owns.
//!
//! # Restoring an older file
//!
//! Ids stay unique because the term and the sequence in the file only grow. A file
//! restored from a backup is older than the one it replaces: its term and sequence may
//! be below ids the lost file already issued, and the next boot would issue them again.
//! So a restore must be followed by a term bump before the server starts: edit `term`
//! and `next_sequence` in the restored file to values above any the lost file can have
//! held (above the highest the clients or the provider have seen), then start; the
//! first boot takes the term after that. A verb that does this safely, and checks it
//! against the provider, is a follow-up; until then a restore is an operator action and
//! the table cannot tell it happened.
//!
//! # Versions
//!
//! A file with a higher `version` than this build writes is accepted: the number is
//! ignored, the fields it adds are kept and written back. That is safe while changes are
//! additive. A later change that is not additive (a field whose meaning changes, one an
//! older build must not ignore) will need a minimum-reader-version field that makes an
//! older build refuse the file; that field is a follow-up and does not exist yet, so
//! such a change cannot be made before it.
//!
//! # Reservation records
//!
//! The file also holds a ledger of reservation records ([`ReservationRecord`]) beside
//! the leases, as `[[reservation]]` tables counted by `reservation_count` (written only
//! when above zero; a file with fewer rows than it counts is refused like a cut lease
//! list). They are written in the same atomic replace as the leases, so a record never
//! outlives a crash without the leases it was saved with. The table keeps the ledger
//! whole and decides nothing about it: [`LeaseTable::set_reservations`] replaces it (the
//! next tick writes it) and [`LeaseTable::reservations`] reads it. An open returns each
//! held deadline raised to at least the boot plus [`Settings::boot_grace`] (never the
//! stored deadline plus the grace, so repeated restarts do not accumulate it; lease
//! records get no such grace here) and stamps every record with
//! `claim_by`, the boot plus [`Settings::regain_grace`]. A file without the rows loads
//! an empty ledger; fields of a later version in a row are kept on rewrite.
//!
//! A row can also carry the request's place on the wait list (`queue_place` and
//! `queued_since_ms`, both or neither, [`Queued`]) and whether its host was quiet when it
//! was set aside (`assigned`). Both are additive: a row without them loads as not queued
//! and not assigned, and a build that does not know them reads past them (they are among
//! the fields of a later version it keeps and writes back), so an older server opens a
//! file that has them. The table writes the keys for a record that has them and nothing
//! for one that has not.
//!
//! # Id limits
//!
//! A term and a sequence are written as TOML integers, so each stops at `i64::MAX`. The
//! last sequence whose successor fits is issued; after it [`AcquireError::IdsExhausted`]
//! refuses every acquire, and a file whose term has no successor is an
//! [`OpenError::TermExhausted`]. Nothing wraps or saturates into a repeated id; a file
//! edited past the limit is refused the same way.

mod book;
mod clock;
mod error;
mod reservations;
mod state;
mod store;
mod table;
mod timings;
mod types;

pub use clock::{Clock, ManualClock, Monotonic, SystemClock, SystemMonotonic, Timestamp};
pub use error::{AcquireError, OpenError, ReleaseError, RenewError, SaveError, SettingsError};
pub use state::LeaseState;
pub use table::{LeaseTable, read_bookings};
pub use timings::{Timer, Timings};
pub use types::{
    Booking, Boot, Exclusion, ExclusionEnd, Held, Lease, LeaseInfo, Queued, ReleaseOutcome,
    ReservationRecord, Settings, TickReport,
};
