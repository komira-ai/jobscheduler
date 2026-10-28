//! The pure rules of whole-node placement: the caps on how many hosts may be reserved
//! or held whole, the cost that orders the hosts to drain, the reservation deadline, and
//! the pool settings of the class.
//!
//! This is a pure crate: no clock, no sleep, no lock, no hashed collection, no socket.
//! Each rule is a function of plain structs and plain integers (milliseconds), so a
//! caller decides under its own lock on facts it has just read, and a test states a case
//! without building a pool.
//!
//! - [`cost`]: which host a reservation drains first, and the deadline it gets.
//! - [`caps`]: the effective ceiling, the cap that blocks a reservation, the
//!   reservations to drop when the ceiling falls, and the last-capable-host rule.
//! - [`config`]: [`WholeNodeConfig`], the settings with their defaults.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod caps;
pub mod config;
pub mod cost;

pub use config::{
    DEFAULT_PREPARE_TIMEOUT_MS, DEFAULT_REGAIN_GRACE_MS, DEFAULT_RESERVE_BACKOFF_MS,
    WholeNodeConfig,
};
