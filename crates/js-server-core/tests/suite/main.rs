//! The core's tests, in one test binary.
//!
//! The core is generic in its adapter, and a test binary's run of it counts toward the
//! instantiation it was built with. One binary and one adapter ([`common::Rig`]) keep every
//! path of the shell in one instantiation.

mod admission;
mod common;
mod feed;
mod placement;
mod session;
