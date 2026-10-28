//! `js-guest`: the agent inside a macOS VM guest. It is started as a LaunchAgent of the
//! guest's non-admin auto-login user, refuses to run as root, and runs exactly one command
//! per boot for the host. Its executable is named `kbf-guest` (see this package's manifest).
//!
//! - [`wire`]: the framed, versioned messages, byte for byte (the codec is the
//!   `js-guest-wire` crate, re-exported here under its old paths).
//! - [`server`]: the handshake (version and per-boot token), the one-command rule,
//!   `Kill`, the timeout, and the exit report.
//! - [`run`]: the working directory checked against the inputs share, stdout and stderr
//!   written to the outputs share, the command's process group, `wait4` usage, and the
//!   report on requested outputs.
//! - [`client`]: the host side.
//! - [`session`]: the launchd session type `Ready` reports, and the root refusal.
//!
//! - [`mount`]: `mount-share`, the guest's own bounded virtiofs mount.
//! - [`frame`]: the frames probe results and ready markers travel in over virtio-socket.
//! - [`shutdown`]: the root daemon that listens on a virtio-socket port for the host's
//!   `shutdown` command and shuts the guest down cleanly.
//! - [`sim`]: the iOS simulator probe (`kbf-guest sim`), its steps, frames and deadlines;
//!   [`sim_run`] runs one command under a deadline.
//! - [`mkuser`]: the guest's user (`kbf-guest mkuser`), created once by `image prime`.
//! - [`simpaths`]: where `kbf-vmm image setup` stages Xcode and the simulator runtimes.
//! - [`probe`]: the probe modes `kbf-vmm probe g2` and `g3` read, which run as any user.
//!
//! It serves a Unix socket. The host side that calls it is a VM driver (over a fake
//! host's socket pair in tests).

// Moved from kbf, which has no `missing_docs` rule: the public items below still lack doc
// comments. Tracked in komira-ai/jobscheduler#32, which removes this allow.
#![allow(missing_docs)]

#[cfg(test)]
mod api_tests;
pub mod client;
pub mod diag;
pub mod mkuser;
pub mod mkuser_audit;
#[cfg(test)]
mod mkuser_audit_tests;
#[cfg(target_os = "macos")]
pub mod mkuser_host;
pub mod mkuser_prime;
#[cfg(test)]
mod mkuser_tests;
pub mod mount;
pub mod probe;
#[cfg(target_os = "macos")]
pub mod probe_host;
pub mod run;
pub mod server;
pub mod session;
pub mod shutdown;
#[cfg(target_os = "macos")]
pub mod shutdown_host;
pub mod sim;
pub mod sim_choose;
#[cfg(target_os = "macos")]
pub mod sim_host;
pub mod sim_parse;
pub mod sim_primed;
pub mod sim_run;
#[cfg(test)]
mod sim_tests;
pub mod sim_user;
pub mod sim_xctest;
pub mod simpaths;
#[cfg(test)]
mod test_dir;
pub mod vsock;

pub use js_guest_wire::{frame, wire};
