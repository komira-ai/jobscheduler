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
//! - **Booked shape.** A host is booked as the pool's configured shape
//!   ([`PoolConfig::new`]), and the host's claim only has to offer at least that. A
//!   claim bigger than the pool's shape is never leased out, so an agent cannot inflate
//!   what a lease holds.
//! - **Loss.** A host silent for more than the grace period is lost: it takes no new
//!   lease, and the lease it holds stays booked and reads `Unknown` (never `Absent`)
//!   until the host is heard again or the lease is stopped.
//! - **Phase.** Only a serving host takes a new lease. A phase the wire does not know
//!   reads as quarantined, which takes none.
//! - **Restart.** The lease file is the record of leases and hosts re-announce
//!   themselves, so [`RegisteredProvider::seat`] puts a booking back on an awaiting host
//!   that the agent's next hello completes.

mod config;
mod credential;
mod provider;
mod refusal;
mod registry;

pub use config::PoolConfig;
pub use credential::Credential;
pub use provider::RegisteredProvider;
pub use refusal::Refusal;
pub use registry::{HostState, HostView, Registry};
