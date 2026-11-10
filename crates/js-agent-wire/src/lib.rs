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
//! [`Command::validate`] (a start is [`JobSpec::validate`], a job start
//! [`JobStartSpec::validate`]), and the parts they hold ([`HeldRun`], [`RunResult`],
//! [`HeldJob`], [`JobResult`], [`Taint`], [`ResultAck`], [`Mount`], [`Refused`]). A
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
//! the commands beside it. Three values are the deliberate exceptions, because skipping
//! them would be unsafe:
//!
//! - an unknown [`Phase`] reads as [`Phase::Quarantined`], which takes no work;
//! - an unknown [`Isolation`] value is a decode error (inside a reply, that one start reads
//!   as [`Command::Unknown`]), so an agent never runs a job it cannot classify, while an
//!   absent one means [`Isolation::Contained`];
//! - an unknown `JobSpec::network` level is a decode error in the same way, so an agent
//!   never runs a job at a level it did not name, while an absent one means `none`;
//! - an unknown `os` or `arch` of a start ([`JobSpec::os`], [`JobSpec::arch`]) is a decode
//!   error in the same way, so an agent never runs an image built for a platform it cannot
//!   name, while an absent one is a legacy start.
//!
//! A field an old receiver would silently ignore cannot carry something the job needs.
//! `JobSpec::files` is such a field (a presigned URL is a secret the task cannot run
//! without), so an agent that can write them reports the capability `run-files` in its
//! hello, and the server queues and sends a start with files only to a host that did.
//! `JobSpec::run` is another (an older agent would run the image's own command instead):
//! a server sends it, with `os` and `arch`, only to a host that reported `one-job`
//! ([`ONE_JOB_CAPABILITY`]), and writes the start in the legacy forms for any other
//! ([`JobSpec::downgrade_for`]).
//! `JobSpec::network` and `JobSpec::ports` are two more: an agent reports its network ceiling
//! in its hello ([`NetworkCeiling`]: an absent one is `none` with no port), and the server
//! sends a start that asks for more than `none`, or declares a port, only to a host whose
//! ceiling covers it.
//! `JobStartSpec::kind` is one more ([`StartKind`]: an older agent would run a VM start as
//! an action): a server sends `kind: vm` only to a host whose hello reports `vm.slots`, the
//! capability an agent with the VM backend sends, and an unknown kind is a decode error
//! like an unknown `Isolation`.
//! `JobStartSpec::isolation` is one more ([`Isolation`]: an older agent would run a host start
//! contained): a server sends `isolation: host` only to a host whose hello reports
//! `host-jobs` ([`HOST_JOBS_CAPABILITY`]), and leaves the field out for a contained start.
//!
//! # Live logs
//!
//! The live-log fields are a message set added within major 1 (`docs/design/live-logs.md`,
//! section 7; see [`LogSink`]): `log_sink` in a job start ([`JobStartSpec::log_sink`], where
//! to upload one job's output, no secret), `log_grant` in a reply
//! ([`HeartbeatReply::log_grant`], the host's upload credential, one for all its jobs),
//! `log_grant_state` in a heartbeat ([`Heartbeat::log_grant_state`]) and `logs` in a job's
//! result ([`JobResult::logs`], each stream's length and dropped bytes). An agent that
//! uploads reports the hello capability [`LOG_UPLOAD_CAPABILITY`], and a server sends
//! `log_sink` and `log_grant` only to an agent that reported it.
//!
//! **A log field never refuses a job or a heartbeat.** Each is decoded as a [`NodeLoad`] is
//! (a malformed value reads as absent) and is checked apart from its message: a start with
//! a bad sink still passes [`JobStartSpec::validate`] and runs without live logs
//! ([`JobStartSpec::log_sink_checked`]), a bad grant is set aside in
//! [`ReplyCheck::log_grant`] while the rest of the reply applies, and an inconsistent `logs`
//! report is dropped by [`JobResult::logs_report`] while the result is recorded.
//!
//! # Two keyings in one major
//!
//! A start, stop and report are keyed by run and lease ([`Command::Start`], [`HeldRun`],
//! [`RunResult`]) or by job ([`Command::JobStart`], [`Command::JobStop`], [`HeldJob`],
//! [`JobResult`], [`JobResultAck`], and the `retired_jobs` and `retired_acks` lists; see
//! [`per_job`]). The job messages are a message set added within major 1 and announced by
//! the hello capability [`PER_JOB_CAPABILITY`], so one server speaks to agents of both
//! kinds (see [`protocol`]). An agent lists what a job start began only under the job
//! keying, so a server that knows no job ids never sees one in `held` or `results`.
//!
//! # Not here yet
//!
//! Fields of design 5.2 that a later change adds, as optional fields of the types here:
//!
//! - the start's `booking`, `deadline`, named `assignments` beyond `gpu_indices`, and
//!   environment by reference: the run-delivery change, which is where a start is first
//!   sent (and where the server fills `valid_for_ms`). The fence of a job start is the
//!   pair of its job id and allocation id, which it already carries; an agent that
//!   enforces it says so with the hello capability [`START_FENCE_CAPABILITY`];
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
//! carries [`RunResult`]s (and [`HeldJob`]s with their [`JobResult`]s) up until each is
//! acknowledged and may carry a [`NodeLoad`]
//! (telemetry: a bad one is dropped, never the beat). [`refusal`] holds the codes a
//! refusal carries.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod caps;
mod check;
mod command;
mod credential;
mod env;
mod error;
mod files;
mod heartbeat;
mod hello;
mod image;
mod kind;
pub mod limits;
mod load;
mod log_sink;
mod native;
mod network;
pub mod per_job;
mod platform;
pub mod protocol;
pub mod refusal;
mod run;

pub use caps::{Caps, LOG_UPLOAD_CAPABILITY, ONE_JOB_CAPABILITY, ReadModel, bundle_capability};
pub use command::{Command, Isolation, JobSpec, Limits, Mount};
pub use credential::HostCredential;
pub use env::check_env;
pub use error::{Rule, ValidationError};
pub use files::{RunFile, validate_files};
pub use heartbeat::{
    Ack, ExclusiveRefusal, Heartbeat, HeartbeatReply, HeldRun, HeldState, LogTail, Outcome,
    RefusalReason, RefusedCommand, ReplyCheck, ResultAck, RunResult, Taint,
};
pub use hello::{
    GpuDevice, Hello, HelloReply, InstanceIdentity, Phase, Refused, Welcome, WireAccelerator,
    WireShape,
};
/// The cloud's signed identity of a machine ([`Hello::identity`]): `js-elastic`'s type,
/// the one the providers verify.
pub use js_elastic::SignedIdentity;
pub use kind::StartKind;
pub use load::{LeaseClock, LeaseMinute, LoadDrops, NodeLoad, lease_parts};
pub use log_sink::{LogGrant, LogGrantState, LogReport, LogSink};
pub use native::{
    BUNDLE_ENTRYPOINT_MAX, BUNDLE_PROGRAM_PREFIX, InputRef, NativeSpec, NetworkLevel, OutputKind,
    OutputSpec, Placement, TreeUnavailable,
};
pub use network::{EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, NetworkCeiling, PortRange};
pub use per_job::{
    BookedResources, HOST_JOBS_CAPABILITY, HeldJob, JobResult, JobResultAck, JobStartSpec,
    PER_JOB_CAPABILITY, START_FENCE_CAPABILITY,
};
pub use platform::{ImagePlatform, check_platform};
pub use protocol::{PROTOCOL_MAJOR, PROTOCOL_NAME, ProtocolRange};
pub use run::RunSpec;

/// The heartbeat interval the server proposes in [`Welcome`] unless its configuration
/// says otherwise, in milliseconds.
pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 5_000;

/// How long a host may stay silent before the server treats it as lost, unless its
/// configuration says otherwise, in milliseconds.
pub const DEFAULT_GRACE_MS: u64 = 15_000;

/// The longest `Retry-After` an agent obeys: a server that asks for more is not allowed
/// to silence the host for longer. It bounds the wait before an agent's next hello, so a
/// server that restarts must allow for it before it reads a missing host as gone.
pub const RETRY_AFTER_CAP: std::time::Duration = std::time::Duration::from_secs(120);

/// The hellos one credential may say in a burst, unless the server's configuration says
/// otherwise (`--agent-hello-burst`).
pub const DEFAULT_HELLO_BURST: u32 = 60;

/// How often a credential that spent its burst is allowed one more hello, unless the
/// server's configuration says otherwise (`--agent-hello-interval`).
pub const DEFAULT_HELLO_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a grace period is long enough for the heartbeat interval: more than two
/// intervals, so one lost heartbeat never makes a live host look lost.
pub const fn grace_covers_heartbeats(heartbeat_interval_ms: u64, grace_ms: u64) -> bool {
    grace_ms > heartbeat_interval_ms.saturating_mul(2)
}

const _: () = assert!(
    grace_covers_heartbeats(DEFAULT_HEARTBEAT_INTERVAL_MS, DEFAULT_GRACE_MS),
    "the default grace must exceed two default heartbeat intervals"
);
