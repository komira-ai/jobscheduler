//! The hosts of a pool that register themselves, and a [`js_provider::Provider`] over
//! them.
//!
//! An operator of a static pool lists every host in a file. Here a machine says hello
//! (design 5.8): it names its id, the pool it wants to join and the shape it offers, and
//! the registry admits it or says why not. The lease table then leases admitted hosts
//! through [`RegisteredProvider`] the way it leases a static pool's, so a consumer asks
//! for a lease and never learns which kind of host it got.
//!
//! This crate is the decisions only: no network, no commands to the host, no files. The
//! transport that carries [`js_agent_wire::Hello`] and [`js_agent_wire::Heartbeat`]
//! into [`Registry::hello`] and [`Registry::heartbeat`] is a layer above it, and the
//! only clock is the one the registry is given.
//!
//! # What the registry decides
//!
//! - **Admission** ([`Registry::hello`]): the wire rules, the pool, the credential a host
//!   id is bound to, a live session of the same id, the shape, room in the pool. Every
//!   refusal is a [`Refusal`] with a constant wire code and changes nothing.
//! - **Binding, and what it does not do.** A host id binds to the credential of its
//!   first hello and, when that hello named `Hello::instance`, to that instance
//!   ([`Binding`]); a later hello or heartbeat that does not match is refused as
//!   host-bound. Without an instance identity the binding is at the level of the pool
//!   credential only: all hosts of a pool share the join token, so a holder of the pool
//!   token can say hello for an unbound-instance host id once that host is lost, and
//!   takes over its booking. Per-host tokens or client certificates close that later
//!   (design D18). [`Credential`] equality is on a digest; the constant-time compare of
//!   the token belongs to the transport.
//! - **Session secret.** Every admitted hello is answered with a fresh secret from the
//!   registry's [`SessionSource`] ([`OsSessions`] in production); the registry keeps its
//!   SHA-256 digest only. A heartbeat must carry the secret issued at that host's last
//!   hello, compared as digests in constant time: one that lacks it or carries another is
//!   refused as `session-mismatch` and changes nothing, so a holder of the pool token,
//!   who also knows the host id and epoch, cannot keep a host live, revive it or set its
//!   phase. A new hello replaces the secret. The one way in for a pool-token holder is a
//!   hello, and only for an id that is not instance-bound and is lost, as above.
//! - **Retention, and what it does not do.** A lost host with no booking is forgotten,
//!   binding included, after [`PoolConfig::with_lost_retention`], so ids made up by a
//!   token holder who then goes silent stop filling the pool. That is all it bounds:
//!   silent squatters. A holder of the pool token can still lock the pool out, because
//!   it can say hello for `max_machines` made-up ids and keep them live with the
//!   sessions it was issued, or refill the pool every grace plus retention. The
//!   transport must rate-limit hellos per credential, and per-host credentials (design
//!   D18) close it; the registry has no per-credential cap. A booked host is never
//!   forgotten.
//! - **Booked shape.** A host is booked as the pool's configured shape
//!   ([`PoolConfig::new`]), and the host's claim only has to offer at least that. A
//!   claim bigger than the pool's shape is never leased out, so an agent cannot inflate
//!   what a lease holds.
//! - **Loss.** A host silent for more than the grace period is lost: it takes no new
//!   lease, and the lease it holds stays booked and reads `Unknown` (never `Absent`)
//!   until the host is heard again or the lease is stopped.
//! - **Phase.** Only a serving host takes a new lease. A phase the wire does not know
//!   reads as quarantined, which takes none.
//! - **Runs.** [`Registry::queue_run`] queues a container run for the host a lease
//!   holds, and [`Registry::heartbeat`] answers with it. A start is carried on every
//!   reply until the host lists the run; a stop is a retained intent carried on every
//!   reply until the host no longer lists it; a run the host lists that the registry
//!   holds no live record of is answered with a cancel, never adopted. A result gets an
//!   acknowledgement (accepted, duplicate, stale, unknown lease, or mismatch for a run
//!   of another host). The limits of a run count against the booked shape until the run
//!   leaves the host's held list, which the host does only after cleanup. When a lease is
//!   stopped with live runs, the booking is freed at once but the host is not bookable
//!   until a heartbeat lists none of them. Run records are in memory only: a restart
//!   forgets them, and nothing here is persisted. A host that lists a run the registry
//!   has no record of (after a restart, say) is told to cancel it, and the agent kills it
//!   without a result: the work is lost, even though its lease may still hold the host.
//!   A run is started from the lease time it has left at each reply, and not at all once
//!   that is gone.
//! - **Taint.** The first taint a host reports is kept with its binding
//!   ([`Binding::taint`], [`Registry::bindings`]), so that a caller that persists
//!   bindings keeps it across a restart ([`Registry::restore_tainted`]). A tainted host
//!   is sent no start, takes no new run and no new lease (a lease already on it is left
//!   alone until it ends), and is never forgotten for being silent; its
//!   live runs finish and are acknowledged. The registry never clears a taint.
//! - **Whole-node commit.** A lease of a `Whole` class is committed, not granted
//!   (design `whole-node.md` sections 5 and 8): the registry books it only on a
//!   host that is live, serving, clean, quiet and clear of another whole-node lease or a
//!   restore, and answers `Unknown`, so the lease stays pending. The host's heartbeat
//!   replies name the lease in `exclusive`; the lease reads `Unknown` until the agent
//!   reports `exclusive_ready` for exactly that lease, then `Present`, which is when a
//!   consumer is given its token. Until then no run is queued
//!   ([`RunRefusal::NotReady`]) or started. A new session (hello) voids the
//!   preparation. When the lease ends the host stays unbookable until two heartbeats
//!   have followed the stop and the last shows it serving with nothing held. A `BookAll`
//!   lease is granted at once and none of this applies; neither does it to a static
//!   pool, which has no agent to wait for and holds a `Whole` lease at once. The state
//!   is in memory only.
//! - **Reservation and drain.** A whole-node request that carries an id and finds no
//!   host to commit waits ([`Registry::queue_position`]) and, on each tick
//!   ([`Registry::advance`], run by every observe), is given a host in queue order
//!   (design `whole-node.md` sections 3, 4 and 6): a quiet host at once, else after
//!   `drain_after` the host that costs least to drain, which then takes no other
//!   request's start and no new lease. The request's own next start commits the lease on
//!   it ([`Registry::reservations`]). Nothing is stopped for a reservation; one that
//!   reaches its deadline with the host still busy is given up, the host is left out for
//!   the request for `reserve_backoff`, and after `max_reserve_attempts` give-ups the
//!   next deadline is the wall time and the reservation commits. Cancelling the request
//!   or letting it expire drops the reservation at once. Reservations are in memory only.
//! - **Restart.** The lease file is the record of leases and hosts re-announce
//!   themselves, so [`RegisteredProvider::seat`] puts a booking back on an awaiting host
//!   that the agent's next hello completes. The seat takes the binding the caller
//!   persisted; with none, the first hello for that id binds it and takes the lease,
//!   from any holder of the pool token. That is the restart gap a caller that persists
//!   bindings closes.

mod config;
mod credential;
mod provider;
mod refusal;
mod registry;
mod runs;
mod session;
mod whole;
use js_whole::config as whole_config;

pub use config::{
    ConfigError, DEFAULT_LOST_RETENTION_MS, MAX_LOST_RETENTION_MS, MIN_LOST_RETENTION_MS,
    PoolConfig,
};
pub use credential::{Binding, Credential};
pub use js_whole::{
    DEFAULT_PREPARE_TIMEOUT_MS, DEFAULT_REGAIN_GRACE_MS, DEFAULT_RESERVE_BACKOFF_MS,
    WholeNodeConfig,
};
pub use provider::RegisteredProvider;
pub use refusal::Refusal;
pub use registry::{HostMark, HostState, HostView, Registry, WholeCounts};
pub use runs::{MAX_FINISHED_RUNS, MAX_LIVE_RUNS, RunRefusal, RunState, RunView};
pub use session::{OsSessions, SequentialSessions, SessionSource};
pub use whole::queue::MAX_WAITING_REQUESTS;
pub use whole::requests::{Exclusion, NotReady, RequestKey, RequestRecord};
pub use whole::reserve::Reservation;
