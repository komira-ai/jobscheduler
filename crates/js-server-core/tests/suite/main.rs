//! The core's tests, in one test binary.
//!
//! The core is generic in its adapter, and a test binary's run of it counts toward the
//! instantiation it was built with. One binary and one adapter ([`common::Rig`]) keep every
//! path of the shell in one instantiation; the simulated cell has an adapter of its own, and
//! its run of the shell adds to the checks, not to the count.

mod admission;
mod cell;
mod common;
mod curves;
mod feed;
mod frontends;
mod handover;
mod hooks;
mod intake;
mod learned;
mod outage;
mod owners;
mod placement;
mod predicted;
mod recover;
mod report;
mod session;
mod settings;
mod shared;
mod stored;
mod walled;
