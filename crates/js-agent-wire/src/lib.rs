//! The messages a host agent and the lease server exchange, and the rules that decide
//! whether one is acceptable.
//!
//! This crate is types only: it does no I/O, reads no clock and names no transport. The
//! bodies are serde types, so any carrier of JSON can move them (the first is HTTP/1.1
//! polling; a stream can replace it later without touching these types). They map to
//! sections 5.2 and 5.4 of the design, and to 5.12 for [`Isolation`] and [`Taint`].
//!
//! # Compatibility rule
//!
//! Every field is additive. A receiver ignores fields it does not know, in both
//! directions, so a newer peer never breaks an older one: no body here uses
//! `deny_unknown_fields`, and a field a sender may leave out has a default on decode. An
//! enum value this version does not know reads as an `Unknown` variant ([`Outcome`],
//! [`Ack`], [`HeldState`], [`Command`]) and never fails the message around it. Two
//! values are the deliberate exceptions, because skipping them would be unsafe:
//!
//! - an unknown [`Phase`] reads as [`Phase::Quarantined`], which takes no work;
//! - an unknown [`Isolation`] value is a decode error (inside a reply, that one start reads
//!   as [`Command::Unknown`]), so an agent never runs a job it cannot classify, while an
//!   absent one means [`Isolation::Contained`].
//!
//! # Layout
//!
//! [`Hello`] and [`Welcome`] open a session, [`Heartbeat`] and [`HeartbeatReply`] keep
//! it, and a reply carries [`Command`]s down while a heartbeat carries [`RunResult`]s up
//! until each is acknowledged. [`refusal`] holds the codes a refusal carries.

mod command;
mod heartbeat;
mod hello;
pub mod refusal;

pub use command::{Command, Isolation, JobSpec, JobSpecError, Limits, Mount, MountError};
pub use heartbeat::{
    Ack, Heartbeat, HeartbeatReply, HeldRun, HeldState, Outcome, ResultAck, RunResult, Taint,
};
pub use hello::{GpuDevice, Hello, HelloError, Phase, Welcome, WireShape};

/// The heartbeat interval the server proposes in [`Welcome`] unless its configuration
/// says otherwise, in milliseconds.
pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 5_000;

/// How long a host may stay silent before the server treats it as lost, unless its
/// configuration says otherwise, in milliseconds.
pub const DEFAULT_GRACE_MS: u64 = 15_000;

/// Whether a grace period is long enough for the heartbeat interval: more than two
/// intervals, so one lost heartbeat never makes a live host look lost.
pub const fn grace_covers_heartbeats(heartbeat_interval_ms: u64, grace_ms: u64) -> bool {
    grace_ms > heartbeat_interval_ms.saturating_mul(2)
}

const _: () = assert!(
    grace_covers_heartbeats(DEFAULT_HEARTBEAT_INTERVAL_MS, DEFAULT_GRACE_MS),
    "the default grace must exceed two default heartbeat intervals"
);
