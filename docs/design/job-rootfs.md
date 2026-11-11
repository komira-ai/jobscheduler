# A job's file system: layers, mounts and host ports

Status: **design for the owner's review; of its build items (section 11), item 2, the agent wire, is built.** It replaces `docs/design/tree-inputs.md` and its build
items: jobscheduler does not know that a node-local cache exists, let alone which one. What tree-inputs.md asked of
jobscheduler is met here by three things any container runtime offers (layers, bind mounts and access to a host port),
and the pieces of its wire that already merged (komira-ai/jobscheduler#842) are retired additively (section 6).

**What changes for a user.** Today a job's file system is one image, read-only, with an empty writable `/work` in which
it runs (section 1). After this design a job can also say, as it would to Docker:

- **layers**: an ordered list of read-only trees stacked on its image, or in place of it, each either a directory on the
  host or another image from a registry the host allows. The job's root is the stack plus a private writable layer on
  the host's scratch volume, so writes never reach a layer;
- **mounts**: a host directory at a path in the job's file system, read-only, read-write, or copy-on-write (the job may
  change it; the changes are its own);
- **host ports**: a TCP port on the host's loopback that the job reaches at the same port on its own loopback.

The host's operator decides which host paths, registries and ports a job may name, by prefix and by port; a start that
names anything else fails by name. What does not change: a job that uses none of the three runs exactly as today, byte
for byte on the wire, on every host; and nothing in jobscheduler's code, wire, flags, capabilities or tests names a
particular cache, file system or storage service.

**Where this comes from.** The farm's build service, kbf, wants a build action's inputs read lazily from the node's
cache instead of downloaded, and its output uploaded to the same node-local service. The cache (tierstore) offers a
FUSE file system of content-addressed trees and a local image registry; how kbf uses it is kbf's design. This document
only needs what any such service would need: a host path can be a layer or a mount (it happens to be a lazy FUSE
directory on the farm), an image can come from a registry on `127.0.0.1` (it happens to be the cache's registry
facade), and a job can reach a port on the host's loopback (it happens to be the cache's upload endpoint). Section 7
says how a client like kbf sees the job being made, which is when it asks its cache to fetch ahead.

Contents: 1 what exists today; 2 the job spec; 3 the agent wire; 4 the agent on Linux; 5 the agent on macOS; 6 the tree
wire of #842; 7 readiness and timing; 8 failures; 9 compatibility; 10 tests; 11 build items; 12 decisions; 13 open
questions.

## 1. What exists today

Read in code; every later section builds on these facts.

- **The root is one flattened image, read-only.** The agent pulls a digest-pinned image, unpacks its layers into one tree
  `oci-images/<hex>/rootfs` of the agent's image store, with whiteouts applied (`crates/js-contain/src/unpack.rs`; the whiteout entries
  themselves are never written), and gives that tree to `crun` as `root.path`, read-only
  (`crates/js-contain/src/oci/spec.rs`, `action_config`). Two images never share a layer on disk.
- **The only overlay is `/work`.** `crun`, run as the unprivileged job user inside a new user namespace, mounts
  `lowerdir=<lease>/root,upperdir=<lease>/upper,workdir=<lease>/work` with the options
  `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off` at `/work` (`OVERLAY_OPTIONS`,
  `crates/js-contain/src/oci/spec.rs:47`, rendered at `:208`). `<lease>/root` is always empty
  (`crates/js-agent/src/crun/spec.rs`, `input_root: lease.root`). The job's working directory is `/work`; the image's
  `WORKDIR` is not applied. The agent itself never calls `mount(2)`.
- **Host paths are an operator's table of named roots, and `crun` refuses it.** `--mount-root NAME=PATH` and
  `--mount-root-ro NAME=PATH` name directories a job may mount from; `MountRoots`
  (`crates/js-agent/src/lifecycle/mounts.rs`) canonicalises a request beneath its root and refuses `..`, a link out, a
  read-write mount from a read-only root, and a path a mount option cannot carry. The Docker runtime reads its binds
  back after the daemon makes them; `crun` does not, so a `crun` agent refuses both flags at start
  (`crates/js-agent/src/crun_host.rs:781-791`): a job that can write inside a root could swap a checked directory for a
  link between the check and the bind. The doc comment of `MountRoots` names the fix, opening the path beneath its root
  with `openat2` in the process that binds it; section 4.2 uses it.
- **Registries are an allow-list by prefix.** `--image-allow HOST[:PORT]/PREFIX` (`crates/js-agent/src/lib.rs`), checked
  by `js_oci::Policy` when a repository is opened (`crates/js-oci/src/client.rs`, `Client::repository`), with
  `--registry-credentials` per entry. Images are pinned by digest end to end (`ImageDigestRef`,
  `crates/js-types/src/job.rs`; `pinned`, `crates/js-agent/src/crun/spec.rs`). `js_oci::Allow` can admit a loopback
  registry over plain HTTP (`Allow::plain_http_loopback`, `crates/js-oci/src/policy.rs:267`), but no flag of the agent
  reaches it.
- **A job cannot reach its host.** A `none` job has only its own loopback. A wired job (`allow-list`, `open`) has a veth
  to the host whose `input` chain rejects everything it sends to the host itself
  (`crates/js-agent/src/egress/ruleset.rs`, the rule commented `input`). The `ports` of a job (`PortForward`,
  `PortMap { host, container }`) are the other direction: a forward from a host port into the job.
- **macOS has no root to replace.** A native job runs as a pool user under `sandbox-exec`, in `exec/` of the user's
  home, with reads open except listed denies (`ReadModel::Open`, `crates/js-agent/src/native/settings.rs`) and writes
  only in the home. A `Mount` input is copied into `exec/<dest>` (`crates/js-agent/src/native/files.rs`). The Seatbelt
  builder can already allow single loopback ports (`Net::Ports`, `crates/js-mac-pool/src/seatbelt/profile.rs:191-195`),
  which no job reaches yet.
- **The preparing stage is image-only.** A contained start whose image is not on the host waits in `Stage::Preparing`
  (`crates/js-agent/src/lifecycle/machine/job.rs`), reported as `starting`, while the prefetcher pulls it
  (`crates/js-agent/src/lifecycle/prefetch.rs`; `Runtime::prepare`, `crates/js-agent/src/lifecycle/runtime.rs:321`,
  keyed by one image string). The wall limit starts when the container is made (`settle`,
  `crates/js-agent/src/lifecycle/machine/mod.rs`). The server's start timeout (900 s by default,
  `crates/js-lease-server/src/job_expiry.rs`) bounds the whole stage from placement.
- **A start failure reaches the client without its reason.** Every `failed_to_start` result becomes
  `StartFailure::AgentRefused` (`end_of`, `crates/js-lease-server/src/per_job.rs:339`), and the public
  `Job.start_failure_detail`, documented as the agent's code, is always empty
  (`crates/js-lease-server/src/job_api/read/view.rs:228`).

## 2. The job spec (public interface)

### 2.1 The fields

All additive to `crates/js-proto/proto/jobscheduler/v1/job.proto`. The numbers follow the file's grouping in tens: the
network group (30s) takes the host ports, and a new 60s group takes the file system.

```proto
// One read-only layer of a job's root file system.
message Layer {
  oneof source {
    // An absolute path of a directory on the host. The host's operator lists the prefixes a
    // job may name; a path outside them fails to start. Read through the job's id mapping:
    // a file owned by an id outside it reads as owned by 65534, so it needs read (and, to run
    // it, execute) permission for others.
    string host_path = 1;
    // An image from a registry the host allows, pinned by digest: NAME[:TAG]@sha256:HEX. Its
    // own layers are flattened into one tree (whiteouts applied), which is the layer; its
    // configuration (entrypoint, command, environment) is not used.
    string image = 2;
  }
}

// How a host directory appears in the job.
enum MountMode {
  // Read as read-only.
  MOUNT_MODE_UNSPECIFIED = 0;
  MOUNT_MODE_READ_ONLY = 1;
  // The job's writes reach the host directory. The host's operator lists the prefixes a job
  // may mount read-write.
  MOUNT_MODE_READ_WRITE = 2;
  // The job may change the directory; its changes are its own, kept in the job's scratch,
  // and never reach the host directory.
  MOUNT_MODE_COPY_ON_WRITE = 3;
}

// A host directory at a path in the job's file system.
message Mount {
  // An absolute path of a directory on the host, as Layer.host_path.
  string host_path = 1;
  // Where it appears: an absolute path in the job's file system, not "/".
  string target = 2;
  MountMode mode = 3;
}

// A TCP port on the host's loopback that the job reaches at the same port on its own
// loopback (127.0.0.1). Not a PortForward, which is the other direction.
message HostPort {
  uint32 port = 1;
}

message JobSpec {
  ...
  // TCP ports on the host's loopback the job may connect to, at the same port on its own
  // loopback, whatever its network level. The host's operator lists the ports a job may
  // name. Empty: none.
  repeated HostPort host_ports = 32;
  // The read-only layers of the job's root, base first, stacked on `image` (or in place of
  // it, when `image` is empty); a later layer's file hides an earlier one's. With layers,
  // the root is writable and the writes are the job's own. Empty: the root is `image`
  // alone, read-only, as before.
  repeated Layer layers = 60;
  // Host directories at paths in the job's file system. Empty: none.
  repeated Mount mounts = 61;
}

enum StartFailureCode {
  ...
  // An input the job named could not be read on its host: a host path of a layer or mount
  // that is missing, is not a directory, cannot be read, or did not answer in time. The
  // host's state, not the job's: another host or a later try may succeed.
  START_FAILURE_CODE_INPUT_UNAVAILABLE = 5;
}

enum ResultReason {
  ...
  // A host path or host port the job named failed while it ran, and the job did not
  // succeed: the failure is the host's, not the job's. The state is FAILED; the exit code
  // is kept.
  RESULT_REASON_INPUT_FAILED = 5;
}

message JobResult {
  ...
  // With RESULT_REASON_INPUT_FAILED: the agent's code, `input-unreadable` or
  // `host-port-lost`. Absent otherwise.
  optional string input_failure = 12;
}
```

`Job.start_failure_detail` (existing, field 5) starts to carry what its comment already promises: the agent's code for
every start failure it refused, among them `input-unavailable` and `input-not-allowed` (section 8).

The name `host_ports` is distinct from `ports` (`PortForward`) on purpose, and the message is a message, not a bare
`uint32`, so that a protocol or a host address can be added to it later without a second field (decision LR9).

### 2.2 Meaning

- **The root.** Bottom to top: `image` when it is set, then `layers` in order, then a private writable layer on the
  host's scratch volume. With no `layers`, the root is `image` alone and read-only, exactly as today. With `layers` and
  an empty `image`, the root is the layers alone; there is then no image configuration, so `command` is the whole
  program, an absolute path in the root, and the environment is the job's `env` over a default `PATH`. With `image` set,
  its configuration applies as today; the configurations of image layers never do.
- **`/work`** stays the job's working directory and stays an empty writable directory unless a mount targets it. A
  `COPY_ON_WRITE` mount at `/work` is how a job runs in a writable copy of an input tree.
- **Mounts** are made after the root, parents before children: a mount may lie below another one (`/work` copy-on-write
  and `/work/sdk` read-only), two mounts may not have the same target.
- **Host ports** are reachable at every network level, `none` included, and are the only way a job reaches its host.
  Nothing else about the job's network changes.

What a job cannot say: a layer or mount on a host isolated job (`ISOLATION_HOST` runs on the machine itself and already
refuses mounts and files); a target in the runtime's own places (`/proc`, `/dev`, `/sys`, `/run/js-files`, `/scratch`,
`/root`, `/tmp`, `/var/tmp`, `/run`, and anything below them), which section 2.3 lists for the submit and section 3 for
the agent.

### 2.3 Validation at submit

`crates/js-lease-server/src/job_api/validate.rs` checks the shape only; it cannot see a host's allow-lists (2.4). A
fault answers `INVALID_ARGUMENT` naming the field, as every other fault of a job does (job.proto: only four faults carry a
`RefusalCode`), so no refusal code is added:

| Field | Rule |
|---|---|
| `image` | as today when set; may be empty only when `layers` is not |
| `command` | with an empty `image`: an absolute path |
| `layers` | at most 16; each has exactly one source |
| `layers[i].host_path`, `mounts[i].host_path` | absolute; normal form (no `.`, `..` or empty part, no trailing `/`); at most 4096 bytes; no NUL or control character |
| `layers[i].image` | `ImageDigestRef` as `image` |
| `mounts` | at most 64 |
| `mounts[i].target` | absolute, normal form, not `/`, not a runtime place of 2.2, ASCII, no `:` or `,`, at most 4096 bytes; no two equal (compared in lower case, for macOS) |
| `mounts[i].mode` | a known value; unspecified reads as read-only |
| `host_ports` | at most 16; each 1 to 65535; no port twice |
| `constraints.isolation` | not `HOST` with any of the three |

The rules for a target reuse the agent wire's relative-path rules (`rel_path` and `disjoint`,
`crates/js-agent-wire/src/run.rs`) after the leading `/`, moved to `js-types` so that the server and the agent check the
same text with the same code.

The three fields are part of "the same job" for an idempotent resubmit: `js_jobs::JobSpec` gains them and `same_job_as`
(`crates/js-jobs/src/record.rs`), which names every field, compares them. The stored row gains three tags after the
highest in use (`crates/js-jobs/src/codec.rs` and a `rootfs_fields` module beside the others), and one for the start
failure detail (section 8).

### 2.4 Placement

An agent of a previous release ignores fields of a start it does not know (`JobStartSpec` has no
`deny_unknown_fields`), so it would run a job with layers on its image alone. Placement must never send such a job there:

- **A detected capability `layered-rootfs=true`**, reported by an agent that reads the three start fields and refuses by
  name what its runtime cannot do. It goes into `js_caps::DETECTED_KEYS` (an operator cannot set it) and the exact keys,
  and `validate.rs` inserts it into the job's constraints when any of the three is set, as it inserts `isa_level` today.
  `start_of` (`crates/js-lease-server/src/job_start.rs`) refuses to build such a start for a host whose hello lacks it
  (`Unsendable::RootfsNotReported`, new), through a `layered_rootfs: bool` in `BeatContext` filled as `host_jobs` is.
- **A job with `layers` also requires `exec=container`**, inserted the same way: a root of layers exists only in a
  container, and a Mac never gets such a start (section 5).
- **The allow-lists are not advertised.** A list of path prefixes does not fit a capability value
  (`MAX_CAPABILITY_VALUE_LEN`, 128 bytes), and a pool's hosts share one configuration. A host whose operator did not
  allow what a job names refuses the start by name (`input-not-allowed`); that is a configuration mistake, fixed in
  the configuration (decision LR6).

## 3. The agent wire

`JobStartSpec` (`crates/js-agent-wire/src/per_job.rs`) gains three fields, each `#[serde(default, skip_serializing_if =
"Vec::is_empty")]`, so a start without them is the same text as today:

```rust
/// The read-only layers of the job's root, base first, above `image`.
pub layers: Vec<Layer>,
/// Host directories at paths in the job's file system.
pub mounts: Vec<PathMount>,
/// TCP ports on the host's loopback the job reaches at the same port on its own.
pub host_ports: Vec<u16>,
```

```rust
/// One read-only layer of a job's root. No catch-all variant: an agent that meets a kind it
/// does not know fails the decode of that one start, as for `InputRef`.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layer {
    /// A directory on the host, by absolute path.
    HostPath { path: String },
    /// An image pinned by digest, flattened into one tree.
    Image { image: String },
}

/// A host directory at a path in the job's file system.
pub struct PathMount {
    /// An absolute path on the host.
    pub source: String,
    /// An absolute path in the job's file system.
    pub target: String,
    /// Read-only (the default, left out), read-write or copy-on-write.
    #[serde(default)]
    pub mode: MountMode,
}

/// No catch-all variant, as `Placement` has none.
#[serde(rename_all = "snake_case")]
pub enum MountMode { #[default] ReadOnly, ReadWrite, CopyOnWrite }
```

`PathMount` is not `Mount` (`crates/js-agent-wire/src/command.rs`, a named root and a relative path, the legacy job
form's) and does not replace it. `JobStartSpec::validate` applies the rules of 2.3, and `image` may be empty when
`layers` is not (today it refuses an empty image `invalid-job`). The limits are constants of `limits.rs`: `MAX_LAYERS`
16, the existing `MAX_MOUNTS` 64 and `MAX_PORTS` 16.

`start_of` copies the three fields from the job. A job without them gets a start without them, byte for byte as today.

New agent refusal codes, added to `refusal.rs` (add-only, `refusal_codes_snapshot`):

| Code | When |
|---|---|
| `input-not-allowed` | a host path lies under no `--mount-root`/`--mount-root-ro` (after links are followed), a read-write mount lies under a read-only root, or a host port is not in `--host-port` |
| `input-unavailable` | a host path is missing, not a directory, cannot be read, or its lookup passed its bound (section 7); a mount's source the job user cannot reach (4.2) |
| `rootfs-unsupported-here` | `layers` on an agent whose runtime has no root to replace (macOS) |
| `mount-target-unsupported-here` | on macOS, a mount target outside `/work`, or a read-only or read-write mount at `/work` itself (5.1) |
| `input-unreadable` | as a result's `input_failure`: a host path no longer answers after the job, which did not succeed (section 8) |
| `host-port-lost` | as a result's `input_failure`: a host port could not be offered again after the agent restarted (4.5) |

An image layer outside every `--image-allow` entry is refused `image-not-allowed`, the existing code.

## 4. The agent on Linux (`crun`)

### 4.1 Configuration

All flags, in the agent's flags file (B14, S2):

| Flag | Meaning |
|---|---|
| `--mount-root NAME=PATH`, `--mount-root-ro NAME=PATH` | **existing**, now accepted with `--runtime crun`. Each root is a prefix: a job may name any directory beneath it (after links are followed), read-only beneath either kind, read-write only beneath a `--mount-root`. The names stay for the legacy `Mount` form and for the log; a host path never names a root. |
| `--image-allow HOST[:PORT]/PREFIX` | **existing**; an image layer is allowed as the job's image is. |
| `--image-allow-loopback 127.0.0.1:PORT/PREFIX` (or `[::1]:PORT/PREFIX`) | **new**: a registry on the host's own loopback, over plain HTTP, with its port given. It builds `Allow::plain_http_loopback()`; `--registry-credentials` may name it as any other entry. |
| `--host-port PORT` or `LO-HI` | **new**, repeatable, at most 16: the host loopback ports a job may reach. |

Why the mount roots and not a new flag: they already are the operator's list of host directories jobs may use, with its
canonicalisation, its escape checks and its tests; a second list of prefixes beside them would be a second way to say
the same thing (decision LR1). What kept them off `crun` was the race of section 1, which 4.2 closes.

### 4.2 Resolving a host path, without a race

Each host path of a start (a layer's, or a mount's) is resolved once, by the agent, which is root on a `crun` host:

1. Find the longest root that is a prefix of the path's text. None: `input-not-allowed`.
2. From the root's directory (held open since the agent started), open the rest of the path with `openat2`,
   `O_PATH | O_DIRECTORY` and `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`. `RESOLVE_NO_XDEV` is not given: a root may
   hold another file system below it (on the farm, the FUSE mount is below its root). A link that leads out of the root
   fails here, at the one moment that counts (`input-not-allowed`); a missing path or a non-directory is
   `input-unavailable`.
3. Clone what was opened into a detached mount of its own with `open_tree(fd, OPEN_TREE_CLONE | AT_RECURSIVE)`. While
   it is still detached, set it with one `mount_setattr(fd, "", AT_EMPTY_PATH | AT_RECURSIVE, ...)`: propagation
   `MS_PRIVATE`, and `MOUNT_ATTR_RDONLY | NOSUID | NODEV` (without `RDONLY` for a read-write mount). Then attach it
   with `move_mount` at a fixed path, `<mounts>/<lease>/src/<i>` (4.3 says where `<mounts>` is).

The clone must be private before it is attached. A clone of a shared mount joins its peer group: without the change, an
unmount of a submount of the clone would propagate to the provider's own tree, and a mount the provider makes later
would appear in the clone.

From then on, nothing reads the job's path text again: the overlay's `lowerdir` and `crun`'s binds name
`<mounts>/<lease>/src/<i>`, a directory of root's that no job can write. A swap of the original path after step 2
changes nothing, and the path text never reaches a mount option, so the rule that a host path hold no `,` or `:`
(`carries`) is not needed for these paths. `openat2` needs Linux 5.6, `open_tree`/`move_mount` 5.2 and
`mount_setattr` 5.12; the farm runs 6.8 and later. `rustix` wraps `openat2`, `open_tree` and `move_mount` safely; it
has no `mount_setattr`, which goes in the crate's platform bindings as one system call with a `// SAFETY:` comment
(R2), unless a safe wrapper exists by then.

`crun` runs as the job user and binds the sources of the read-only and read-write mounts itself, so the job user must be
able to reach them: `<mounts>` and every directory below it are mode 0711, owned by root. For each mount source, at
assembly, the agent checks with `faccessat` under the job user's ids that the clone's root can be searched; a source the
job user cannot reach is `input-unavailable` at the start, rather than a `crun` error. Layers need no such check, since
the agent mounts them.

The race is closed for every host path of the new fields. Lifting the `crun_host.rs` refusal also needs `--mount-root`'s
legacy `Mount` form to go the same way on `crun`; it does, because `crun` jobs receive starts, and the legacy form is
the Docker runtime's.

### 4.3 The root: an overlay the agent mounts

`crun` cannot mount an overlay at `/`: it enters `root.path` before it makes the mounts of the configuration. So, as
Docker's daemon does, the agent mounts the root itself, on the host, and gives the result to `crun` as `root.path`:

```
mount -t overlay overlay <mounts>/<lease>/rootfs \
  -o lowerdir=<mounts>/<lease>/src/<n-1>:...:<mounts>/<lease>/src/0,upperdir=<lease>/rootfs-upper,workdir=<lease>/rootfs-work,
     index=off,metacopy=off,redirect_dir=off,xino=off,nosuid,nodev
```

- The lowers are the layers top first (overlay's order is the reverse of the spec's). The image, when there is one, is
  the bottom lower: its tree in the image store, `oci-images/<hex>/rootfs`, pinned for the lease as today. An image layer is
  pulled and unpacked into the same store, pinned the same way, and is a lower as it is: two image layers stack as two
  whole trees, so a file one deletes in its own layers is deleted in its tree but not in the tree below it (decision LR4).
- The upper and work directories are on the scratch volume, in the lease directory, under the run's project quota as
  `<lease>/upper` is today. The scratch volume is local (XFS, for the quota), which overlayfs requires of an upper; a
  lower may be any file system the kernel can stack, FUSE among them, provided its mount lets other users read it
  (`allow_other`): the agent reads as root and the job as its own ids.
- The mount is made in the agent's mount namespace, as root, so `userxattr` is not given: whiteouts in the upper are the
  kernel's character devices. `crun` then puts the job in its user namespace over a root it did not make, which is how
  a daemon-made overlay and a mapped container work together.
- The upper's root directory is made owned by the container's root (the job user's first subordinate id) and mode 0755,
  as an image's root is, so the job sees `/` owned by root.
- The root is now writable where its modes allow the job (uid 1000 in the container) to write. `/tmp`, `/var/tmp` and
  `/run` stay tmpfs, `/root` stays the run's home, `/scratch` stays the run's scratch: nothing moves for a program
  that writes where it did.

**Where the mounts are: `<mounts>`, which is `/run/js-agent/mounts`, outside the scratch root.** The agent's start sweep
removes every entry of the scratch root that is not a kept run (`ScratchRoot::sweep`,
`crates/js-agent/src/lifecycle/scratch.rs`), and an agent of an earlier release does that too. A directory there that
held a live clone of a read-write host path would be walked into by that sweep and the host's data removed. So the
agent's mounts live in a directory that no release sweeps, beside the run files' default root `/run/js-agent/files`: a
`tmpfs`, which a reboot clears along with the mounts. The upper and work directories are plain directories and stay in
the lease directory, under the quota.

The mounts propagate nowhere. At start the agent makes `<mounts>` a mount point by binding it onto itself, but only if
it is not one already (an agent that restarts finds its own bind, with the adopted runs' mounts below it, and must not
cover them with a second); then it makes it private, whether it made it or found it. This happens before the integrity
baseline (4.6).

A job without `layers` keeps today's root exactly: the image's tree, read-only, no agent mount.

### 4.4 Mounts

- **`READ_ONLY` and `READ_WRITE`** become `HostMount` entries of the container spec
  (`crates/js-contain/src/container.rs`) with `source` `<mounts>/<lease>/src/<i>`, bound by `crun` `rbind,ro|rw,nosuid,nodev`
  as the existing mounts are (`crates/js-contain/src/oci/spec.rs`, `mounts`). A read-only clone is read-only at step 3
  of 4.2 already; `crun`'s `ro` is the second lock.
- **`COPY_ON_WRITE`** is an overlay of its own, made by the agent as in 4.3 with one lower (`<mounts>/<lease>/src/<i>`)
  and its own upper and work directories (`<lease>/cow/<i>/{upper,work}`), mounted at `<mounts>/<lease>/cow/<i>` and bound by
  `crun` read-write at the target. A copy-on-write mount at `/work` takes the place of today's `/work` overlay, which
  `crun` then does not make.
- Targets are made in the root before `crun` binds them: in the root's upper, by the agent, owned as the upper is, so a
  target exists whatever the layers hold there. A target under another mount is made after that mount, in its upper
  when it is copy-on-write; a target under a read-only mount must exist in that mount, or the start fails
  `invalid-mount` (the existing code: it is a fault of the spec on this host's data, not the host's state).

What the job sees through a lower, compared with a tree in its scratch (the overlay's own semantics): reads, `mmap`,
`exec` work; a write copies a file up into the upper; a delete is a whiteout; renaming a directory that came from a
lower fails `EXDEV` (`redirect_dir=off`; GNU `mv` then copies), renaming a file works. Writing over a lower directory
needs the job to have write permission on it as the lower presents it: a host path whose files are owned by an id the
job's mapping does not hold reads as owned by 65534, which the job cannot write into. A provider of trees a job will
write over presents them owned by the job's container identity (the host uid `sub_uid + 1000`, `Oci::container_ids`);
the agent does not change ownership (decision LR5).

### 4.5 Host ports

The agent offers each host port to the job with a listener inside the job's network namespace and a connection out of
its own:

- After `crun` has made the container and before the job's program runs, the agent opens `/proc/<pid>/ns/net` (as it
  does for a wired job today, `crates/js-agent/src/egress/host.rs`, `attach`), and on a dedicated thread enters it with
  `setns`, binds and listens on `127.0.0.1:<port>`, and returns. A socket keeps the namespace it was made in, so the
  listener is in the job's namespace while the agent's thread is back in the host's.
- Each accepted connection is joined to a new connection to `127.0.0.1:<port>` in the host's namespace and the bytes are
  copied both ways (`tokio::io::copy_bidirectional`) on a runtime of its own (`js-ports`, two threads), so a busy
  upload cannot slow the lifecycle. A refused connection to the host closes the job's connection.
- The job's program must not run before the listeners exist. Its init already waits for a route on a wired job
  (`--wait-route`); it gains `--wait-listen PORT,...`, which waits until each port is in state `LISTEN` in the job's own
  `/proc/net/tcp`, under the same 30 s bound, so no connection is made to find out.
- It works at every network level, because it needs no interface and no firewall rule: the `input` refusal of the host
  stays whole. It is TCP over IPv4, as the forwards of `ports` are.
- The listeners are released with the run (`Wired::release`'s place in `remove`). After an agent restart, an adopted
  run's listeners are made again; if a port cannot be bound again (the job has bound it itself meanwhile), the run is
  stopped and its result carries `input_failure: host-port-lost`.

Why not a firewall exception: reaching a service on the host's `127.0.0.1` from a veth needs `route_localnet` on that
interface and a DNAT, both of which loosen the rules that keep a job from its host, and a `none` job has no interface
at all. Why not a process in the namespace: `agent-egress.md` rejected a per-run holder process; a socket held by the
agent is not one (decision LR8).

### 4.6 Cleanup, restart and the integrity check

**The agent removes what it mounted, as the mounter.** The agent writes the list of a run's mount points to
`<mounts>/<lease>/mounts`, a file of its own, adding each point before it makes the mount. The ledger sidecar
(`crates/js-agent/src/crun/ledger.rs`, `Entry`) does not change: it is decoded with `deny_unknown_fields`, so a new
field would make an agent of an earlier release take this release's sidecars for foreign files. Every path that ends a
run (`CrunRuntime::remove`, `remove_leftover`, `tear_down` after a failed make, and an end in the preparing stage after
the assembly began) unmounts them deepest first (`umount2` with `MNT_DETACH`, since a lazy lower may not answer) after
`crun delete` and before the lease directory is removed, then removes `<mounts>/<lease>`.
`js_contain::remove::remove_tree` then removes the upper's whiteouts and the mode-000 `work/work` as root, as it does
today. It gains one rule as a backstop: it does not descend into a directory whose mount id (`statx`, `STATX_MNT_ID`)
differs from the tree root's, and fails instead. A device number would miss a bind of a directory of the same file
system. A mount left behind is then a cleanup failure, which taints the host, never a walk through a host directory.

**Restart.** Mounts in the host's namespace outlive the agent. An adopted run keeps them and removes them as above. After
adoption the agent unmounts and removes each `<mounts>/<lease>` of a run it did not keep, from its list file.
Image pins are unchanged.

**Rollback.** An agent of an earlier release knows nothing of `<mounts>`. It does not read or walk it, and it removes the
lease directories of the runs it does not adopt (the uppers under the overlays, which holds no host data). The mounts of
runs that lived through the rollback stay until the host reboots; they were there when that agent took its integrity
baseline, so they are no finding.

**Integrity.** The check runs after a job's container and scratch are gone (`crates/js-agent/src/integrity/mod.rs`), so
the job's own mounts are already removed. Other jobs' mounts are not, so `new-mount` excludes a mount whose point lies
under `<mounts>/<lease>/` of a lease the agent holds, as it excludes the other jobs' containers. Any other new mount is
still a finding.

### 4.7 Admission and the preparing stage

At admission (`admit`, `crates/js-agent/src/lifecycle/machine/mod.rs`), with no I/O: the start's text against the rules,
each host path's root found by prefix (`input-not-allowed`), each host port against `--host-port`, each image layer
against the registry policy (`image-not-allowed`).

Then the start waits in `Stage::Preparing` until it is ready, as a start whose image is not here waits today, but for
three things, each on a thread pool that is not the lifecycle worker:

- **its images**, the image and every image layer, each through the prefetcher (single flight per image; `prepare`
  takes the set and answers `Ready` when all are);
- **its host paths**, each resolved (4.2, steps 1 and 2) on a pool of four threads (`js-paths`) with a bound of 60 s
  each; a path that is missing or unreadable is `input-unavailable` at once, and one that has not answered by the bound
  is `input-unavailable` then;
- **the assembly**, once both are ready: the lease directory, its quota, the clones (4.2, step 3), the targets, the
  overlays (4.3, 4.4), on the same pool.

`begin` then runs as today, and finds the root and the sources made. Section 7 says why the lookups and mounts are kept
off the lifecycle worker, and what the bound means.

## 5. The agent on macOS

Not built now: the Macs are deferred, and how a Mac gives a job a writable tree over a lazy one is **not chosen here**.
It waits for measurements of the cache's real macOS mount (13, Q1). This section fixes everything that does not depend
on that choice: what the Mac agent does with each field of the same `JobStartSpec`, the Seatbelt view, and the three
candidate mechanisms with the tests that will decide between them. The wire and the server of sections 2 and 3 do not
change whichever wins, because all three realise the same thing: a `COPY_ON_WRITE` mount.

### 5.1 What applies

| Field | macOS |
|---|---|
| `layers` | refused `rootfs-unsupported-here`; placement never sends them (2.4: they require `exec=container`). A native job runs in its own directory of the host's file system; there is no root for layers to replace, under any of the mechanisms of 5.3. |
| `mounts`, `COPY_ON_WRITE`, target `/work` or below it | realised in the run's `exec/` by the mechanism chosen from 5.3 |
| `mounts`, `READ_ONLY` or `READ_WRITE`, target below `/work` | a link in `exec/` (5.2) |
| `mounts`, `READ_ONLY` or `READ_WRITE`, target exactly `/work` | refused `mount-target-unsupported-here`: the run's working directory is a directory of the run's home, which a link cannot replace |
| `mounts`, any target outside `/work` | refused `mount-target-unsupported-here` |
| `host_ports` | allowed in the Seatbelt profile (5.4) |

**Paths.** The agent does not rewrite a job's paths. `/work` names the run's working directory on both platforms: on
Linux it is that path, on macOS it is `exec/` in the run's home, and the job starts in it on both. A mount at
`/work/<rest>` appears at `<rest>` relative to the working directory, so a client that keeps its mounts under `/work`
and its program's paths relative sends the same spec to both platforms. The allow-lists are the same flags with the same
meaning (`--mount-root`, `--mount-root-ro`, `--host-port`), resolved by the same code against `realpath`-canonical roots
(`/private/var/...`, `AbsPath`, `crates/js-mac-pool/src/seatbelt/path.rs`).

**The preparing stage.** The native runtime has no `prepare` today (`Runtime::prepare` has its default, `Ready`), so
any work it does before the job's program runs is done in `create_and_start`, on the lifecycle worker. Every mechanism
of 5.3 does some (a mount, a clone, or building a tree of links), so build item 7 gives the native runtime a `prepare`
that does it on a pool off the worker, as 4.7 does on Linux, whichever is chosen.

### 5.2 Read-only and read-write mounts

Independent of 5.3:

- **`READ_ONLY`**: a symbolic link `exec/<rest>` to the canonical host path, made by the agent before the job's first
  process (tree-inputs.md 4.3 designed the same for its trees). Reads through it are the provider's own, lazily if it is
  lazy. The link is in the run's read-only list (`IN_i`), so the job cannot replace it; writes through it land outside
  the run's home, where the profile's head denies them. A working directory at or below the link is refused
  `cwd-in-tree` (an existing code), since the working-directory check (`crates/js-agent/src/native/cwd.rs`) refuses a
  link out of `exec/`.
- **`READ_WRITE`**: the same link, with the host path in a write grant of the profile (5.4); the Unix permissions of the
  host path must also let the run's user write it, which the profile cannot grant.

### 5.3 Copy-on-write mounts: three candidates, not chosen

What any candidate must give the job: at `exec/<rest>`, a tree that reads as the host path and in which the job may
create, change, rename and delete anything, with no change reaching the host path, owned and writable by the run's
user, removed with the run.

| | (a) A union view from a user-space file system | (b) APFS clones | (c) Copy on first write in js's own layer |
|---|---|---|---|
| What it is | Per run, root mounts a file system (FSKit, or FUSE-T, which serves NFS on loopback) at `exec/<rest>` that shows the host path with the run's changes in an upper directory in the home: overlayfs's semantics, served by a program of ours | Before the program runs, the agent clones the tree into `exec/<rest>` (`clonefile`, as `js-treestore`'s `Clonefile` does for a bundle's writable trees), then gives every entry to the run's user | The agent builds `exec/<rest>` as real directories and one link per file into the host path; a file the job will write is copied first, into the run's home |
| Lazy | yes: only what the job reads is fetched | no: a clone needs the source on the same APFS volume as the home; a lazy mount is not, so it is a byte copy, which reads the whole tree | yes for reads |
| Correct for every program | yes, if our file system implements copy-up, whiteouts and renames correctly | yes: the result is an ordinary tree | no: Seatbelt cannot intercept a write, so a program that opens an input for writing writes through the link and is denied rather than given a copy; it works only for files declared in advance, or for programs that replace files by rename |
| Cost per run | a mount and an unmount | proportional to the tree (a clone is a metadata operation and shares blocks; a copy reads every byte) | proportional to the number of entries (the cache measured about 2 s for 46,362 files) |
| What it needs | a file system of ours with overlay semantics; an FSKit extension approved on every Mac (the MDM layer can approve it) or FUSE-T installed; root to mount; an unmount before `user-delete`, which refuses to cross a mount (`docs/design/mac-lease-teardown.md`, the deletion's step 1); a way to end processes blocked in a hung mount | the copy `files.rs` makes of a `Mount` input, changed to give owner-writable modes and to leave the tree out of the read-only list; nothing mounted | a list of the files the job writes, or an acceptance that writes to inputs fail |
| Seatbelt | a grant of the mount's path (it is in the home, which is writable already) | nothing new: the tree is in the home | read grants on the host path behind the links; the copies are in the home |

**The tests that decide.** Each candidate is run against the cache's real macOS mount, on the same tree, and then:

1. *Semantics*: a program that opens an input read-write and changes it, truncates and appends to one, renames a file
   and a directory from the lower, deletes both, makes a hard link, changes a mode and `mmap`s a file for writing;
   every step must give what it gives on Linux's overlay (4.4), and the host path must be unchanged after.
2. *Laziness*: a tree of the size of a large build's inputs (the cache's measurement used 46,362 files), of which the
   job reads 1 percent: time before the program's first instruction, bytes fetched from the cache, and the job's time.
3. *Cost*: the same tree read whole, to bound the worst case of each.
4. *Teardown*: the run's processes killed while some are blocked in a read the cache does not answer, then
   `user-delete`; nothing of the run may remain, and nothing may block the agent.
5. *Fleet*: what each needs installed and approved on a Mac, and whether the MDM layer can do it without a person.

Candidate (c) fails test 1 by construction unless the client declares its outputs; it is listed because the brief named
it and because it is the cheapest if the cache's measurements show that build actions only replace files by rename.

### 5.4 The Seatbelt view

The profile of a run with mounts (reads stay open, `ReadModel::Open`), whichever candidate is chosen:

- Each `--mount-root`/`--mount-root-ro` is a **read deny** of every run (`NativeConfig.read_denies`), so a run cannot
  read what another run was given below the same root. An operator who keeps a tool under a mount root, which jobs read
  without mounting, moves it out; the agent already refuses a files root that overlaps a mount root at start.
- Each read-only or read-write mount of the run, and each host path a candidate reads through, is **granted** by a new
  policy item emitted after the read denies and before the floor: `(allow file-read* (subpath (param "GR_i")))`, and
  `(allow file-read-metadata (literal ...))` for each ancestor between the root and the path, so that `realpath` and
  `getcwd` work. Later rules win in Seatbelt, so the grant beats the root's deny and loses to the floor. A read-write
  mount also gets `(allow file-write* (subpath (param "GW_i")))` in the same place. (tree-inputs.md 4.2 designed the
  same item for its trees.)
- **Host ports.** The profile's network rules come from the run's level narrowed to the agent's ceiling
  (`Net::narrow`), and narrowing never widens: at a ceiling of `off` it cannot yield `Net::Ports`. So host ports are not
  a level. A separate policy item, applied after the narrowing, adds `(allow network* (remote ip "localhost:<port>"))`
  per port when the narrowed level is `Off` (which `Net::Ports` renders today as the `off` rules plus those lines);
  at `Loopback`, `On` and `Open` loopback is not filtered (pf passes loopback, `crates/js-mac-provision/src/render.rs`),
  so the port is reachable already. The operator's `--host-port`, not the ceiling, bounds the ports. Loopback is shared
  by every user of a Mac, as `seatbelt/net.rs` already says.
- The builder's lists hold at most 64 entries each (`MAX_LISTED`); a start whose links, grants or ancestors need more is
  refused `too-many-inputs-here`, an existing code.

### 5.5 Cleanup

Links and any tree built in the home go with the home at `user-delete`. Candidate (a) adds an unmount, by the agent as
the mounter, after `kill-uid` and before `user-delete`.

## 6. The tree wire of #842

komira-ai/jobscheduler#842 merged build item 1 of tree-inputs.md: `InputRef::Tree`, `TreeUnavailable`,
`JobResult.input_failure`, nine refusal codes, and the capability key `tierstore-trees`. The release workflow releases a
push to main that changes a file that ships, and #842 did, so these are in a release; B8 forbids removing or re-meaning a shipped item. Nothing ever produces them:
no server sends a tree input (`start_of` sends no `run`), no agent sets `input_failure`, no agent reports the key. So
each is retired or generalised as follows, in build item 1:

| Item | What happens | Why |
|---|---|---|
| `InputRef::Tree`, `TreeUnavailable` | **Kept, retired.** Decoded and validated as now; no server builds one; the doc comments say "retired: a tree named by digest, which no server sends", with no file system named. Every runtime refuses a start that holds one: `crun` already refuses every `run` (`run-unsupported`); the macOS runtime, which today passes over a tree input when it stages inputs, refuses it `native-unsupported` as it refuses a `Stage`. | Removing a variant of a shipped enum is not additive. A tree by digest names a namespace only one provider has; the generic input is a host path (sections 2 to 5), not a renamed tree. |
| `JobResult.input_failure` | **Generalised.** Same field, same type, same bound: "the code of an input the host provides that failed while the job ran". Its codes become `input-unreadable` and `host-port-lost` (section 8). | Its meaning, a failure of the host's input rather than of the job, is the one section 8 needs; no agent ever sent a value, so no reader meets a changed one. |
| `tree-unavailable`, `tree-invalid`, `tree-read-failed`, `tree-stalled`, `tree-placement-unsupported`, `tree-unsupported-here` | **Kept** (refusal codes are add-only), documented as retired; no agent sends them. | `refusal_codes_snapshot` pins the list. |
| `outputs-unsupported-here`, `too-many-inputs-here`, `cwd-in-tree` | **Kept and used**: the last two by 5.2 and 5.4; the first stays for a `run` with outputs. Their docs lose the word tree where it named a provider's tree. | They describe the agent's limits, not a provider. |
| `js_caps::TIERSTORE_TREES` (`tierstore-trees`) | **Removed** from `js-caps` (the constant, `DETECTED_KEYS`, the exact keys, the docs) and its tests. | It is the one item whose text names a provider. No agent of any release reports it, and a client cannot name a capability (the public `Constraints` has no free map). A server of an earlier release that knows the key meets no host reporting it. After the removal, two things change, and neither matters: a request that names the key is refused as an unknown key, where before it could never be met; and an operator's `--capability tierstore-trees=...` becomes an ordinary operator capability that nothing reads, where before it was refused at start. |
| `docs/design/agent-wire.md` | Its list of types without a catch-all keeps `TreeUnavailable`; the pointer to tree-inputs.md 2.2 goes. (`Layer` and `MountMode` join the list in build item 2.) | |
| `docs/design/tree-inputs.md` | Marked superseded by this document at its head; kept, as `jobscheduler-design.md` 0.3 keeps every older document. | |

The tests that pin the tree wire stay and keep asserting what they assert: `a_tree_start_reads_as_unknown_to_the_previous_release`
(a release before #842 still reads such a start as unknown), `tree_input_wire_form` (the retired form still has the
same text), `an_old_server_ignores_input_failure`. The tests of `tierstore_keys.rs` and `flag_cannot_claim_tierstore_trees`
go with the key; that is the removal of a key, not a weakened assertion over a gap (T7), and the PR says so.

**Migration note.** No deployment uses tree inputs. A client that planned to send `JobSpec.trees` (tree-inputs.md 2.4,
never built) sends instead a `COPY_ON_WRITE` mount of the provider's directory for the tree at `/work`, or a
`READ_ONLY` mount at `/work/<dest>`; it asks its provider to fetch ahead when the job is placed (section 7); and it
reads `START_FAILURE_CODE_INPUT_UNAVAILABLE` and `RESULT_REASON_INPUT_FAILED` where tree-inputs.md had `tree-unavailable`
and `INPUT_FAILED`. The fallback `TreeUnavailable::JobStages` has no successor: a client that wants to stage inputs
itself when the host path is missing submits a job without the mount.

## 7. Readiness and timing

**No special fetch.** The agent never asks a provider for anything. A host path is a directory; the agent opens its
root (4.2) and, at the overlay's mount, the kernel looks up the lowers' roots; every other read is the job's, when the
job makes it, through the kernel, to whatever file system is there. On the farm that is a lazy FUSE file system, which
fetches on first read; jobscheduler neither knows nor needs to.

**Where the time goes.**

| Time | Counted as | Bounded by |
|---|---|---|
| image and image-layer pulls | the preparing stage (`starting`) | the prefetcher's per-pull limit (30 min) and the server's start timeout (900 s from placement) |
| a host path's root lookup | the preparing stage | 60 s per path (4.7), then the start timeout |
| the clones and overlay mounts | the preparing stage | the start timeout |
| reads of a lazy lower while the job runs | the job's wall clock | the job's wall limit |
| on macOS, the work a candidate of 5.3 does before the program (a mount, a clone, links) | the preparing stage, once the native runtime has one (5.1) | the start timeout |

So on Linux a lazy input is paid for while the job runs, and a client sets the wall limit with that in mind. The time
a job spent preparing is visible already: `Job.times.started_at_ms - Job.times.placed_at_ms` (`JobTimes`, job.proto),
which includes the heartbeat that carried the start. Nothing new is reported.

Why the lookups and mounts stay off the lifecycle worker: a lookup on a FUSE file system blocks in the kernel until
the file system answers, and one that has hung never returns; the worker serves every other job. The `js-paths` pool
has four threads and makes none on demand, so a hung provider holds at most four threads, and further lookups wait their
60 s and fail `input-unavailable`. A thread stuck in the kernel stays stuck until the file system answers or is
unmounted; that is the provider's to fix, and the host reports it only through failed starts.

**How a client sees the job being made, and when to fetch ahead.** The public API has no stream: a client polls
`GetJob` (`crates/js-client/src/jobs/loops.rs`, `wait_for_terminal`). The moments it can see:

- `PLACED`, with `Job.host_id` set: the server has chosen the host; the start goes out on that host's next heartbeat.
  This is the first moment the client knows which node will read the inputs.
- `STARTING`: the host's agent has the start and is preparing it ("creating": pulling images, resolving host paths,
  mounting).
- `RUNNING`: the job's program has started; its wall clock runs.

A client with a node-local cache asks that node's cache to fetch ahead at the first poll that shows a `host_id`, which is
`PLACED` or, if the poll came late, `STARTING`; that is the "creating" moment of the owner's flow, taken as early as the
API allows. Fetching ahead only warms the cache: if it is late, or fails, the job still reads every file it needs, on
first read. A client that wants the moment sooner polls faster while the job is `QUEUED` or `PLACED`; this design adds
no watch (decision LR12).

## 8. Failures

Every failure ends in one of three places: the start fails (nothing ran), the job's result says an input failed (it ran,
and the host's input is to blame), or the job's own result stands.

| Failure | Learnt | Result |
|---|---|---|
| A host path under no root, a read-write mount under a read-only root, a host port not allowed | admission | start fails `input-not-allowed`; public `START_FAILURE_CODE_AGENT_REFUSED`, detail `input-not-allowed` |
| An image or image layer outside `--image-allow` | admission | start fails `image-not-allowed`, as today |
| A host path missing, not a directory, unreadable, or leading out of its root by a link | preparing | start fails `input-unavailable`, or `input-not-allowed` for the link; public `START_FAILURE_CODE_INPUT_UNAVAILABLE` / `AGENT_REFUSED` |
| A host path's lookup passes 60 s | preparing | start fails `input-unavailable` |
| An image layer's pull fails | preparing | as an image's pull fails today |
| An overlay or clone mount fails (the kernel refuses a lower, the upper's volume is not local) | preparing | start fails with the existing runtime failure code; it is the host's configuration |
| A read of a lazy lower fails while the job runs (`EIO`) | the job | the job's own result: the agent cannot tell one file's failed read from the job's |
| The job did not succeed, and after it ended a host path of its layers or mounts no longer answers its lookup (`ENOTCONN`, `EIO`, `ESTALE`, or no answer within 5 s) | after exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: input-unreadable`, exit code kept |
| A host port cannot be offered again after an agent restart | adoption | the run is stopped; `FAILED`, reason `INPUT_FAILED`, `input_failure: host-port-lost` |
| The host's service behind a host port refuses or drops a connection | the job | the job's own result |
| The job's writes fill its quota | the job | the job's own result (`ENOSPC`), as today |

The after-exit check is the one place the agent looks at an input after the job. It looks at what the job used, never
at the path text again: the agent keeps an `O_PATH` descriptor of each clone's root (`<mounts>/<lease>/src/<i>`) from
the assembly, and the check is an `fstatfs` and a `getdents` of one entry through it. It runs on the `js-paths` pool,
before the result is built (`Event::ResultReady`), only for a job that did not succeed: a job that succeeded read what
it needed. It catches the case that matters most, a provider whose whole file system went away under a running job, so
that a compiler's "cannot read file" is not reported as the user's build error; it does not catch one file's failed
fill, which only the provider knows, and which a client with a provider asks that provider about (decision LR11).

**The detail reaches the client.** `JobState::FailedToStart` gains the agent's code, stored in a new tag of the job's
row and shown in `Job.start_failure_detail`; `end_of` maps `input-unavailable` to `StartFailure::InputUnavailable` and
every other refusal to `AgentRefused`, with the code. A result with `input_failure` maps to `FAILED` with reason
`INPUT_FAILED` and the code, whatever its outcome; a result without it maps as today.

## 9. Compatibility (B8)

- **Public API.** `JobSpec.host_ports` (32), `layers` (60), `mounts` (61), the messages `Layer`, `Mount`, `HostPort`,
  the enum `MountMode`, `START_FAILURE_CODE_INPUT_UNAVAILABLE`, `RESULT_REASON_INPUT_FAILED` and `JobResult.input_failure`
  (12) are additive. A client of an earlier release reads the new enum values as unspecified, as each enum's comment
  says. The dangerous direction is a new client against an earlier server: the server drops the unknown fields and runs
  the job without its layers. Servers upgrade before clients that use the fields (as before agents), and such a client
  checks that the job the submit answers with (`Job.spec`) lists them, and cancels it if not. The proto test of build
  item 3 shows the drop.
- **The stored row.** Four new tags; an earlier server keeps unknown tags through a rewrite (`Carried`), and a missing
  tag reads as empty.
- **Agent wire.** The three `JobStartSpec` fields are absent when empty. An agent of an earlier release ignores them,
  which is why placement and `start_of` require `layered-rootfs` (2.4). `Layer` and `MountMode` have no catch-all: an
  agent that meets an unknown kind fails that one start's decode, as for `InputRef`.
- **The agent's files.** The ledger sidecar does not change (4.6). The list of a run's mounts is a new file in a new
  directory, `<mounts>`, that no earlier release reads or sweeps; 4.6 says what a rollback leaves.
- **macOS.** A mount root becomes a read deny of every run (5.4). A job that read a tool under a mount root without
  mounting it no longer can; the operator keeps such tools outside the roots.
- **The tree wire** is retired additively (section 6); only the never-reported capability key is removed.
- **Kernel.** Linux 5.6 for `openat2`; `layered-rootfs` is reported only when the agent's start-time check finds the
  three system calls (a `ENOSYS` means the capability is not reported, with a warning), so an older kernel runs jobs
  as before and never gets a layered one.

## 10. Tests

Each pull request of section 11 carries its own. Every test is hermetic (T12): host paths are directories in the test's
scratch directory; a slow or hung lazy file system is a fake opener behind the resolver's trait, released by the test
with an explicit event, never a real FUSE mount; every bound (60 s per lookup, 5 s after exit, 30 s for listeners) is
run on the agent's injected clock, never waited for (T10); a host port is a listener the test binds on `127.0.0.1`
port 0.

**Wire and server** (`js-agent-wire`, `js-proto`, `js-jobs`, `js-lease-server`, `js-sched-placer`, `js-caps`):

- goldens of a start with every new field and with each minimal form; validation of each rule of 2.3 at its boundary
  (16 and 17 layers, 64 and 65 mounts, 16 and 17 ports, a target of 4096 and 4097 bytes, `/`, each runtime place, a
  relative path, `..`, two targets equal in different case, a mount below another accepted, an empty image without
  layers refused and with layers accepted, a relative command with an empty image refused, a layer with no source and
  one with two, a host path not in normal form, with a NUL, and of 4096 and 4097 bytes, a port given twice, `HOST`
  isolation with each field, an unspecified mode stored and read as read-only);
- idempotent resubmit: the same job with the same layers, mounts and host ports is the same job; a change of any one,
  or of their order, is `job-id-conflict`;
- previous release: an agent of the previous release decodes a start with layers and drops them (the hazard 2.4
  guards against, pinned); a start without the fields is the previous text byte for byte; an unknown `Layer` kind or
  `MountMode` reads as an unknown command beside intact siblings; the previous proto envelope and the new one read each
  other (the `isolation.rs` pattern), and a previous server drops the fields (9);
- placement: a job with any field requires `layered-rootfs`, and one with layers also `exec=container`, overwriting a
  client's own values; placed on the reporting host of two and never on the other; `start_of` refuses a host without
  it (`RootfsNotReported`); a job without the fields starts as today;
- `end_of` maps `input-unavailable`, `input-not-allowed` and an unknown code, and a result with each `input_failure`
  and each outcome; the start failure detail round-trips through the stored row and `GetJob`;
- the tree wire's tests of section 6 pass unchanged; `tierstore-trees` is an unknown key to a request after its removal.

**Agent, Linux** (`js-agent`, with fakes):

- resolution (4.2): a path under a read-only root, read-only (accepted) and read-write (`input-not-allowed`); the
  longest of two nested roots chosen; a path under no root; a link inside a root to inside it (accepted) and to outside
  it (`input-not-allowed`); a missing path, a file instead of a directory, an unreadable directory
  (`input-unavailable`); a path swapped for a link after resolution leaves the clone where it was;
- the preparing stage: the job stays `starting` while the fake opener holds a lookup and starts when it is released;
  another job is admitted, started and ended meanwhile; a lookup held past 60 s fails `input-unavailable`; four held
  lookups and a fifth that waits its bound; an image layer pending and a host path ready, and the reverse; a stop, a
  cancel and a lease end while preparing, each after the assembly began, unmount what was mounted;
- admission: an image layer outside `--image-allow` refused `image-not-allowed`; an image layer from a registry on
  `127.0.0.1` with a port, given by `--image-allow-loopback`, pulled over plain HTTP from an in-process registry on
  port 0; `--image-allow-loopback` refused at start for a host that is not a loopback address and for an entry without a
  port; an image layer's failed pull failing the start as an image's does;
- assembly: a target below a read-only mount that does not exist in it refused `invalid-mount`; a mount source the job
  user cannot search refused `input-unavailable`; a failed overlay or clone mount from the fake failing the start with
  the runtime failure code and unmounting what was made before it;
- a layers-only root: with an empty image, the job's command run as given with the default `PATH`;
- cleanup: the list file names every mount before it is made; `remove`, `remove_leftover`, `tear_down` and the
  post-adoption sweep unmount deepest first and remove `<mounts>/<lease>`; `remove_tree` refuses to cross a mount, a
  bind of the same file system included; `<mounts>` bound once at a first start and found, not bound again, at a
  restart with an adopted run's mounts below it;
- previous release: the start sweep of the previous release, run over a scratch root and a `<mounts>` holding a live
  layered lease with a read-write mount, leaves the host directory behind the mount whole;
- integrity: another live lease's mount under `<mounts>` is not a finding; the same mount with no lease, and any other
  new mount, is `new-mount`;
- the kernel check: a fake that answers `ENOSYS` for each of the four system calls leaves `layered-rootfs` unreported,
  with the warning;
- a job whose read of a lower fails while it runs, and one that fills its quota, keep their own results, with no
  `input_failure`;
- the after-exit check: a failed job whose fake source answers `ENOTCONN`, `EIO`, `ESTALE`, and does not answer in 5 s,
  gets `input-unreadable`; a failed job whose sources answer, and a succeeded job whose source does not, get none;
- host ports: bytes both ways between a job-side client and a host-side listener; the job's close and the host's close
  each close the other side; a port not in `--host-port` refused; a host side that refuses closes the job's connection;
  `--wait-listen` waits for `LISTEN` and gives up at its bound; adoption re-makes a listener, and stops the run with
  `host-port-lost` when the port is taken.

The resolver, the forwarder's namespace step and the mounts are behind traits so the cases above run as the ordinary
user; the system calls themselves run in the root suites that exist.

**Root suites** (`crates/js-contain/tests/crun_root`, run by the existing `crun-root` job of `ci.yml`; no new job): a
container over a root of three lowers (an image's tree and two directories): it reads a file of each, sees the top
one's where two have the same name, writes a file and deletes a lower file and a lower directory (whiteouts in the
upper), and the lowers are unchanged after; a read-only mount refuses a write `EROFS`; a read-write mount's write
reaches the host directory; a copy-on-write mount at `/work` takes a write that does not reach the host directory; a
host port reaches a listener of the test's in the host's namespace at level `none`; a clone of a shared source with a
mount below it is unmounted, and the source's own mount below it is still there; the lease directory is removed whole
by `remove_tree` after the unmounts, and nothing of it remains.

**Agent, macOS** (`js-mac-pool`, `js-agent`, in the macOS job that runs the workspace's tests), whichever candidate of
5.3 is chosen: the profile text of a run with a read-only and a read-write mount and a host port, asserted whole, at a
ceiling of `off` and of `loopback`; `layers` refused `rootfs-unsupported-here`; a target outside `/work`, and a
read-only or read-write mount at `/work` itself, refused `mount-target-unsupported-here`; 64 listed entries accepted and
65 refused; a working directory below a read-only link refused `cwd-in-tree`; a start holding a tree input refused
`native-unsupported`; a `sandbox-exec` behaviour test in which a run reads its granted path, is denied the root's other
paths and a write to a read-only mount, and reaches a granted host port and not another. The chosen candidate brings
the semantics test of 5.3 (test 1) as a behaviour test of its own.

**Why this list is complete for the failure and boundary cases.** It takes each row of the failure table of section 8,
each rule of 2.3 at both sides of its bound, each refusal code of section 3 and each existing code this design gives a
new use (`image-not-allowed`, `invalid-mount`, `native-unsupported`, `cwd-in-tree`, `too-many-inputs-here`), each flag
of 4.1, each place a run ends (`remove`,
`remove_leftover`, `tear_down`, each end of the preparing stage, the adoption sweep) with mounts present, both
directions of each compatibility edge of section 9 (client and server, server and agent, previous release and this),
for the resolver each way a path can fail to be inside its root (no root, wrong kind, link out, missing, wrong type,
unreadable, swapped after the check), and for each bound one case inside and one past it.

## 11. Build items

One subject per pull request, in this order; each names its tests from section 10.

1. **Retire the tree wire** (`js-agent-wire`, `js-caps`, `js-agent`): section 6, whole: the docs, the explicit macOS
   refusal of a tree input, the generalised `input_failure` doc, the removed key and its tests, `agent-wire.md`'s pointer.
2. **The agent wire** (`js-agent-wire`, `js-types`, `js-caps`): `Layer`, `PathMount`, `MountMode`, the three
   `JobStartSpec` fields, `image` optional with layers, the shared path rules moved to `js-types`, the limits, the
   refusal codes of section 3, the `layered-rootfs` key, `Layer` and `MountMode` in `agent-wire.md`'s list; goldens and
   previous-release tests.
3. **The public API and the server** (`js-proto`, `js-jobs`, `js-lease-server`, `js-sched-placer`, `js-client`,
   `js-cli`): the proto fields and enums of 2.1, the stored tags, `validate`, `same_job_as`, the read view, the
   inserted constraints, `start_of` and `RootfsNotReported`, the start failure detail and `end_of`; the regenerated
   client and CLI surface and their goldens (`cargo xtask gen-job-surface`), `docs/job-surface.md`. Nothing reaches an
   agent that does not report the key, and none does yet.
4. **Mounts in `js-contain`**: the resolver's system calls (`openat2`, `open_tree`, `move_mount`, `mount_setattr`), the
   overlay the agent mounts for a root and for a copy-on-write mount, the unmounts, `remove_tree`'s mount rule, the
   private `<mounts>`; the root suite cases.
5. **The `crun` agent's file system** (`js-agent`): admission, the `js-paths` pool and the preparing stage, the multi-image
   prepare, the assembly, the list of mounts and every cleanup path, the integrity exclusion, `--image-allow-loopback`,
   accepting `--mount-root`/`--mount-root-ro` with `crun`, the after-exit check, the kernel check and the
   `layered-rootfs` report.
6. **Host ports on `crun`** (`js-agent`, `js-contain` for the init's `--wait-listen`): `--host-port`, the forwarder,
   adoption.
7. **macOS** (`js-agent`, `js-mac-pool`), when the Macs are built and after the candidate of 5.3 is chosen: the native
   runtime's `prepare`, the links, the chosen candidate, the profile items of 5.4, the refusals.
8. **An end-to-end run on one Linux node** with a FUSE file system as a layer's and a mount's source, a registry on
   `127.0.0.1`, and a service on a host port: the job reads lazily, writes copy up, the upload reaches the service, the
   run directory is removed, the integrity check passes, and a restart of the FUSE file system's service while no job
   runs leaves later jobs working. A runbook entry under `docs/runbooks/`, not a test of the PR set (B9). It also
   measures the forwarder's throughput for an upload of a build's size (13, Q2).

Item 1 stands alone. Item 3 needs 2, and 4 to 6 need 2; until 5 lands no agent reports `layered-rootfs`, so items 2
and 3 change no running job. kbf's side (sending the fields, asking its cache to fetch ahead, its own wall limits) is
kbf's design.

## 12. Decisions

Each is decided here; the owner may overturn any.

- **LR1. The operator's allow-list of host paths is the existing mount-root table, read as prefixes.** A second list
  would be a second way to say the same thing; the table already has the canonicalisation and its tests, and only the
  race kept it off `crun` (4.1, 4.2).
- **LR2. Jobs name absolute host paths, not a root and a relative path.** The owner's flow passes a file path; the
  roots bound what it may be. The legacy named form stays for the legacy job form.
- **LR3. The agent mounts the root's overlay as root and gives it to `crun`**, as Docker's daemon does; `crun` cannot
  overlay `/` (4.3). Cleanup is the agent's, as the mounter (4.6).
- **LR4. An image layer is the image's flattened tree.** The store keeps one tree per image; per-layer storage with
  whiteouts kept is not asked for (B20). Two image layers stack as whole trees.
- **LR5. The agent does not change the ownership of a host path.** Idmapped mounts of arbitrary file systems are not
  available on every kernel and file system the farm uses; a provider presents writable trees owned by the job's
  container identity (4.4).
- **LR6. The allow-lists are checked by the agent, not advertised.** Prefix lists do not fit a capability value, and a
  pool's hosts share a configuration; a mismatch is a configuration mistake with a named refusal (2.4).
- **LR7. Every host path is resolved with `openat2` beneath its root and used through a clone** (4.2), so no path text
  is read twice and none reaches a mount option.
- **LR8. Host ports are a listener the agent holds in the job's namespace**, not a firewall exception or a process in
  the namespace (4.5); they work at every level, `none` included.
- **LR9. `host_ports` is a list of messages**, so a protocol or address can be added without a second field; a job
  reaches a port at the same number on its own loopback, which is all macOS can offer (5.4).
- **LR10. On macOS a read-only or read-write mount is a link; how a copy-on-write mount is made is not chosen** (5.3).
  The owner decides it from measurements of the cache's real macOS mount. Every candidate realises the same
  `COPY_ON_WRITE` mode, so the spec does not change whichever wins.
- **LR11. The agent checks a failed job's host paths once after it ends**, and nothing while it runs (section 8).
- **LR12. No new state or watch for "creating".** `PLACED` with a host and `STARTING` are the moments a client can
  use, and `JobTimes` already gives the time spent preparing (section 7).
- **LR13. The tree wire is retired additively; only the never-reported capability key is removed** (section 6).
- **LR14. `input-unavailable` is a public start failure code of its own; a refused allow-list is not.** The first is
  the host's state and worth a retry elsewhere; the second is a configuration mistake, named in the detail.
- **LR15. Images keep today's failure mapping.** A failed pull of an image layer fails as a failed pull of the image
  does, so one rule covers both.
- **LR16. The agent's mounts live in `/run/js-agent/mounts`, and their list in a file there, not in the ledger** (4.3,
  4.6). The scratch root is swept by every release, and an earlier release reads the ledger strictly.

## 13. Open questions

- **Q1. Which candidate makes a copy-on-write mount on macOS** (5.3): a union view, APFS clones, or copy on first
  write. It is decided after the cache's real macOS mount is measured, by the five tests of 5.3.
- **Q2. The forwarder's throughput.** Every byte of an upload through a host port is copied by the agent. If build item
  8 measures it short of what the farm's uploads need, the next step is `splice(2)` between the two sockets, then a
  per-job Unix socket mount for providers that offer one; neither changes the spec.
- **Q3. Retrying a start that failed `input-unavailable`** on another host is the client's today (a start failure ends
  the job). Whether the server should place it again belongs with `retries-and-runs.md`, not here.
