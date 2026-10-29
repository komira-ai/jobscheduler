//! The codes a refusal carries on the wire.
//!
//! They are plain constants with no central table: a code is text a deployed agent or
//! server already matches on, so renaming one breaks a peer that was not upgraded with
//! it. A later change adds a code; it never changes or removes one. The
//! `refusal_codes_snapshot` test fails on a rename.

/// A hello whose body is malformed: a bad id, list, label or shape.
pub const INVALID_HELLO: &str = "invalid-hello";
/// A hello (or run) whose labels use a key under the reserved `jobscheduler.dev/`
/// prefix.
pub const RESERVED_LABEL: &str = "reserved-label";
/// A job whose wall-time limit is zero.
pub const WALL_TIME_NOT_POSITIVE: &str = "wall-time-not-positive";
/// A job spec with a malformed id, argument list, limit or label.
pub const INVALID_JOB: &str = "invalid-job";
/// A job whose image name is empty, too long or outside the image alphabet.
pub const INVALID_IMAGE: &str = "invalid-image";
/// A job whose image is not pinned by digest: it has no `@sha256:<64 lower case hex>`
/// part (a tag alone, another algorithm, a digest of the wrong length). The refusal's
/// `field` is `image`. Agents run with the pull policy `missing`, so a tag could change
/// under a fleet; a name with a tag as well as a digest is accepted.
pub const IMAGE_DIGEST_REQUIRED: &str = "image-digest-required";
/// A job with a mount that is absolute, climbs with `..`, is empty or names no root.
pub const INVALID_MOUNT: &str = "invalid-mount";
/// A hello for a host id that is already live.
pub const HOST_LIVE: &str = "host-live";
/// A hello for a host id the scheduler holds as tainted; and, as the detail of a
/// `failed_to_start` result, a `Start` that a tainted agent refused. The second is terminal
/// for that `Start`: the run did not start on that host and the host will not start it
/// later, so the server must not send the same run to that host again.
///
/// The server must treat a `Start` refused with it as the host's taint: the agent's marker
/// may be unwritable, and then its heartbeat carries no taint although the host refuses
/// every start. It is the host's fault and not the run's, so it must not be counted against
/// the run's retries.
pub const HOST_TAINTED: &str = "host-tainted";
/// A heartbeat for a session epoch the server no longer holds.
pub const STALE_EPOCH: &str = "stale-epoch";
/// A heartbeat from a host the server does not know.
pub const UNKNOWN_HOST: &str = "unknown-host";
/// A run on a host whose lease is not held by the caller.
pub const NOT_HELD: &str = "not-held";
/// A run on a host that has stopped heartbeating.
pub const HOST_LOST: &str = "host-lost";
/// A run that does not fit the capacity still free on the lease.
pub const NO_CAPACITY: &str = "no-capacity";
/// A run on a host that did not register itself.
pub const HOST_NOT_REGISTERED: &str = "host-not-registered";
/// A run whose limits exceed the booked shape.
pub const LIMITS_EXCEED_SHAPE: &str = "limits-exceed-shape";
/// A host job where host jobs are not allowed.
pub const HOST_JOBS_NOT_ALLOWED: &str = "host-jobs-not-allowed";
/// A hello whose protocol range shares no major with the server's (see
/// [`crate::protocol`]).
pub const UNSUPPORTED_PROTOCOL: &str = "unsupported-protocol";
/// A heartbeat whose body is malformed: a bad id, list or result.
pub const INVALID_HEARTBEAT: &str = "invalid-heartbeat";
/// A welcome whose body is malformed: a zero interval, a grace that does not cover the
/// interval, a pool or protocol the agent cannot accept.
pub const INVALID_WELCOME: &str = "invalid-welcome";
/// A reply whose commands or acknowledgements are malformed.
pub const INVALID_REPLY: &str = "invalid-reply";
/// A hello naming a pool this server does not serve.
pub const UNKNOWN_POOL: &str = "unknown-pool";
/// A hello whose shape does not offer what the pool's shape promises.
pub const SHAPE_NOT_ACCEPTED: &str = "shape-not-accepted";
/// A hello from a new host id while the pool already holds as many hosts as it allows.
pub const POOL_FULL: &str = "pool-full";
/// A hello for a host id that was first admitted with another credential.
pub const HOST_BOUND: &str = "host-bound";
/// A heartbeat that does not carry the session secret issued at the host's last hello:
/// absent, or not the one issued. Nothing about the host changes.
pub const SESSION_MISMATCH: &str = "session-mismatch";
/// A hello from a host that lacks what the pool requires of every host: no `runtime`
/// capability, or no `gpu-toolkit` in a pool with GPUs. The refusal's `field` names the
/// capability. Only the operator can change it, so the agent does not ask again.
pub const HOST_CONTRACT: &str = "host-contract";
/// A hello from a host that reports another execution mode than the pool's `exec`: its
/// `exec` or its `runtime` names a different mode. The refusal's `field` names the
/// capability. Only the operator can change it, so the agent does not ask again.
pub const EXECUTION_MISMATCH: &str = "execution-mismatch";
/// A hello the server cannot answer because it could not make a session secret.
pub const SESSION_UNAVAILABLE: &str = "session-unavailable";
/// A hello for a host id the operator lists in a static pool: an id belongs to one
/// source of hosts only, so a registering host cannot take it.
pub const HOST_LISTED: &str = "host-listed";
/// A hello the server refuses because its credential has said hello too often; the agent
/// tries again later.
pub const RATE_LIMITED: &str = "rate-limited";
/// A request body over the transport's cap, refused before it is read in full.
pub const BODY_TOO_LARGE: &str = "body-too-large";

/// A whole-node lease request that failed after the host could not be prepared within the
/// allowed number of replacements; its detail names the last reason code and detail the
/// agent reported. Used by the scheduler, not by the agent.
pub const EXCLUSIVE_NOT_READY: &str = "exclusive-not-ready";
/// A whole-node lease request for which no host is capable of it.
pub const NO_CAPABLE_HOST: &str = "no-capable-host";
/// A whole-node lease request that matches no pool.
pub const NO_MATCHING_POOL: &str = "no-matching-pool";
/// A run for a whole-node lease whose agent has not reported `exclusive_ready` for it:
/// the lease holds the host, but the host is still being prepared. Used by the server,
/// not by the agent.
pub const NOT_READY: &str = "not-ready";
