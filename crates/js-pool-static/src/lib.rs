//! A static pool provider: hosts an operator lists in a TOML file, leased one whole
//! host at a time.
//!
//! The pool serves `gpu-host` ([`js_types::ResourceClass::gpu_host`], admitted
//! `BookAll`) unless [`StaticPool::with_classes`] names others, such as `whole-node`
//! ([`js_types::ResourceClass::whole_node`], admitted `Whole`). In both a lease holds
//! a whole host, so nothing else runs on it. The pool refuses an admission mode that does not book a whole host with
//! `UnsupportedClass` rather than leasing a whole host anyway. Its rules, in design
//! terms:
//!
//! - **Fit (6.1).** A host fits when its shape offers at least the requested
//!   resources, with the requested GPU class, and reports every required capability.
//! - **Rank (6.1).** The first free host by id (byte order of the id) among those that
//!   fit. There is no load term: with whole-host leases every free fitting host is
//!   equal, so the id order makes placement deterministic.
//! - **Stopped leases.** A stopped lease id never starts again: a late or duplicate
//!   start after the stop is refused with `LeaseEnded`, books nothing, and the lease
//!   observes `Absent`. A stop of a lease the pool never held is remembered the same way.
//! - **Absence (5.6).** The pool owns its state, so it knows absence exactly:
//!   [`js_provider::Provider::observe`] answers `Present` or `Absent`, never
//!   `Unknown`, and a stop frees the host at once.
//! - **Seating.** The pool's bookings live in memory and a restart loses them.
//!   [`StaticPool::seat`] books a named host for a named lease without a start, so a
//!   caller that has the bookings elsewhere (a lease file) can put them back; it refuses
//!   a host the pool does not list or another lease holds ([`SeatError`]).
//!
//! # Configuration
//!
//! [`StaticPool::from_config_file`] takes the path of the file, which the caller
//! reads from its own `--pool-config` style flag. The file is TOML; an unknown key is
//! refused so a typo does not pass as a default, and a syntax or type error names the
//! line. This is the example file shipped with the crate:
//!
#![doc = concat!("```toml\n", include_str!("../example.toml"), "```")]

mod book;
mod config;
mod pool;

pub use book::{HostBook, SeatError};
pub use config::{ConfigError, EXAMPLE_CONFIG};
pub use pool::StaticPool;
