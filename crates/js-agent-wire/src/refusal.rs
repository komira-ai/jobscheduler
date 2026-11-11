//! The codes a refusal carries on the wire.
//!
//! They are plain constants with no central table: a code is text a deployed agent or
//! server already matches on, so renaming one breaks a peer that was not upgraded with
//! it. A later change adds a code; it never changes or removes one. The
//! `refusal_codes_snapshot` test fails on a rename.
//!
//! The codes of a start in the one job definition (an `image` with a `run`, `os` and
//! `arch`):
//!
//! | code | when |
//! |---|---|
//! | [`INVALID_IMAGE`] | a `run` beside an empty `image` |
//! | [`INVALID_JOB`] | a `run` beside a `native` body (field `run`) or beside job `args` (field `args`), or a start a server cannot write in the legacy forms for an older host |
//! | [`PLATFORM_MISMATCH`] | the start's `os` or `arch` is not the platform of its image's config, or that platform is outside the set a start names |
//! | [`IMAGE_UNSUPPORTED_HERE`] | an image start on a Mac agent that cannot run it: one without a `run`, or an agent with no bundle store |
//! | [`RUN_UNSUPPORTED`] | a `run` on a Linux agent, which runs the image's own command only |
//! | [`IMAGE_NOT_ALLOWED`] | an image start on a Mac agent whose image is on no `--image-allow` entry |

/// A hello whose body is malformed: a bad id, list, label or shape.
pub const INVALID_HELLO: &str = "invalid-hello";
/// A hello (or run) whose labels use a key under the reserved `jobscheduler.dev/`
/// prefix.
pub const RESERVED_LABEL: &str = "reserved-label";
/// A job whose wall-time limit is zero.
pub const WALL_TIME_NOT_POSITIVE: &str = "wall-time-not-positive";
/// A job spec with a malformed id, argument list, limit or label.
pub const INVALID_JOB: &str = "invalid-job";
/// A job whose image name is empty, too long or outside the image alphabet; or a job
/// start's image layer (`layers.image`) of that kind, or a job start with a `run` and no
/// image (field `run`).
pub const INVALID_IMAGE: &str = "invalid-image";
/// A job whose image is not pinned by digest: it has no `@sha256:<64 lower case hex>`
/// part (a tag alone, another algorithm, a digest of the wrong length). The refusal's
/// `field` is `image`. Agents run with the pull policy `missing`, so a tag could change
/// under a fleet; a name with a tag as well as a digest is accepted.
pub const IMAGE_DIGEST_REQUIRED: &str = "image-digest-required";
/// A job whose bundle is malformed or contradicts the rest of the spec: a name outside
/// the image alphabet, a digest that breaks the grammar (upper case hex, fewer than 32
/// digits), a bundle without a native body (a bundle needs `native`), or a bundle beside a native body (not yet supported). The refusal's `field` is `bundle`.
pub const INVALID_BUNDLE: &str = "invalid-bundle";
/// A job whose bundle is not pinned by digest: it has no `@sha256:<64 lower case hex>`
/// part (a tag alone, another algorithm, a digest of the wrong length). The refusal's
/// `field` is `bundle`. A bundle is a toolchain a host unpacks once and keeps, so a tag
/// that moves would change the tools under a fleet; a name with a tag as well as a
/// digest is accepted and the tag is not read.
pub const BUNDLE_DIGEST_REQUIRED: &str = "bundle-digest-required";
/// A native job whose `program` is empty, relative, climbs with `..`, or holds a
/// character a path may not; or a job start with no image whose `command` is empty or
/// does not start with such a program.
pub const INVALID_PROGRAM: &str = "invalid-program";
/// A native job, or a job start, whose environment has a bad name or value, too many
/// entries, or a name the agent reserves (`DYLD_`, `LD_`, `SANDBOX_`, `JS_AGENT_`).
pub const INVALID_ENV: &str = "invalid-env";
/// As the detail of a `failed_to_start` result: a job start with a non-empty `env` on an
/// agent whose runtime does not carry a job start's environment; nothing runs. Before the
/// container runtimes set a job's environment every agent answered it so, and the macOS
/// native runtime did until it carried the environment too. No agent of this release or
/// later sends it. Kept, as every code is: a deployed peer may still match on it.
pub const ENV_UNSUPPORTED: &str = "env-unsupported";
/// A start with a `native` body sent to an agent that cannot run native jobs. An agent
/// that predates the field reads such a start as `invalid-image` instead.
pub const NATIVE_UNSUPPORTED: &str = "native-unsupported";
/// A VM start ([`crate::StartKind::Vm`]) sent to an agent that runs no VMs: it was started
/// without the VM backend (`--vm-driver`). The agent answers it `failed_to_start` and
/// creates nothing.
pub const VM_UNAVAILABLE: &str = "vm-unavailable";
/// A start whose `os` or `arch` is not the platform its image's config says it is built
/// for, or whose image names a platform outside the set a start can name (`linux`,
/// `darwin`; `amd64`, `arm64`). The refusal's `field` is `os` or `arch`, or `image.os` or
/// `image.architecture` for the image's own. Retrying cannot change it.
pub const PLATFORM_MISMATCH: &str = "platform-mismatch";
/// As the detail of a `failed_to_start` result: a start named by its image on a Mac agent
/// that cannot run it. A Mac agent with a bundle store runs an image start that carries a
/// `run`, by the bundle the image resolves to; it refuses one without a `run` (a bundle has
/// no command of its own) and any image start when it has no bundle store. Nothing is
/// created. A server sends such a Mac host the legacy form.
pub const IMAGE_UNSUPPORTED_HERE: &str = "image-unsupported-here";
/// As the detail of a `failed_to_start` result: a start with a `run` on an agent that runs
/// containers, which runs an image's own command and has no consumer of `run` yet. Nothing
/// is created; the field is refused rather than dropped, so the job never runs another
/// command than it asked for.
pub const RUN_UNSUPPORTED: &str = "run-unsupported";
/// As the detail of a `failed_to_start` result: a start on a Mac agent whose image is on no
/// `--image-allow` entry, so the agent will not fetch it. Nothing is created and no
/// registry is asked. A Linux agent has no such code.
pub const IMAGE_NOT_ALLOWED: &str = "image-not-allowed";
/// A job with a mount that is absolute, climbs with `..`, is empty or names no root; or a
/// job start with a mount (`mounts`) whose source is not an absolute path in normal
/// form, whose target breaks the target rules (`js_types::path::target`) or repeats
/// another, whose `sub_path` is not a relative path in normal form, or with more than [`crate::limits::MAX_MOUNTS`] of them. An agent also answers
/// it, as the detail of a `failed_to_start` result, for a target below a read-only mount
/// that does not exist in that mount.
pub const INVALID_MOUNT: &str = "invalid-mount";
/// A job with a run file that is badly named, named twice, too large, holds a NUL, or
/// with more files than allowed.
pub const INVALID_FILE: &str = "invalid-file";
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
/// The `reason` of a `shape-not-accepted` refusal when the host's claimed shape does not
/// satisfy the pool's floor: resources, labels or capabilities.
pub const SHAPE_BELOW_FLOOR: &str = "below-floor";
/// The `reason` of a `shape-not-accepted` refusal when the pool names `scratch-quota` and
/// the host's scratch quota default is absent, does not count, or exceeds the pool's disk.
pub const SHAPE_QUOTA_DEFAULT: &str = "quota-default-unfit";
/// The `reason` of a `shape-not-accepted` refusal when the host enforces a scratch quota
/// and the pool books no disk and does not name `scratch-quota`.
pub const SHAPE_QUOTA_WITHOUT_DISK: &str = "quota-without-pool-disk";
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
/// A job start's `log_sink` that fails [`crate::JobStartSpec::log_sink_checked`]: an
/// endpoint, region, bucket or prefix out of its form, or a prefix that is not the job's
/// own. The refusal's `field` names the part. It never refuses the start: the server sends
/// no sink instead, and an agent starts the job without live logs.
pub const INVALID_LOG_SINK: &str = "invalid-log-sink";
/// A reply's `log_grant` with an empty, over-long or malformed field, or an expiry of zero
/// ([`crate::ReplyCheck::log_grant`]). The refusal's `field` names it. The agent ignores
/// the credential and acts on the rest of the reply.
pub const INVALID_LOG_CREDENTIAL: &str = "invalid-log-credential";

/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. It named a tree input the
/// host could not attach for a reason that was not the tree's own, as the detail of a
/// `failed_to_start` result or as a result's `input_failure`.
pub const TREE_UNAVAILABLE: &str = "tree-unavailable";
/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. As the detail of a
/// `failed_to_start` result, it named a tree input refused for the tree itself.
pub const TREE_INVALID: &str = "tree-invalid";
/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. As a result's
/// `input_failure`, it named a failed read of a tree input while the job ran.
pub const TREE_READ_FAILED: &str = "tree-read-failed";
/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. As a result's
/// `input_failure`, it named a job past its wall limit that reading its tree inputs had
/// stalled.
pub const TREE_STALLED: &str = "tree-stalled";
/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. As the detail of a
/// `failed_to_start` result, it named a writable tree input at a `dest` other than the exec
/// root.
pub const TREE_PLACEMENT_UNSUPPORTED: &str = "tree-placement-unsupported";
/// Retired, as [`crate::InputRef::Tree`] is: no agent sends it. As the detail of a
/// `failed_to_start` result, it named a tree input on an agent that attaches none.
pub const TREE_UNSUPPORTED_HERE: &str = "tree-unsupported-here";
/// As the detail of a `failed_to_start` result: a `run` with an output, on an agent that
/// collects no outputs of a `run`.
pub const OUTPUTS_UNSUPPORTED_HERE: &str = "outputs-unsupported-here";
/// As the detail of a `failed_to_start` result: a start whose read-only paths (its files
/// directory and its inputs) are more than the agent's sandbox profile can list.
pub const TOO_MANY_INPUTS_HERE: &str = "too-many-inputs-here";
/// As the detail of a `failed_to_start` result: a start whose working directory is at or
/// below an input's `dest`, on an agent that places that input as a link out of the run
/// directory.
pub const CWD_IN_TREE: &str = "cwd-in-tree";

/// As the detail of a `failed_to_start` result: a start whose layers, mounts or host
/// ports name what the host's operator did not allow: a host path under no `--mount-root`
/// or `--mount-root-ro` once its links are followed, a read-write mount under a read-only
/// root, a mount's `sub_path` that leads out of its source by a link, or a host port not in
/// `--host-port`. Nothing was created. A mistake of the host's
/// configuration, not of the job or the host's state; an image layer outside every
/// `--image-allow` entry is [`IMAGE_NOT_ALLOWED`] instead.
pub const INPUT_NOT_ALLOWED: &str = "input-not-allowed";
/// As the detail of a `failed_to_start` result: a host path of a layer or a mount, or a
/// mount's `sub_path`, that is missing, is not a directory, cannot be read, or whose lookup
/// passed its bound; or a
/// mount's source the job's user cannot reach. The host's state: another host or a later
/// try may succeed.
pub const INPUT_UNAVAILABLE: &str = "input-unavailable";
/// As the detail of a `failed_to_start` result: a start with `layers` on an agent whose
/// runtime has no root to replace (the macOS native runtime). Nothing was created.
pub const ROOTFS_UNSUPPORTED_HERE: &str = "rootfs-unsupported-here";
/// As the detail of a `failed_to_start` result: on the macOS native runtime, a mount
/// whose target is outside `/work`, or a read-only or read-write one at `/work` itself.
/// Nothing was created.
pub const MOUNT_TARGET_UNSUPPORTED_HERE: &str = "mount-target-unsupported-here";
/// As the detail of a `failed_to_start` result: on the macOS native runtime, a host port
/// whose `container_port` differs from its `host_port`: a native job has no loopback of its
/// own to offer another port on. Nothing was created; placement never sends one there.
pub const HOST_PORT_REMAP_UNSUPPORTED_HERE: &str = "host-port-remap-unsupported-here";
/// As a result's `input_failure` ([`crate::JobResult::input_failure`]): a job that did not
/// succeed, after which a host path of its layers or mounts no longer answered. The
/// failure is the host's; the outcome and exit status are kept.
pub const INPUT_UNREADABLE: &str = "input-unreadable";
/// As a result's `input_failure` ([`crate::JobResult::input_failure`]): a host port of the
/// job that the agent could not offer again after it restarted, so it stopped the job.
pub const HOST_PORT_LOST: &str = "host-port-lost";

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
/// As the detail of a `failed_to_start` result: the agent's scratch disk quota is not
/// working (it failed several calls in a row and no longer passes its own check, or too many
/// slots were given up on), so the `Start` was refused and nothing was created. It tells the
/// consumer that this host's quota is unavailable: the host's fault and not the run's, and,
/// unlike [`HOST_TAINTED`], not a taint (the host is clean; a repaired quota, or a restart
/// of the agent after it gave up on too many slots, ends it). The run may be sent to
/// another host. Whether a retry is charged to the run is the consumer's choice; nothing in
/// the scheduler counts retries.
pub const QUOTA_UNAVAILABLE: &str = "quota-unavailable";

/// As the detail of a `failed_to_start` result: a start whose `network` is above the agent's
/// ceiling (`--egress`, reported in the hello as the capability `egress`). Nothing was
/// created, and the job is never run with less than it asked for. The text that stays on the
/// host names the level asked for and the ceiling (`network allow-list asked, agent ceiling
/// is none`). The host's ceiling is at fault, not the run: the run may be sent to a host
/// whose ceiling covers it. A container-engine run (Docker or Podman) has the ceiling
/// `none`. A Mac agent answers it, too, for a native body whose `network` is above its
/// `--native-network` (the text then names that value: `the agent allows at most off`).
pub const NETWORK_ABOVE_CEILING: &str = "network-above-ceiling";
/// As the detail of a `failed_to_start` result: a start that declares a host port the agent
/// does not list (`--egress-forward-port`, reported in the hello as the capability
/// `egress-ports`). Nothing was created. The text that stays on the host names the port and
/// the ports the agent lists. The host's ceiling is at fault, not the run.
pub const PORT_ABOVE_CEILING: &str = "port-above-ceiling";
/// As the detail of a `failed_to_start` result: a start within the agent's ceiling that
/// declares ports, on an agent that forwards none (its hello reports no `egress-ports`), or
/// for a job whose network is `none` (a forward needs a job with an address). Nothing was
/// created. It is checked after [`NETWORK_ABOVE_CEILING`]; on an agent that forwards some
/// ports, after [`PORT_ABOVE_CEILING`]. A Mac agent, which forwards no port, answers it for
/// a native body that declares any, after [`NETWORK_ABOVE_CEILING`].
pub const PORTS_NOT_SUPPORTED: &str = "ports-not-supported";

/// A capabilities frame whose digests, root or Xcode builds are malformed.
pub const INVALID_CAPS: &str = "invalid-caps";
/// A start that names a bundle, sent to an agent that holds no bundle store (it reports no
/// `bundle` capability). The placement should not have sent it there; the agent refuses
/// rather than run the program with the host's tools.
pub const BUNDLE_UNSUPPORTED: &str = "bundle-unsupported";
/// A start whose bundle the agent has begun to fetch and does not hold yet; ask again.
pub const BUNDLE_PENDING: &str = "bundle-pending";
/// A start whose bundle the agent does not hold, on an agent that fetches nothing
/// (`--bundle-pull never`): the bundle was not prefetched before the job was placed.
pub const BUNDLE_NOT_HELD: &str = "bundle-not-held";
/// A start whose bundle would have to be fetched while the store has no room: over its
/// budget with nothing to evict, or the volume under its low-water mark. Retriable.
pub const BUNDLE_NO_SPACE: &str = "bundle-no-space";
/// A start whose bundle needs a newer macOS than the host runs.
pub const BUNDLE_OS_TOO_OLD: &str = "bundle-os-too-old";
/// A start whose bundle needs an Xcode the host does not have.
pub const BUNDLE_XCODE_MISMATCH: &str = "bundle-xcode-mismatch";
/// A start whose bundle digest is not on the host's allow list (`--bundle-trust list`).
pub const BUNDLE_NOT_ALLOWED: &str = "bundle-not-allowed";
/// A start whose bundle digest is on the host's deny list.
pub const BUNDLE_DENIED: &str = "bundle-denied";
/// A start whose bundle name is outside every registry and namespace the host may fetch
/// from (`--bundle-registry-allow`).
pub const BUNDLE_REGISTRY_NOT_ALLOWED: &str = "bundle-registry-not-allowed";
/// A start whose bundle could not be fetched or did not verify (a digest or size
/// mismatch).
pub const BUNDLE_FETCH_FAILED: &str = "bundle-fetch-failed";
/// A start whose pin names an index whose darwin/arm64 entry is itself an index (one level
/// only).
pub const BUNDLE_INDEX: &str = "bundle-index";

/// Whose fault a refused start is, which decides what a client does with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalClass {
    /// The node was not ready or the rollout order was wrong: the work can run elsewhere
    /// or later, and the refusal is never cached as the action's result.
    Farm,
    /// The client sent something wrong: a precondition failure that asking again cannot
    /// change.
    Request,
}

/// The refusals of a bundle start that are the farm's fault.
const FARM_CODES: [&str; 11] = [
    BUNDLE_UNSUPPORTED,
    BUNDLE_PENDING,
    BUNDLE_NOT_HELD,
    BUNDLE_NO_SPACE,
    BUNDLE_OS_TOO_OLD,
    BUNDLE_XCODE_MISMATCH,
    BUNDLE_NOT_ALLOWED,
    BUNDLE_DENIED,
    BUNDLE_REGISTRY_NOT_ALLOWED,
    BUNDLE_FETCH_FAILED,
    IMAGE_NOT_ALLOWED,
];

/// The refusals of a bundle start that are the request's fault.
const REQUEST_CODES: [&str; 4] = [
    BUNDLE_DIGEST_REQUIRED,
    INVALID_BUNDLE,
    BUNDLE_INDEX,
    INVALID_ENV,
];

/// The class of a refusal a bundle start can meet, or `None` for any other code. A bundle
/// start that an agent admits but cannot run ends as `native-unsupported`, which has no
/// class: it is the agent's lack of a runtime, and a consumer must neither cache it as the
/// action's result nor treat it as the request's fault.
#[must_use]
pub fn bundle_class(code: &str) -> Option<RefusalClass> {
    let farm = FARM_CODES.contains(&code).then_some(RefusalClass::Farm);
    farm.or_else(|| {
        REQUEST_CODES
            .contains(&code)
            .then_some(RefusalClass::Request)
    })
}
