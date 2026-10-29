//! The messages a host agent and the lease server exchange, and the rules that decide
//! whether one is acceptable.
//!
//! This crate is types only: it does no I/O, reads no clock and names no transport. The
//! bodies are serde types, so any carrier of JSON can move them (the first is HTTP/1.1
//! polling; a stream can replace it later without touching these types). They map to
//! sections 5.2 and 5.4 of the design, and to 5.12 for [`Isolation`] and [`Taint`].
//!
//! # This crate is the validation layer of a trust boundary
//!
//! An agent is a semi-trusted machine; the server is trusted but may be older or newer
//! than the agent. Decoding proves only that a body has the right shape. **Every message
//! has a `validate()` and the server and the agent must call it on every message they
//! receive, before they act on any field:** [`Hello::validate`], [`Welcome::validate`]
//! (and [`HelloReply::validate`]), [`Heartbeat::validate`], [`HeartbeatReply::validate`],
//! [`Command::validate`] (a start is [`JobSpec::validate`]), and the parts they hold
//! ([`HeldRun`], [`RunResult`], [`Taint`], [`ResultAck`], [`Mount`], [`Refused`]). A
//! validator reads no state and no clock; whether an epoch is current, a lease is held
//! or a shape fits a pool is the caller's. [`JobSpec::fits_shape`] answers the one
//! question of that kind a single job and a single shape can answer.
//!
//! Every untrusted string and list is bounded by a named constant in [`limits`], every
//! id and the image have an alphabet, and a number has a range. This crate cannot bound
//! the size of a body, so a transport must cap what it reads before it decodes.
//!
//! A refusal never repeats the peer's data: [`ValidationError`]'s `Display` is built from
//! a field name and a rule alone, and [`ValidationError::code`] is the stable output a
//! peer matches on (the codes are in [`refusal`]).
//!
//! That claim is about [`ValidationError`] only. A serde decode error does echo peer
//! text (it names the unknown variant or the value it could not read, in full), so a
//! transport must never forward or log the error of a failed decode; it answers with a
//! fixed refusal instead. Inside a [`HeartbeatReply`] a command that fails to decode
//! reads as [`Command::Unknown`] and its error is dropped here.
//!
//! A reply is validated command by command ([`ReplyCheck`]): one refused start never
//! costs a stop or cancel beside it, and the agent acts on every valid command.
//!
//! # Protocol version
//!
//! See [`protocol`]: a hello states the majors the agent speaks, a welcome names the one
//! chosen, and a server refuses a hello it cannot serve with
//! [`refusal::UNSUPPORTED_PROTOCOL`]. Servers upgrade before agents.
//!
//! # Compatibility rule
//!
//! `Welcome::session` is required, so a new server with an old agent gives
//! `session-mismatch` and a new agent with an old server gives `invalid-welcome`, both
//! within protocol major 1. That is accepted and the major stays 1: no agent or server
//! of major 1 has been released before the session field. From the first release on,
//! fields are additive, as follows.
//!
//! Within a major every field is additive. A receiver ignores fields it does not know, in
//! both directions, so a newer peer never breaks an older one: no body here uses
//! `deny_unknown_fields`, and a field a sender may leave out has a default on decode. An
//! enum value this version does not know reads as an `Unknown` variant ([`Outcome`],
//! [`Ack`], [`HeldState`], [`Command`]) and never fails the message around it; a reply
//! decodes its commands one by one, so one command this version cannot read does not cost
//! the commands beside it. Two values are the deliberate exceptions, because skipping
//! them would be unsafe:
//!
//! - an unknown [`Phase`] reads as [`Phase::Quarantined`], which takes no work;
//! - an unknown [`Isolation`] value is a decode error (inside a reply, that one start reads
//!   as [`Command::Unknown`]), so an agent never runs a job it cannot classify, while an
//!   absent one means [`Isolation::Contained`].
//!
//! # Not here yet
//!
//! Fields of design 5.2 that a later change adds, as optional fields of the types here:
//!
//! - the start's `fence`, `booking`, `deadline`, named `assignments` beyond `gpu_indices`,
//!   and environment by reference: the run-delivery change, which is where a start is
//!   first sent (and where the server fills `valid_for_ms`);
//! - the sum of several jobs' limits against one shape, and checks against pool state
//!   (the pool's shape, `max_machines`, the tainted set): the server's registry. The
//!   credential binding is there now, in `js-registry` (`Binding`, checked by
//!   `Registry::hello` and `Registry::heartbeat`);
//! - the `Usage` message (what a run measurably used, with the `basis` of the
//!   measurement) and the per-message extension slot design 5.2 and 10.3 describe: the
//!   run-delivery change, the first that has a measured run to report and a field a
//!   major could not add on its own. Neither exists in this crate yet. What exists is the
//!   host's own load, [`NodeLoad`]: an optional, lenient part of a heartbeat that reports
//!   per-minute CPU and memory per lease, never a run's `Usage`.
//!
//! # Layout
//!
//! [`Hello`] opens a session and is answered by a [`Welcome`] or a [`Refused`] (together
//! a [`HelloReply`]; over HTTP the welcome is bare and a refusal is the API's
//! `{"error", "supported"}` body, see [`HelloReply`]), [`Heartbeat`] and
//! [`HeartbeatReply`] keep it, and a reply carries [`Command`]s down while a heartbeat
//! carries [`RunResult`]s up until each is acknowledged and may carry a [`NodeLoad`]
//! (telemetry: a bad one is dropped, never the beat). [`refusal`] holds the codes a
//! refusal carries.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod check;
mod command;
mod credential;
mod error;
mod heartbeat;
mod hello;
mod image;
pub mod limits;
mod load;
mod native;
pub mod protocol;
pub mod refusal;

pub use command::{Command, Isolation, JobSpec, Limits, Mount};
pub use credential::HostCredential;
pub use error::{Rule, ValidationError};
pub use heartbeat::{
    Ack, ExclusiveRefusal, Heartbeat, HeartbeatReply, HeldRun, HeldState, Outcome, RefusalReason,
    RefusedCommand, ReplyCheck, ResultAck, RunResult, Taint,
};
pub use hello::{
    GpuDevice, Hello, HelloReply, InstanceIdentity, Phase, Refused, Welcome, WireShape,
};
pub use load::{LeaseClock, LeaseMinute, LoadDrops, NodeLoad, lease_parts};
pub use native::{InputRef, NativeSpec, NetworkLevel, OutputKind, OutputSpec, Placement};
pub use protocol::{PROTOCOL_MAJOR, PROTOCOL_NAME, ProtocolRange};

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
