//! The pool of Mac users and what runs as them.
//!
//! This is a pure crate: no clock, no sleep, no hashed collection, and no socket in the
//! library code. The `disallowed_types` deny is lib-only, so tests (the runner tests open
//! sockets) are exempt. It builds text and argument vectors and starts nothing, so every
//! rule in it is tested on Linux.
//!
//! - [`seatbelt`]: the `sandbox-exec` profile of one run, from a typed
//!   [`Policy`](seatbelt::Policy), in two read models, with the start-up self-test of the
//!   allow-list one.
//! - [`xcode`]: the catalog of the Xcodes of this Mac, built from `--xcode BUILD=PATH`
//!   flags; only ready Xcodes are advertised or granted.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]
#![allow(dead_code, unused_imports, unused_variables)]

pub mod seatbelt;
pub mod xcode;
