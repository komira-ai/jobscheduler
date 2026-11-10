//! The hosts of a pool that register themselves, and the `static` machine provider over the
//! same host table ([`StaticCapacity`], a [`js_elastic::CapacityProvider`] that never
//! launches).
//!
//! An operator of a static pool lists every host in a file. Here a machine says hello
//! (design 5.8): it names its id, the pool it wants to join and the shape it offers, and
//! the registry admits it or says why not. The job engine then places jobs on admitted hosts.
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
//!   token can say hello for an unbound-instance host id once that host is lost. Per-host
//!   tokens or client certificates close that later (design D18). [`Credential`] equality
//!   is on a digest; the constant-time compare of the token belongs to the transport.
//! - **Session secret.** Every admitted hello is answered with a fresh secret from the
//!   registry's [`SessionSource`] ([`OsSessions`] in production); the registry keeps its
//!   SHA-256 digest only. A heartbeat must carry the secret issued at that host's last
//!   hello, compared as digests in constant time: one that lacks it or carries another is
//!   refused as `session-mismatch` and changes nothing, so a holder of the pool token,
//!   who also knows the host id and epoch, cannot keep a host live, revive it or set its
//!   phase. A new hello replaces the secret. The one way in for a pool-token holder is a
//!   hello, and only for an id that is not instance-bound and is lost, as above.
//! - **Retention, and what it does not do.** A lost host that its placer holds no work on
//!   is forgotten, binding included, after [`PoolConfig::with_lost_retention`], so ids made
//!   up by a token holder who then goes silent stop filling the pool. That is all it
//!   bounds: silent squatters. A holder of the pool token can still lock the pool out,
//!   because it can say hello for `max_machines` made-up ids and keep them live with the
//!   sessions it was issued, or refill the pool every grace plus retention. The transport
//!   must rate-limit hellos per credential, and per-host credentials (design D18) close
//!   it; the registry has no per-credential cap. A host its placer holds work on
//!   ([`Placer::holds_work_on`]) is never forgotten.
//! - **Loss.** A host silent for more than the grace period is lost: it takes no new job.
//! - **Phase.** Only a serving host takes a new job. A phase the wire does not know
//!   reads as quarantined, which takes none.
//! - **Heartbeat reply.** The registry queues no run. A previous release's agent that
//!   still keys its work by run is answered with a cancel for each run it lists as held and
//!   an `UnknownLease` acknowledgement for each result it reports, so it kills what it holds
//!   and drops what it reports ([`Registry::heartbeat`]). The job half of the reply is the
//!   transport's.
//! - **Taint.** The first taint a host reports, or that the caller sets with
//!   [`Registry::taint_host`] (the first one stands), is kept with its binding
//!   ([`Binding::taint`], [`Registry::bindings`]), so that a caller that persists
//!   bindings keeps it across a restart ([`Registry::restore_tainted`]). A tainted host
//!   takes no new job and is never forgotten for being silent. The registry never clears a
//!   taint.
//! - **Cordon.** A host can cordon itself for a while ([`Registry::cordon`]): it takes no
//!   new job until the cordon lapses or [`Registry::uncordon`] ends it, and the work it
//!   has is untouched. The requester sets the length, a repeat renews and never shortens,
//!   and nothing ticks: the lapse is read when a host is placed. The cordon is kept with
//!   the binding ([`Binding::with_cordon`]) and put back by [`Registry::restore_tainted`],
//!   so a restart keeps it; a host that is cordoned is not forgotten for being silent.
//! - **Scale-down marks.** A scaler that gives hosts back marks them in two steps,
//!   [`Registry::drain`] and then [`Registry::release_host`] ([`HostMark::Draining`],
//!   [`HostMark::Releasing`]). A marked host takes no new job. Each call sets its mark only
//!   if the host is quiet (it lists no run) and checks that under the registry's lock; a
//!   busy host answers [`Busy`] and keeps its mark. [`Registry::undrain`] ends a drain,
//!   [`Registry::remove`] drops a releasing host and frees its place in the machine limit.
//!   The marks have no deadline and are in memory only.
//! - **Whole-host jobs.** [`prepare_exclusion`] is the table of what a failed prepare does
//!   to its host, which the job engine reads, and [`Registry::taint_after_prepare`] records
//!   the taint of a `host-unsafe` refusal.

mod capacity;
mod config;
mod credential;
mod load;
mod placer;
mod refusal;
mod registry;
mod session;
mod whole;

pub use capacity::StaticCapacity;
pub use config::{
    ConfigError, DEFAULT_LOST_RETENTION_MS, MAX_LOST_RETENTION_MS, MIN_LOST_RETENTION_MS,
    PoolConfig,
};
pub use credential::{Binding, Credential};
pub use js_whole::{
    DEFAULT_PREPARE_TIMEOUT_MS, DEFAULT_REGAIN_GRACE_MS, DEFAULT_RESERVE_BACKOFF_MS,
    WholeNodeConfig,
};
pub use load::{BadLease, node_load};
pub use placer::Placer;
pub use refusal::{Refusal, ShapeFault};
pub use registry::{
    Busy, Cordoned, HeartbeatCheck, HostMark, HostState, HostView, NotHeld, OwnHost, Registry,
    Uncordoned,
};
pub use session::{OsSessions, SequentialSessions, SessionSource};
pub use whole::requests::{Exclusion, prepare_exclusion};
