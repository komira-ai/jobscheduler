//! The pool of Mac users and what runs as them.
//!
//! This is a pure crate: no clock, no sleep, no hashed collection, and no socket in the
//! library code. The `disallowed_types` deny is lib-only, so tests (the runner tests open
//! sockets) are exempt. It builds text and argument vectors and starts nothing, so every
//! rule in it is tested on Linux.
//!
//! - [`seatbelt`]: the `sandbox-exec` profile of one run, from a typed
//!   [`Policy`](seatbelt::Policy).

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod seatbelt;
