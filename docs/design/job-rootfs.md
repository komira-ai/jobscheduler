# A job's file system: layers, mounts and host ports

Status: **design for the owner's review; of its build items (section 11), item 2, the agent wire, item 3, the
public API and the server, item 4, the mounts in `js-contain`, item 5, the `crun` agent's file system, and item 6, host
ports on `crun`, are built.** It replaces `docs/design/tree-inputs.md` and its build
items: jobscheduler does not know that a node-local cache exists, let alone which one. What tree-inputs.md asked of
jobscheduler is met here by things any container runtime offers (layers, mounts, and access to a host port or socket),
and the pieces of its wire that already merged (komira-ai/jobscheduler#842) are retired additively (section 6).

**What changes for a user.** Today a job's file system is one image, read-only, with an empty writable `/work` in which
it runs (section 1). After this design a job can also say, as it would to Docker:

- **layers**: an ordered list of read-only trees stacked on its image, or in place of it, each either a directory on the
  host or another image from a registry the host allows. The job's root is the stack plus a private writable layer on
  the host's scratch volume, so writes never reach a layer;
- **mounts**: a tree at a path in the job's file system, read-only, read-write, or copy-on-write (the job may change
  it; the changes are its own). The tree comes from a source: a host directory now; a memory-backed `tmpfs` and an
  image's tree are designed here and built when a client needs them. A mount has named options (`sub_path`, `no_exec`,
  `propagation`), each with a default;
- **host ports**: a TCP port on the host's loopback that the job reaches on its own loopback, at the same port or one
  it names (UDP designed, not built);
- **host sockets** (designed, not built): a Unix socket on the host that the job connects to at a path of its own.

Section 2.1 sets every one of these beside Docker's `--mount` and `-p`, the OCI runtime specification's `mounts`, and
Kubernetes' volumes: each is a kind one of them already has, and none is shaped by one client. The host's operator
decides which host paths, registries, ports and sockets a job may name, by prefix and by port; a start that names
anything else fails by name. What does not change: a job that uses none of them runs exactly as today, byte for byte on
the wire, on every host; and nothing in jobscheduler's code, wire, flags, capabilities or tests names a particular
client, cache, file system or storage service.

**Where this comes from.** A client that keeps a node-local store of trees and images wants a job's inputs read lazily
from that store instead of downloaded, and its outputs sent to a service on the same node. This document needs only
what any such client would need: a host path can be a layer or a mount (on such a node, a lazy FUSE directory), an
image can come from a registry on `127.0.0.1`, and a job can reach a port, or later a Unix socket, of the host. Which
client that is, and how it uses its store, is that client's design. Section 7 states the job's lifecycle as a client
can observe it, which is when a client with a node-local store warms it.

Contents: 1 what exists today; 2 the job spec; 3 the agent wire; 4 the agent on Linux; 5 the agent on macOS; 6 the tree
wire of #842; 7 the lifecycle a client can rely on; 8 failures; 9 compatibility; 10 tests; 11 build items; 12
decisions; 13 open questions.

## 1. What exists today

Read in code; every later section builds on these facts.

- **The root is one flattened image, read-only.** The agent pulls a digest-pinned image, unpacks its layers into one
  tree `oci-images/<hex>/rootfs` of the agent's image store, with whiteouts applied (`crates/js-contain/src/unpack.rs`;
  the whiteout entries themselves are never written), and gives that tree to `crun` as `root.path`, read-only
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

The interface says, generically, "put these trees at these paths in this way, and let the job reach these host
services". It names kinds of source (a host directory, an image, memory), never a client or a storage system; a client
of any kind maps what it has onto those kinds. It grows by adding kinds and options (2.4), never by changing one.

### 2.1 The interface beside Docker, the OCI runtime and Kubernetes

Docker's `--mount` takes a `type` (`bind`, `volume`, `tmpfs`, `image`; `npipe` and `cluster` are Windows' and Swarm's)
and `key=value` options per type. The OCI runtime specification, which `crun` reads, has a list of `mounts`, each a
`destination`, a `type`, a `source` and a list of `options` strings in `fstab` style, plus `uidMappings`/`gidMappings`
for an id-mapped mount; its root is `root.path`. Kubernetes splits a volume's source (`hostPath`, `emptyDir`, `image`,
`configMap`, `secret`, `persistentVolumeClaim`, `csi`, ...) from its `volumeMounts` (`mountPath`, `readOnly`,
`recursiveReadOnly`, `subPath`, `subPathExpr`, `mountPropagation`), and a container's `ports` carry a `protocol`.
Ours follows the same split: a source (a `oneof`) and how it appears (target, mode, named options).

| What a job wants | Docker | OCI runtime spec | Kubernetes | Ours | When |
|---|---|---|---|---|---|
| A read-only tree under its root | the image's layers | `root.path` (the runtime does not stack) | the image | `JobSpec.layers` (`Layer.host_path`, `Layer.image`) | now |
| A host directory at a path | `type=bind` | `type: bind`, `options: [rbind, ...]` | `hostPath` (`type: Directory`) | `Mount.host_path` | now |
| Read-only | `readonly` | `ro`; `rro` for recursive | `readOnly`; `recursiveReadOnly` | `MOUNT_MODE_READ_ONLY`, always recursive | now |
| Read-write | the default | `rw` | the default | `MOUNT_MODE_READ_WRITE` | now |
| Writable, changes kept apart | none per mount (an image's own upper layer) | `type: overlay` with `lowerdir`/`upperdir` options | none | `MOUNT_MODE_COPY_ON_WRITE` | now |
| A sub-directory of the source | `volume-subpath`, `image-subpath` | none (the source names it) | `subPath`; `subPathExpr` | `Mount.sub_path`; no expressions | now |
| No programs run from it | not on `--mount`: a local volume's `volume-opt=o=noexec`, or `--tmpfs PATH:noexec` | `noexec` | `volumeMounts[].bindMountOptions` (alpha) | `Mount.no_exec` | now |
| No set-id programs, no device files | not on `--mount`: a local volume's `o=` options, or `--tmpfs` options | `nosuid`, `nodev` | `bindMountOptions` (alpha); no default | always on; not an option | now |
| Propagation | `bind-propagation` (default `rprivate`) | `rprivate`, `rslave`, `rshared`, ... | `mountPropagation` (`None`, `HostToContainer`, `Bidirectional`) | `Mount.propagation`: private only | now; others refused |
| Memory-backed scratch | `type=tmpfs` (`tmpfs-size`, `tmpfs-mode`) | `type: tmpfs` with `size=` | `emptyDir` with `medium: Memory`, `sizeLimit` | `Mount.tmpfs` (`size_bytes`) | designed (2.4), later |
| An image's tree at a path | `type=image` | a bind of a tree the runtime unpacked | `image` volume (read-only) | `Mount.image` | designed (2.4), later |
| Disk scratch | an anonymous volume | none | `emptyDir` | `/work` and `/scratch` (exist) | exists |
| Small files from the caller | none per mount | none | `configMap`, `secret`, `projected` | `JobSpec.files` (exists) | exists |
| A host Unix socket at a path | `type=bind` of the socket | `type: bind` of the socket | `hostPath` (`type: Socket`) | `JobSpec.host_sockets` (`HostSocket`) | designed (2.4), later |
| A host port forwarded into the job | `-p HOST:CONTAINER/PROTO` | none (the runtime's network) | `ports[].hostPort`, `containerPort`, `protocol` | `JobSpec.ports` (`PortForward`, exists) | exists |
| A service on the host's loopback, from the job | `--network host`, or `host-gateway` with a route | none | `hostNetwork: true` | `JobSpec.host_ports` (`HostPort`: host port, job port, protocol) | now, TCP; UDP designed, later |
| A volume the runtime keeps between jobs | `type=volume` (named) | none | `persistentVolumeClaim`, `csi`, `ephemeral` | **no** | never: see below |
| A network or device file system | `type=volume` with a driver (`nfs`, `cifs`) | `type: nfs`, ... | `nfs`, `iscsi`, `csi`, ... | **no** | never: see below |
| Id-mapped ownership | daemon-wide `userns-remap` | `uidMappings`, `gidMappings`, `idmap` | `hostUsers: false` | `Mount.id_mapping` (`IdMapping`) | designed (2.4), later (LR5) |
| Free option strings | `volume-opt` | `options` | a volume's `mountOptions` | **no**: named fields only (LR18) | never |
| Expanded paths | none | none | `subPathExpr` | **no** | not now |
| Facts about the job as files | none | none | `downwardAPI` | **no**: the job's `env` carries what a client sends | not now |

Every row we support is a kind at least one of the three already has, with the same meaning, so none is shaped by one
client. What we refuse, and why:

- **Volumes the runtime keeps, and network or device file systems.** Each makes the runtime the owner of state that
  outlives a job, or makes the agent a client of a storage protocol. A provider of either mounts it on the host by its
  own means; a job names the resulting directory as a host path, which the operator bounds (4.1). This keeps the agent
  ignorant of every storage system, and it is what Kubernetes' `hostPath` is to a node-local driver.
- **Free option strings.** An `fstab`-style list cannot be validated at submit, means different things to different
  runtimes, and gives a job a way to ask for `suid`, `dev` or `shared`. Options are named fields, each validated, each
  added additively (LR18).
- **Propagation other than private.** `shared` or `Bidirectional` lets the job's mounts reach the host;
  `HostToContainer` (`rslave`) lets a mount the host makes later appear in the job. The first is refused for good; the
  second is designed as a later value if a client needs it (2.4).
- **Clearing `nosuid` or `nodev`.** No job may run a set-id program from a mount or open a device file through one;
  there is no option to clear either (LR18).

### 2.2 The fields

All additive to `crates/js-proto/proto/jobscheduler/v1/job.proto`. The numbers follow the file's grouping in tens: the
network group (30s) takes the host ports and host sockets, and a new 60s group takes the file system. Nothing of this
section is built, so its numbers are set here freely; once built they are fixed (B8). What is marked *designed* below
lands in the proto file with its own build item (section 11), at the number given here, and not before.

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

// How a mount's tree appears in the job.
enum MountMode {
  // Read as read-only in a request. The server stores and answers the explicit value, so a
  // job it returns never holds this; a reader that meets it, or a value it does not know,
  // treats the mount as one it cannot read.
  MOUNT_MODE_UNSPECIFIED = 0;
  // The job cannot write the tree, nor anything mounted below it in the source.
  MOUNT_MODE_READ_ONLY = 1;
  // The job's writes reach the source. For a host path, the host's operator lists the
  // prefixes a job may mount read-write.
  MOUNT_MODE_READ_WRITE = 2;
  // The job may change the tree; its changes are its own, kept in the job's scratch, and
  // never reach the source.
  MOUNT_MODE_COPY_ON_WRITE = 3;
}

// Whether mounts made on one side of a mount appear on the other.
enum MountPropagation {
  // Read as private in a request; never in a job the server returns, as for MountMode.
  MOUNT_PROPAGATION_UNSPECIFIED = 0;
  // Neither side sees the other's later mounts: the job sees what was mounted below the
  // source when its start was prepared, and nothing the host mounts or unmounts after.
  MOUNT_PROPAGATION_PRIVATE = 1;
}

// A tree at a path in the job's file system.
message Mount {
  // Where the tree comes from: exactly one. Numbers 1 to 9 are kept for sources.
  oneof source {
    // An absolute path of a directory on the host, as Layer.host_path.
    string host_path = 1;
    // Designed (section 2.4), added with its build item:
    //   TmpfsSource tmpfs = 2;
    //   ImageSource image = 3;
  }
  // Where it appears: an absolute path in the job's file system, not "/".
  string target = 10;
  // Unspecified: read-only.
  MountMode mode = 11;
  // A relative path in the source whose directory is mounted instead of the source's root.
  // It is resolved beneath the source: a link in it may not lead out of the source. Empty:
  // the source's root.
  string sub_path = 12;
  // No file in the mount may be executed or mapped executable (noexec); an interpreter can
  // still read one. False: as the files' modes allow. Set-id bits and device files are
  // ignored in every mount, whatever this says.
  bool no_exec = 13;
  // Unspecified: private.
  MountPropagation propagation = 14;
  // Designed (section 2.4), added with its build item:
  //   IdMapping id_mapping = 15;
}

// The transport of a host port.
enum PortProtocol {
  // Read as TCP in a request; never in a job the server returns, as for MountMode.
  PORT_PROTOCOL_UNSPECIFIED = 0;
  PORT_PROTOCOL_TCP = 1;
  // Designed (section 2.4), added with its build item:
  //   PORT_PROTOCOL_UDP = 2;
}

// A port on the host's loopback (127.0.0.1) that the job reaches at a port of its own
// loopback. Not a PortForward, which is the other direction.
message HostPort {
  // The port on the host's loopback: 1 to 65535. The host's operator lists the ports a job
  // may name.
  uint32 host_port = 1;
  // The port on the job's loopback at which it reaches host_port: 1 to 65535. Absent: the
  // same number as host_port.
  optional uint32 container_port = 2;
  // Unspecified: TCP.
  PortProtocol protocol = 3;
}

message JobSpec {
  ...
  // Ports on the host's loopback the job may connect to, each at a port of its own
  // loopback, whatever its network level. Empty: none.
  repeated HostPort host_ports = 32;
  // Designed (section 2.4), added with its build item:
  //   repeated HostSocket host_sockets = 33;
  // The read-only layers of the job's root, base first, stacked on `image` (or in place of
  // it, when `image` is empty); a later layer's file hides an earlier one's. With layers,
  // the root is writable and the writes are the job's own. Empty: the root is `image`
  // alone, read-only, as before.
  repeated Layer layers = 60;
  // Trees at paths in the job's file system. Empty: none.
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

The name `host_ports` is distinct from `ports` (`PortForward`) on purpose: the two go in opposite directions. `HostPort`
names its two ports as `PortForward` does (`host_port`, `container_port`), so the same words mean the same ports in
both, and `PortProtocol` is named for any port so that `PortForward` can take it later (decision LR9).

### 2.3 Meaning

- **The root.** Bottom to top: `image` when it is set, then `layers` in order, then a private writable layer on the
  host's scratch volume. With no `layers`, the root is `image` alone and read-only, exactly as today. With `layers` and
  an empty `image`, the root is the layers alone; there is then no image configuration, so `command` is the whole
  program, an absolute path in the root, and the environment is the job's `env` over a default `PATH`. With `image` set,
  its configuration applies as today; the configurations of image layers never do.
- **`/work`** stays the job's working directory and stays an empty writable directory unless a mount targets it. A
  `COPY_ON_WRITE` mount at `/work` is how a job runs in a writable copy of an input tree.
- **Mounts** are made after the root, parents before children: a mount may lie below another one (`/work` copy-on-write
  and `/work/sdk` read-only), two mounts may not have the same target.
- **A mount's options** mean the same for every source: `sub_path` picks a directory of the source, resolved beneath it;
  `no_exec` forbids executing or mapping executable a file of the mount (an interpreter can still read one);
  `propagation` is private. Every mount is also `nosuid` and `nodev`, and a read-only one is read-only all the way down
  (what the source has mounted below it, too).
- **Host ports** are reachable at every network level, `none` included, and are the only way a job reaches its host
  (host sockets, designed in 2.4, would be the second). A job connects to `127.0.0.1:<container_port>` in its own
  network and reaches `127.0.0.1:<host_port>` on the host. Nothing else about the job's network changes.

What a job cannot say: a layer, mount or host port on a host isolated job (host sockets join the list when built)
(`ISOLATION_HOST` runs on the machine itself and already refuses mounts and files); a target in the runtime's own places
(`/proc`, `/dev`, `/sys`, `/run/js-files`, `/scratch`, `/root`, `/tmp`, `/var/tmp`, `/run`, and anything below them),
which 2.5 checks at submit and `JobStartSpec::validate` (section 3) checks again on the agent; any value of an enum, or
a source, that the server does not know (2.5).

### 2.4 How the interface grows, and what is designed now

**The rule.** Every addition is one of: a new member of a `oneof` (a source of a mount or a layer), a new value of an
enum (a mode, a propagation, a protocol), a new field with a default that keeps today's meaning (an option), or a new
repeated field of `JobSpec`. None changes a shipped number, name or meaning (B8). Each comes with four things:

1. **A server that does not know it refuses it, by name.** A mount whose only source an older server does not know
   arrives with no source, and is refused `INVALID_ARGUMENT` "mounts[i]: no source this server knows"; an enum value
   it does not know is refused the same way (2.5). Never a silent different answer.
2. **An option or a repeated field of `JobSpec` an older server does not know is dropped**, as any unknown field is
   (9); rule 1 cannot refuse what the server never sees. A client that sets one checks that the job the submit answers
   with (`Job.spec`) still has it, and cancels the job if not, as it checks the fields of this design today (9).
3. **A detected capability of its own** (`js_caps::DETECTED_KEYS`), reported by an agent that can do it, and inserted
   by `validate.rs` into the job's constraints when a job uses it, as `layered-rootfs` is (2.6). No agent that cannot do
   it is sent it.
4. **No catch-all on the agent wire** (3): an agent that meets a kind or value it does not know fails the decode of
   that one start, never runs it without. An option field it does not know it would ignore, as every body of the agent
   wire ignores unknown fields; rule 3's capability is what keeps such a start from it.

**Designed now, built later.** Each is fixed here so that a client can plan on it and no number is taken twice; none is
in the proto file, the wire or the agent until its build item (section 11). Until then a server refuses it as rule 1
says.

```proto
// Designed: Mount.source = 2. An empty tmpfs, private to the job, removed with it.
message TmpfsSource {
  // The most bytes it holds: 1 to the job's Needs.memory_bytes, and with the job's other tmpfs
  // mounts no more than it. Its pages are the job's
  // memory, counted against the job's memory as any other page it holds.
  uint64 size_bytes = 1;
}

// Designed: Mount.source = 3. An image's tree at a path: the flattened tree of Layer.image.
message ImageSource {
  // An image from a registry the host allows, pinned by digest, as Layer.image.
  string image = 1;
}

// Designed: JobSpec.host_sockets = 33. A Unix socket on the host that the job connects to
// at a path of its own file system.
message HostSocket {
  // An absolute path of a Unix socket on the host, in normal form. The host's operator lists
  // the prefixes a job may name (--host-socket).
  string host_path = 1;
  // Where the job connects: an absolute path in the job's file system, with the rules of
  // Mount.target.
  string target = 2;
}

// Designed: Mount.id_mapping = 15. The source's files, owned on the host by `host_uid` and
// `host_gid`, appear owned by the job's own user, so the job may write over a tree it did not
// make. Absent: no mapping, as today (LR5).
message IdMapping {
  // The owner of the source's files on the host. The host's operator lists the ids a job may
  // map (--map-id); any other is refused.
  uint32 host_uid = 1;
  uint32 host_gid = 2;
}

// Designed: PortProtocol value 2, PORT_PROTOCOL_UDP: datagrams to host_port on the host's
// loopback, and the answers back, through a port of the job's loopback.
```

| Addition | Meaning and limits | Capability | Linux | macOS |
|---|---|---|---|---|
| `Mount.tmpfs` | mode must be `READ_WRITE`, `sub_path` empty (an empty tree has no sub-directory); `size_bytes` 1 to the job's memory need, and the sum over the job's tmpfs mounts no more than it; owned by the job's user, mode 0700 | `mount-tmpfs` | a `tmpfs` entry of `crun`'s spec, `size=`, owned by the container's uid 1000, `nosuid,nodev` and `noexec` when asked | refused `mount-source-unsupported-here`: no per-run memory file system |
| `Mount.image` | the image's flattened tree, as an image layer is (LR4); mode `READ_ONLY` or `COPY_ON_WRITE` (the store's tree is shared, so `READ_WRITE` is refused at submit); `sub_path` allowed | `mount-image` | the tree in the image store, pinned for the lease, bound as a host path's clone is (4.4) | refused `mount-source-unsupported-here` until the Mac has an image store |
| `PORT_PROTOCOL_UDP` | a host port by datagram; unique per (protocol, job port); `--host-port` stays TCP's, and UDP gets an entry form of its own (`udp/PORT`), so an operator who allowed a TCP port has not allowed UDP | `host-port-udp` | a UDP socket the agent holds in the job's namespace, relaying each peer's datagrams to the host port and back (4.5) | a Seatbelt `network*` grant of the port, as TCP |
| `host_sockets` | at most 16; `host_path` must be a socket beneath an operator's `--host-socket` prefix (after links); a path socket, not an abstract one; `target` as a mount's, at most 107 bytes (a socket address's path), and in one namespace with the mount targets: no two equal, and none below a read-only mount that does not hold it | `host-sockets` | 4.5: the socket resolved and cloned as a host path is, but a socket instead of a directory, then bound at the target | a link at the target in `exec/`, and a Seatbelt grant to connect to that socket (5.4) |
| `Mount.id_mapping` | `host_uid`/`host_gid` mapped to the job's user (uid and gid 1000 in the container); the ids are bounded by an operator's `--map-id UID:GID`, repeatable; any source, every mode | `mount-idmap` | the clone of 4.2 step 3 made an id-mapped mount (`mount_setattr` with `MOUNT_ATTR_IDMAP` and a user namespace that maps `host_uid` to the job's container identity), done before it is attached; reported only where the kernel and the source's file system support id-mapped mounts (FUSE from 6.12; overlay lowers per kernel) | refused `mount-idmap-unsupported-here`: Seatbelt has no ownership mapping |
| `HostToContainer` propagation | a later `MOUNT_PROPAGATION_RECEIVE`: mounts the host makes below the source after the start appear in the job; never the other way | `mount-propagation-receive` | the clone made a slave of its source instead of private | refused: a link has no propagation |

A host socket is a message of its own rather than a third kind of `Mount.source`, though Docker and Kubernetes say it
with a bind and `hostPath`: what it gives a job is a host service, as a host port does, not a tree; none of a mount's
modes or options applies to it; its allow-list is a separate flag, so that an operator who lets jobs read a tree does
not thereby let them talk to a service listening in it; and on macOS it is a network grant, not a file grant (decision
LR20).

### 2.5 Validation at submit

`crates/js-lease-server/src/job_api/validate.rs` checks the shape only; it cannot see a host's allow-lists (2.6). A
fault answers `INVALID_ARGUMENT` naming the field, as every other fault of a job does (job.proto: only four faults carry
a `RefusalCode`), so no refusal code is added:

| Field | Rule |
|---|---|
| `image` | as today when set; may be empty only when `layers` is not |
| `command` | with an empty `image`: an absolute path |
| `layers` | at most 16; each has exactly one source |
| `layers[i].host_path`, `mounts[i].host_path` | absolute; normal form (no `.`, `..` or empty part, no trailing `/`); at most 4096 bytes; no NUL or control character |
| `layers[i].image` | `ImageDigestRef` as `image` |
| `mounts` | at most 64 |
| `mounts[i].source` | exactly one source this server knows; a mount with none ("no source this server knows") is refused |
| `mounts[i].target` | absolute, normal form, not `/`, not a runtime place of 2.3, ASCII, no `:` or `,`, at most 4096 bytes; no two equal (compared in lower case, for macOS) |
| `mounts[i].mode` | a known value; unspecified reads as read-only |
| `mounts[i].sub_path` | empty, or relative, in normal form, at most 4096 bytes, no NUL or control character |
| `mounts[i].propagation` | a known value; unspecified reads as private |
| `host_ports` | at most 16 |
| `host_ports[i].host_port` | 1 to 65535 |
| `host_ports[i].container_port` | absent, or 1 to 65535; with `host_port` when absent, no two entries of one protocol have the same job port, and none equals the `container_port` of one of the job's `ports` (that port of the job is its own listener's) |
| `host_ports[i].protocol` | a known value; unspecified reads as TCP |
| `constraints.isolation` | not `HOST` with any of `layers`, `mounts`, `host_ports` |
| `constraints.exec` | not `SANDBOX_EXEC` with `layers`, nor with a host port whose job port differs from its host port; left unspecified, the server sets `CONTAINER` (2.6) |

The rules for a target and a `sub_path` reuse the agent wire's relative-path rules (`rel_path` and `disjoint`,
`crates/js-agent-wire/src/run.rs`), after the leading `/` for a target, moved to `js-types` so that the server and the
agent check the same text with the same code. Two host ports may name the same `host_port` at two job ports; nothing is
gained by refusing it.

The three fields are part of "the same job" for an idempotent resubmit: `js_jobs::JobSpec` gains them and `same_job_as`
(`crates/js-jobs/src/record.rs`), which names every field, compares them, options and job ports included, an absent
`container_port` equal to one given as the same number as `host_port`, and an unspecified mode, propagation or protocol
equal to its default value given explicitly: the server stores each in its explicit form, so a resubmit that spells a
default either way is the same job. The stored row gains three tags after the highest in use
(`crates/js-jobs/src/codec.rs` and a `rootfs_fields` module beside the others), one for the start failure detail
and one for a result's `input_failure` (section 8). The row of a job with an empty `image` holds no image tag, which
every earlier server reads as a field every row holds: such a row is written with the format byte 2, which an earlier
server reads as a later build's row and leaves out of its reads, as it leaves any such row, rather than fail its
listings on a row it takes for corrupt.

### 2.6 Placement

An agent of a previous release ignores fields of a start it does not know (`JobStartSpec` has no `deny_unknown_fields`),
so it would run a job with layers on its image alone. Placement must never send such a job there:

- **A detected capability `layered-rootfs=true`**, reported by an agent that reads the three start fields, with every
  option of 2.2, and refuses by name what its runtime cannot do. It goes into `js_caps::DETECTED_KEYS` (an operator
  cannot set it) and the exact keys, and `validate.rs` inserts it into the job's constraints when any of the three is
  set, as it inserts `isa_level` today. `start_of` (`crates/js-lease-server/src/job_start.rs`) refuses to build such a
  start for a host whose hello lacks it (`Unsendable::RootfsNotReported`, new), through a `layered_rootfs: bool` in
  `BeatContext` filled as `host_jobs` is. Each later addition of 2.4 brings its own key, inserted and checked the same
  way.
- **A job with `layers`, or with a host port whose job port differs from its host port, also requires
  `exec=container`**, set by `validate.rs` when the job leaves `exec` unspecified (an explicit `SANDBOX_EXEC` is
  refused, 2.5): a root of layers exists only in a container, and a job port of its own needs a network of its own,
  which a Mac's native job does not have (section 5). A Mac never gets such a start.
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
/// Trees at paths in the job's file system.
pub mounts: Vec<JobMount>,
/// Ports on the host's loopback the job reaches at ports of its own loopback.
pub host_ports: Vec<HostPort>,
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

/// A tree at a path in the job's file system. Unknown fields are ignored, as in every body
/// of the agent wire; placement keeps an option from an agent that does not report it.
pub struct JobMount {
    /// Where the tree comes from.
    pub source: MountSource,
    /// An absolute path in the job's file system.
    pub target: String,
    /// Read-only (the default, left out), read-write or copy-on-write.
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: MountMode,
    /// A relative path in the source. Absent: the source's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    /// Nothing in the mount may be run. Left out when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_exec: bool,
    /// Private (the default, left out).
    #[serde(default, skip_serializing_if = "is_default")]
    pub propagation: Propagation,
}

/// Where a mount's tree comes from. No catch-all variant, as `Layer` has none; the
/// designed sources of 2.4 are added as `Tmpfs { size_bytes }` and `Image { image }`.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MountSource {
    /// A directory on the host, by absolute path.
    HostPath { path: String },
}

/// No catch-all variant, as `Placement` has none.
#[serde(rename_all = "snake_case")]
pub enum MountMode { #[default] ReadOnly, ReadWrite, CopyOnWrite }

/// No catch-all variant. A later `Receive` is added with its build item.
#[serde(rename_all = "snake_case")]
pub enum Propagation { #[default] Private }

/// A port on the host's loopback the job reaches at a port of its own loopback.
pub struct HostPort {
    /// The port on the host's loopback.
    pub host_port: u16,
    /// The port on the job's loopback. Absent: `host_port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_port: Option<u16>,
    /// TCP (the default, left out).
    #[serde(default, skip_serializing_if = "is_default")]
    pub protocol: PortProtocol,
}

/// No catch-all variant. A later `Udp` is added with its build item.
#[serde(rename_all = "snake_case")]
pub enum PortProtocol { #[default] Tcp }
```

`JobMount` is not `Mount` (`crates/js-agent-wire/src/command.rs`, a named root and a relative path, the legacy job
form's) and does not replace it. `JobStartSpec::validate` applies the rules of 2.5, and `image` may be empty when
`layers` is not (today it refuses an empty image `invalid-job`). The limits are constants of `limits.rs`: `MAX_LAYERS`
16, the existing `MAX_MOUNTS` 64 and `MAX_PORTS` 16. A field left out when it holds its default keeps the text of a
start that uses no option as small as it can be, and lets an option added later be absent from every start that does not
use it, so an agent that does not know the option still reads such a start.

`start_of` copies the three fields from the job: an unspecified mode, propagation or protocol becomes the default
variant, and a `container_port` equal to `host_port` is sent as given. A job without the fields gets a start without
them, byte for byte as today.

New agent refusal codes, added to `refusal.rs` (add-only, `refusal_codes_snapshot`):

| Code | When |
|---|---|
| `input-not-allowed` | a host path lies under no `--mount-root`/`--mount-root-ro` (after links are followed), a read-write mount lies under a read-only root, a `sub_path` leads out of its source by a link, or a host port is not in `--host-port` |
| `input-unavailable` | a host path, or its `sub_path`, is missing, not a directory, cannot be read, or its lookup passed its bound (4.7); a mount's source the job cannot reach (4.2) |
| `rootfs-unsupported-here` | `layers` on an agent whose runtime has no root to replace (macOS) |
| `mount-target-unsupported-here` | on macOS, a mount target outside `/work`, or a read-only or read-write mount at `/work` itself (5.1) |
| `host-port-remap-unsupported-here` | on macOS, a host port whose job port differs from its host port (5.1); placement never sends one there (2.6) |
| `input-unreadable` | as a result's `input_failure`: a host path no longer answers after the job, which did not succeed (section 8) |
| `host-port-lost` | as a result's `input_failure`: a host port could not be offered again after the agent restarted (4.5) |

An image layer outside every `--image-allow` entry is refused `image-not-allowed`, the existing code. The designed
additions of 2.4 bring two more codes, `mount-source-unsupported-here` and `mount-idmap-unsupported-here`, with their
build items; neither is added before.

## 4. The agent on Linux (`crun`)

### 4.1 Configuration

All flags, in the agent's flags file (B14, S2):

| Flag | Meaning |
|---|---|
| `--mount-root NAME=PATH`, `--mount-root-ro NAME=PATH` | **existing**, now accepted with `--runtime crun`. Each root is a prefix: a job may name any directory beneath it (after links are followed), read-only beneath either kind, read-write only beneath a `--mount-root`. The names stay for the legacy `Mount` form and for the log; a host path never names a root. |
| `--image-allow HOST[:PORT]/PREFIX` | **existing**; an image layer is allowed as the job's image is. |
| `--image-allow-loopback 127.0.0.1:PORT/PREFIX` (or `[::1]:PORT/PREFIX`) | **new**: a registry on the host's own loopback, over plain HTTP, with its port given. It builds `Allow::plain_http_loopback()`; `--registry-credentials` may name it as any other entry. |
| `--host-port PORT` or `LO-HI` | **new**, repeatable, at most 16: the host loopback ports a job may reach (a `HostPort.host_port`; the job's own port is not bounded, since it is in the job's network alone). |
| `--host-socket PREFIX` | **designed (2.4)**, added with host sockets: an absolute directory beneath which a job may name a Unix socket, after links are followed. Repeatable. |

Why the mount roots and not a new flag: they already are the operator's list of host directories jobs may use, with its
canonicalisation, its escape checks and its tests; a second list of prefixes beside them would be a second way to say
the same thing (decision LR1). What kept them off `crun` was the race of section 1, which 4.2 closes.

### 4.2 Resolving a host path, without a race

Each host path of a start (a layer's, or a mount's with its `sub_path`) is resolved once, by the agent, which is root
on a `crun` host:

1. Find the longest root that is a prefix of the path's text. None: `input-not-allowed`.
2. From the root's directory (held open since the agent started), open the rest of the path with `openat2`,
   `O_PATH | O_DIRECTORY` and `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`. `RESOLVE_NO_XDEV` is not given: a root may
   hold another file system below it (a lazy FUSE directory, say, mounted below its root). A link that leads out of the
   root fails here, at the one moment that counts (`input-not-allowed`); a missing path or a non-directory is
   `input-unavailable`. A mount with a `sub_path` then opens it the same way from the descriptor just opened, so a link
   in the `sub_path` may not lead out of the host path (`input-not-allowed`), and a missing or non-directory one is
   `input-unavailable`.
3. Clone what was opened into a detached mount of its own with `open_tree(fd, OPEN_TREE_CLONE | AT_RECURSIVE)`. While
   it is still detached, set it with one `mount_setattr(fd, "", AT_EMPTY_PATH | AT_RECURSIVE, ...)`: propagation
   `MS_PRIVATE`, and `MOUNT_ATTR_RDONLY | NOSUID | NODEV` (without `RDONLY` for a read-write or copy-on-write mount's
   source, which the overlay keeps unchanged anyway; with `NOEXEC` for a mount with `no_exec`). Then attach it
   with `move_mount` at a fixed path, `<mounts>/<lease>/src/<i>` (4.3 says where `<mounts>` is). `<i>` is one count
   over the start's host paths, its layers' first in spec order and then its mounts', so a layer and a mount never
   share one; 4.3 names a layer's clone `src/<i>` and 4.4 a mount's by the same count.

The clone must be private before it is attached. A clone of a shared mount joins its peer group: without the change, an
unmount of a submount of the clone would propagate to the provider's own tree, and a mount the provider makes later
would appear in the clone.

From then on, nothing reads the job's path text again: the overlay's `lowerdir` and `crun`'s binds name
`<mounts>/<lease>/src/<i>`, a directory of root's that no job can write. A swap of the original path after step 2
changes nothing, and the path text never reaches a mount option, so the rule that a host path hold no `,` or `:`
(`carries`) is not needed for these paths. `openat2` needs Linux 5.6, `open_tree`/`move_mount` 5.2 and `mount_setattr`
5.12; the hosts this is built for run 6.8 and later. `rustix` wraps `openat2`, `open_tree` and `move_mount` safely; it
has no `mount_setattr`, which goes in the crate's platform bindings as one system call with a `// SAFETY:` comment (R2),
unless a safe wrapper exists by then.

`crun` runs as the job user and binds the sources of the mounts itself, in the job's user namespace, where the job user's
own ids are not mapped and the job's are: `<mounts>` and every directory below it are mode 0711, owned by root, so any
id passes them. For each mount source, at assembly, the agent checks with `faccessat` under the job's own ids (the host
ids `sub_uid + 1000`, `Oci::container_ids`) that the clone's root can be searched; a source the job cannot reach is
`input-unavailable` at the start, rather than a `crun` error or a mount the job cannot use. The job user's ids would be
the wrong test: a source of mode 0700 owned by the job's identity, as a provider presents a writable tree (LR5), is one
`crun` binds and the job uses, and the job user cannot search it. Layers need no such check, since the agent mounts
them.

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
  the bottom lower: its tree in the image store, `oci-images/<hex>/rootfs`, pinned for the lease as today. An image
  layer is pulled and unpacked into the same store, pinned the same way, and is a lower as it is: two image layers stack
  as two whole trees, so a file one deletes in its own layers is deleted in its tree but not in the tree below it
  (decision LR4).
- The upper and work directories are on the scratch volume, in the lease directory, under the run's project quota as
  `<lease>/upper` is today. The scratch volume is local (XFS, for the quota), which overlayfs requires of an upper; a
  lower may be any file system the kernel can stack, FUSE among them, provided its mount lets other users read it
  (`allow_other`): the agent reads as root and the job as its own ids.
- The mount is made in the agent's mount namespace, as root, so `userxattr` is not given: whiteouts in the upper are the
  kernel's character devices. `crun` then puts the job in its user namespace over a root it did not make, which is how
  a daemon-made overlay and a mapped container work together.
- The upper's root directory is made owned by the job's identity in the container (uid and gid 1000) and mode 0755, so
  the job can make files at the top of its root: `kbf-task` of a tree job makes `/.kbf-run` there, and with a root-owned
  `/` that `mkdir` failed with `EACCES`. The targets the agent makes in the root stay owned by the container's root.
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
  (`crates/js-contain/src/container.rs`) with `source` `<mounts>/<lease>/src/<i>`, bound by `crun`
  `rbind,ro|rw,nosuid,nodev` (and `noexec` with `no_exec`) as the existing mounts are
  (`crates/js-contain/src/oci/spec.rs`, `mounts`). A read-only clone is read-only at step 3 of 4.2 already, recursively;
  `crun`'s `ro` is the second lock. `HostMount` gains the `noexec` flag; its other options are the ones it has.
- **`COPY_ON_WRITE`** is an overlay of its own, made by the agent as in 4.3 with one lower (`<mounts>/<lease>/src/<i>`)
  and its own upper and work directories (`<lease>/cow/<i>/{upper,work}`), mounted at `<mounts>/<lease>/cow/<i>` and
  bound by `crun` read-write at the target, with `noexec` when the mount has `no_exec` (the overlay's own mount carries
  `nosuid,nodev` and `noexec` too). A copy-on-write mount at `/work` takes the place of today's `/work` overlay, which
  `crun` then does not make.
- **Propagation** is private for every mount, by step 3 of 4.2 and by `<mounts>` being private (4.3): what the source
  has mounted below it at the clone is in the job, and nothing the host mounts or unmounts later is.
- Targets are made in the root before `crun` binds them: in the root's upper, by the agent, owned as the upper is, so a
  target exists whatever the layers hold there. A target under another mount is made after that mount, in its upper
  when it is copy-on-write; a target under a read-only mount must exist in that mount, or the start fails
  `invalid-mount` (the existing code: it is a fault of the spec on this host's data, not the host's state).

What the job sees through a lower, compared with a tree in its scratch (the overlay's own semantics): reads, `mmap`,
`exec` work; a write copies a file up into the upper; a delete is a whiteout; renaming a directory that came from a
lower fails `EXDEV` (`redirect_dir=off`; GNU `mv` then copies), renaming a file works. Writing over a lower directory
needs the job to have write permission on it as the lower presents it: a host path whose files are owned by an id the
job's mapping does not hold reads as owned by 65534, which the job cannot write into. A provider of trees a job will
write over presents them owned by the job's container identity (the host uid `sub_uid + 1000`, `Oci::container_ids`;
for `tierstore fs`, its `fs.owner`, as `docs/runbooks/fuse-layers-and-mounts.md` says);
the agent does not change ownership (decision LR5), until the designed `id_mapping` option of 2.4 is built.

### 4.5 Host ports

The agent offers each host port to the job with a listener inside the job's network namespace and a connection out of
its own:

- After `crun` has made the container and before the job's program runs, the agent opens `/proc/<pid>/ns/net` (as it
  does for a wired job today, `crates/js-agent/src/egress/host.rs`, `attach`), and on a dedicated thread enters it with
  `setns`, binds and listens on `127.0.0.1:<container_port>`, and returns. A socket keeps the namespace it was made in,
  so the listener is in the job's namespace while the agent's thread is back in the host's.
- Each accepted connection is joined to a new connection to `127.0.0.1:<host_port>` in the host's namespace and the
  bytes are copied both ways (`tokio::io::copy_bidirectional`) on a runtime of its own (`js-ports`, two threads), so a
  busy upload cannot slow the lifecycle. A refused connection to the host closes the job's connection.
- The job's program must not run before the listeners exist. Its init already waits for a route on a wired job
  (`--wait-route`); it gains `--wait-listen SECONDS PORT,...` (the bundle gives 30), which waits until each job port
  is in state `LISTEN` in the job's own `/proc/net/tcp`, under the same 30 s bound, so no connection is made to find
  out. Each listener is bound with `IP_FREEBIND`, so a bind made before `crun` has brought the job's loopback up
  still answers once it is.
- It works at every network level, because it needs no interface and no firewall rule: the `input` refusal of the host
  stays whole. It is TCP over IPv4, as the forwards of `ports` are; a designed UDP port (2.4) is a UDP socket the agent
  holds the same way, relaying each datagram and its answer, with no listener to wait for.
- The listeners are released with the run (`Wired::release`'s place in `remove`). After an agent restart, an adopted
  run's listeners are made again; if a port cannot be bound again (the job has bound it itself meanwhile), the run is
  stopped and its result carries `input_failure: host-port-lost`.

Why not a firewall exception: reaching a service on the host's `127.0.0.1` from a veth needs `route_localnet` on that
interface and a DNAT, both of which loosen the rules that keep a job from its host, and a `none` job has no interface
at all. Why not a process in the namespace: `agent-egress.md` rejected a per-run holder process; a socket held by the
agent is not one (decision LR8).

**Host sockets (designed, 2.4).** A host socket is resolved as a host path is (4.2), beneath its `--host-socket`
prefix, except that what is opened must be a socket (`O_PATH` without `O_DIRECTORY`, then `fstat`: anything else is
`input-unavailable`). The clone of that one file is attached at `<mounts>/<lease>/sock/<i>`, and `crun` binds it at the
target, which the agent makes as an empty file in the root's upper. A path socket is reached through its inode, not
through the network, so a job connects to it at every network level, `none` included; the job's user needs write
permission on the socket as its mapping presents it, so a provider makes the socket writable by others or owned by the
job's container identity (LR5). Nothing is copied by the agent: the job's connection is the provider's own.

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

**Restart.** Mounts in the host's namespace outlive the agent. An adopted run keeps them and removes them as above.
After adoption the agent unmounts and removes each `<mounts>/<lease>` of a run it did not keep, from its list file.
Image pins are unchanged.

**Rollback.** An agent of an earlier release knows nothing of `<mounts>`. It does not read or walk it, and it removes
the lease directories of the runs it does not adopt (the uppers under the overlays, which holds no host data). The
mounts of runs that lived through the rollback stay until the host reboots; they were there when that agent took its
integrity baseline, so they are no finding.

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

`begin` then runs as today, and finds the root and the sources made. Section 7.4 says why the lookups and mounts are
kept off the lifecycle worker, and what the bound means.

## 5. The agent on macOS

Not built now: the Macs are deferred, and how a Mac gives a job a writable tree over a lazy one is **not chosen here**.
It waits for measurements of a lazy provider's real macOS mount (13, Q1). This section fixes everything that does not
depend on that choice: what the Mac agent does with each field of the same `JobStartSpec`, the Seatbelt view, and the
three candidate mechanisms with the tests that will decide between them. The wire and the server of sections 2 and 3 do
not change whichever wins, because all three realise the same thing: a `COPY_ON_WRITE` mount.

### 5.1 What applies

| Field | macOS |
|---|---|
| `layers` | refused `rootfs-unsupported-here`; placement never sends them (2.6: they require `exec=container`). A native job runs in its own directory of the host's file system; there is no root for layers to replace, under any of the mechanisms of 5.3. |
| `mounts`, `COPY_ON_WRITE`, target `/work` or below it | realised in the run's `exec/` by the mechanism chosen from 5.3 |
| `mounts`, `READ_ONLY` or `READ_WRITE`, target below `/work` | a link in `exec/` (5.2) |
| `mounts`, `READ_ONLY` or `READ_WRITE`, target exactly `/work` | refused `mount-target-unsupported-here`: the run's working directory is a directory of the run's home, which a link cannot replace |
| `mounts`, any target outside `/work` | refused `mount-target-unsupported-here` |
| `mounts[i].sub_path` | resolved beneath the host path as on Linux; the link of 5.2 points at the result |
| `mounts[i].no_exec` | a Seatbelt deny of `process-exec*` and `file-map-executable` for the mount's path (5.4) |
| `mounts[i].propagation` | private is what a link gives: nothing mounted on the host later appears through it unless it is mounted below the linked path itself, which the provider controls |
| `host_ports`, job port equal to the host port | allowed in the Seatbelt profile (5.4) |
| `host_ports`, job port different from the host port | refused `host-port-remap-unsupported-here`: a native job shares the host's loopback, so it reaches a host port only at its own number; placement never sends one (2.6) |
| `host_sockets` (designed) | a link at the target in `exec/`, which must be below `/work` as a mount's is, and a Seatbelt grant to connect to the socket (5.4) |

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
| Cost per run | a mount and an unmount | proportional to the tree (a clone is a metadata operation and shares blocks; a copy reads every byte) | proportional to the number of entries (about 2 s measured for 46,362 files) |
| What it needs | a file system of ours with overlay semantics; an FSKit extension approved on every Mac (the MDM layer can approve it) or FUSE-T installed; root to mount; an unmount before `user-delete`, which refuses to cross a mount (`docs/design/mac-lease-teardown.md`, the deletion's step 1); a way to end processes blocked in a hung mount | the copy `files.rs` makes of a `Mount` input, changed to give owner-writable modes and to leave the tree out of the read-only list; nothing mounted | a list of the files the job writes, or an acceptance that writes to inputs fail |
| Seatbelt | a grant of the mount's path (it is in the home, which is writable already) | nothing new: the tree is in the home | read grants on the host path behind the links; the copies are in the home |

**The tests that decide.** Each candidate is run against a lazy provider's real macOS mount, on the same tree, and then:

1. *Semantics*: a program that opens an input read-write and changes it, truncates and appends to one, renames a file
   and a directory from the lower, deletes both, makes a hard link, changes a mode and `mmap`s a file for writing;
   every step must give what it gives on Linux's overlay (4.4), and the host path must be unchanged after.
2. *Laziness*: a tree of the size of a large build's inputs (46,362 files, as measured), of which the
   job reads 1 percent: time before the program's first instruction, bytes fetched by the provider, and the job's time.
3. *Cost*: the same tree read whole, to bound the worst case of each.
4. *Teardown*: the run's processes killed while some are blocked in a read the provider does not answer, then
   `user-delete`; nothing of the run may remain, and nothing may block the agent.
5. *Fleet*: what each needs installed and approved on a Mac, and whether the MDM layer can do it without a person.

Candidate (c) fails test 1 by construction unless the client declares its outputs; it is listed because it was proposed
and because it is the cheapest if the measurements show that the jobs only replace files by rename.

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
- **`no_exec`.** For a mount with it, `(deny process-exec* (subpath (param "GR_i")))` and
  `(deny file-map-executable (subpath (param "GR_i")))` after the grants, so nothing in it is run or mapped executable.
- **Host sockets (designed).** `(allow network-outbound (remote unix-socket (path-literal (param "HS_i"))))` per socket,
  after the narrowing as host ports are, and a read grant of the socket's path; the operator's `--host-socket` bounds
  them.
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
push to main that changes a file that ships, and #842 did, so these are in a release; B8 forbids removing or re-meaning
a shipped item. Nothing ever produces them: no server sends a tree input (`start_of` sends no `run`), no agent sets
`input_failure`, no agent reports the key. So each is retired or generalised as follows, in build item 1:

| Item | What happens | Why |
|---|---|---|
| `InputRef::Tree`, `TreeUnavailable` | **Kept, retired.** Decoded and validated as now; no server builds one; the doc comments say "retired: a tree named by digest, which no server sends", with no file system named. Every runtime refuses a start that holds one: `crun` already refuses every `run` (`run-unsupported`); the macOS runtime, which today passes over a tree input when it stages inputs, refuses it `native-unsupported` as it refuses a `Stage`. | Removing a variant of a shipped enum is not additive. A tree by digest names a namespace only one provider has; the generic input is a host path (sections 2 to 5), not a renamed tree. |
| `JobResult.input_failure` | **Generalised.** Same field, same type, same bound: "the code of an input the host provides that failed while the job ran". Its codes become `input-unreadable` and `host-port-lost` (section 8). | Its meaning, a failure of the host's input rather than of the job, is the one section 8 needs; no agent ever sent a value, so no reader meets a changed one. |
| `tree-unavailable`, `tree-invalid`, `tree-read-failed`, `tree-stalled`, `tree-placement-unsupported`, `tree-unsupported-here` | **Kept** (refusal codes are add-only), documented as retired; no agent sends them. | `refusal_codes_snapshot` pins the list. |
| `outputs-unsupported-here`, `too-many-inputs-here`, `cwd-in-tree` | **Kept and used**: the last two by 5.2 and 5.4; the first stays for a `run` with outputs. Their docs lose the word tree where it named a provider's tree. | They describe the agent's limits, not a provider. |
| `js_caps::TIERSTORE_TREES` (`tierstore-trees`) | **Removed** from `js-caps` (the constant, `DETECTED_KEYS`, the exact keys, the docs) and its tests. | It is the one item whose text names a provider. No agent of any release reports it, and a client cannot name a capability (the public `Constraints` has no free map). A server of an earlier release that knows the key meets no host reporting it. After the removal, two things change, and neither matters: a request that names the key is refused as an unknown key, where before it could never be met; and an operator's `--capability tierstore-trees=...` becomes an ordinary operator capability that nothing reads, where before it was refused at start. |
| `docs/design/agent-wire.md` | Its list of types without a catch-all keeps `TreeUnavailable`; the pointer to tree-inputs.md 2.2 goes. (`Layer`, `MountSource`, `MountMode`, `Propagation` and `PortProtocol` join the list in build item 2.) | |
| `docs/design/tree-inputs.md` | Marked superseded by this document at its head; kept, as `jobscheduler-design.md` 0.3 keeps every older document. | |

The tests that pin the tree wire stay and keep asserting what they assert:
`a_tree_start_reads_as_unknown_to_the_previous_release` (a release before #842 still reads such a start as unknown),
`tree_input_wire_form` (the retired form still has the same text), `an_old_server_ignores_input_failure`. The tests of
`tierstore_keys.rs` and `flag_cannot_claim_tierstore_trees` go with the key; that is the removal of a key, not a
weakened assertion over a gap (T7), and the PR says so.

**Migration note.** No deployment uses tree inputs. A client that planned to send `JobSpec.trees` (tree-inputs.md 2.4,
never built) sends instead a `COPY_ON_WRITE` mount of the provider's directory for the tree at `/work`, or a `READ_ONLY`
mount at `/work/<dest>`; it asks its provider to fetch ahead when the job is placed (section 7); and it reads
`START_FAILURE_CODE_INPUT_UNAVAILABLE` and `RESULT_REASON_INPUT_FAILED` where tree-inputs.md had `tree-unavailable` and
`INPUT_FAILED`. The fallback `TreeUnavailable::JobStages` has no successor: a client that wants to stage inputs itself
when the host path is missing submits a job without the mount.

## 7. The lifecycle a client can rely on

This section is the hook a client uses to prepare anything of its own on the job's host (warm a node-local store, start
the service behind a host port, check a path exists): what it can observe, when, and what each state promises about the
job's inputs. It names no client, and it adds no state, field or call: it states what the API already gives, so that a
client can depend on it (decision LR12).

### 7.1 The states and when each is observable

The public API has no stream: a client reads a job with `GetJob` or `ListJobs`, and sees the state the server has
recorded at that moment (`crates/js-client/src/jobs/loops.rs`, `wait_for_terminal`, polls). The server records what a
host's agent reports on its heartbeat (`report`, `crates/js-lease-server/src/per_job.rs`), so a state is observable from
the heartbeat that carries it, not from the moment the agent reached it.

| Phase | `JobState` | Observable from | What the host has done with the job's inputs | Set on the job |
|---|---|---|---|---|
| queued | `QUEUED` | the submit's answer | nothing: no host is chosen | `times.queued_at_ms` |
| creating | `PLACED` | the commit of the placement | nothing yet: the start leaves on the host's next heartbeat reply (a whole-host job's, once its host is quiet and ready) | `host_id`; `times.placed_at_ms`; an event `PLACED` naming the host |
| creating | `STARTING` | the first heartbeat on which the host lists the run as starting | preparing (4.7): pulling the image and image layers, resolving each host path (4.2), making the clones, overlays and targets; a host path's root is opened here, and the overlay's mount looks up each lower's root | as `PLACED` |
| running | `RUNNING` | the first heartbeat on which the host lists the run as running | the job's program runs; every read of a lower or a mount from here on is the job's own, made when the job makes it | `times.started_at_ms`; the wall limit counts from here |
| ended | `SUCCEEDED`, `FAILED`, `CANCELLED`, `LOST`, `FAILED_TO_START` | the heartbeat that carries the result, or the server's own end (a cancel before a start, the start timeout, the queue's limit, a lost host) | for `SUCCEEDED` and `FAILED`: the job's processes have exited; the host removes its mounts as it removes the run (4.6) | `times.ended_at_ms`; the result; for `FAILED_TO_START`, `start_failure` and `start_failure_detail` |

"Creating" (or "preparing") is `PLACED` and `STARTING` together: the time from the moment the host is known to the
moment the program runs. `times.started_at_ms - times.placed_at_ms` is how long it took.

### 7.2 What a client can rely on

1. **States move forward**: from `QUEUED` to `PLACED` by the placement's commit, then along `STARTING` and `RUNNING`
   (the server's `may_move`, `crates/js-jobs/src/record.rs`), then one end, which never changes. The one way back, an
   operation of the store's own outside `may_move` (`requeue_failed_prepare`, `crates/js-jobs/src/store.rs`): a
   whole-host job whose host's prepare failed returns from `PLACED` to `QUEUED`, its `host_id` cleared, with an event
   `REQUEUED` naming the host it left; it may then be placed on another host.
2. **A read may miss a state.** Every state the job passes is recorded, one commit each, but a read sees only the
   state of its moment: a job may go from `PLACED` to `RUNNING`, or to an end, between two reads, and from `QUEUED` to
   an end without being placed (no capacity, a cancel). A client acts on the first read that shows a state at or past
   the one it waits for, never on seeing each one. `times` and `events` say what it missed.
3. **The host is known from `PLACED` on** (`host_id`), and is the host that will read the job's inputs, unless rule 1's
   requeue moves it. Nothing on any host touches a job's inputs before that host has the job's start, which is after
   the placement is recorded.
4. **The inputs are first touched in the creating phase, on that host.** The agent may begin resolving host paths as
   soon as it has the start, which can be before a read shows `PLACED`, since the start may leave on the heartbeat
   right after the placement's commit. So the hook of 7.3 can warm or fetch ahead, but it cannot be relied on to make a
   path exist in time: a host path whose root must exist exists before the submit, or the start fails
   `input-unavailable` (section 8). Beyond the roots it opens, the agent reads nothing of a host path: every other
   read is the job's, during `RUNNING`.
5. **Creating is bounded.** The pool's start timeout (900 s by default, `crates/js-lease-server/src/job_expiry.rs`)
   counts from placement; a job not `RUNNING` by then ends `FAILED_TO_START` with `START_TIMEOUT`. A whole-host job is
   bounded by its host's prepare deadline while `PLACED`, and by the start timeout counted from that deadline once
   `STARTING`. Inside it, each host
   path's lookup is bounded by 60 s (4.7).
6. **An ended job no longer reads its inputs** after `SUCCEEDED` or `FAILED`, which come only from its host's
   result: the host reports a result only after the job's processes have exited. `CANCELLED` may be the server's own
   end of a job its host had not yet started, or the host's result; a client that must know reads `GetResult`, which
   holds the host's result only in the second case.
   After `LOST` nothing is known of the host, and a client that changes a tree a lost job used does so knowing that the
   job's processes may, on a host that is cut off, still run.

### 7.3 The generic hook

A client that prepares something on the job's host does it at the first read that shows a `host_id` (rule 3), which is
`PLACED` or, if the read came late, a later state (rule 2), and treats what it does as advice, never as a condition of
the job's correctness: the job may still fail to start, be cancelled, or (rule 1) move to another host, on which the
client prepares again. If the preparation is late, or fails, the job still reads every file it needs, on first read,
through whatever file system is there. A client that wants the moment sooner reads more often while the job is `QUEUED`
or `PLACED`; this design adds no watch (decision LR12).

### 7.4 Where the time goes

**No special fetch.** The agent never asks a provider for anything. A host path is a directory; the agent opens its
root (4.2) and, at the overlay's mount, the kernel looks up the lowers' roots; every other read is the job's, when the
job makes it, through the kernel, to whatever file system is there. On a node with a lazy file system, that file system
fetches on first read; jobscheduler neither knows nor needs to.

| Time | Counted as | Bounded by |
|---|---|---|
| image and image-layer pulls | the creating phase (`STARTING`) | the prefetcher's per-pull limit (30 min) and the server's start timeout (900 s from placement; for a whole-host job, from its prepare deadline) |
| a host path's root lookup | the creating phase | 60 s per path (4.7), then the start timeout |
| the clones and overlay mounts | the creating phase | the start timeout |
| reads of a lazy lower while the job runs | the job's wall clock | the job's wall limit |
| on macOS, the work a candidate of 5.3 does before the program (a mount, a clone, links) | the creating phase, once the native runtime has a preparing stage (5.1) | the start timeout |

So on Linux a lazy input is paid for while the job runs, and a client sets the wall limit with that in mind. The time
a job spent creating is visible already: `Job.times.started_at_ms - Job.times.placed_at_ms` (`JobTimes`, job.proto),
which includes the heartbeat that carried the start. Nothing new is reported.

Why the lookups and mounts stay off the lifecycle worker: a lookup on a FUSE file system blocks in the kernel until
the file system answers, and one that has hung never returns; the worker serves every other job. The `js-paths` pool
has four threads and makes none on demand, so a hung provider holds at most four threads, and further lookups wait their
60 s and fail `input-unavailable`. A thread stuck in the kernel stays stuck until the file system answers or is
unmounted; that is the provider's to fix, and the host reports it only through failed starts.

## 8. Failures

Every failure ends in one of three places: the start fails (nothing ran), the job's result says an input failed (it ran,
and the host's input is to blame), or the job's own result stands.

| Failure | Learnt | Result |
|---|---|---|
| A host path under no root, a read-write mount under a read-only root, a host port not allowed | admission | start fails `input-not-allowed`; public `START_FAILURE_CODE_AGENT_REFUSED`, detail `input-not-allowed` |
| An image or image layer outside `--image-allow` | admission | start fails `image-not-allowed`, as today |
| On macOS, a host port whose job port differs from its host port, which placement never sends (2.6) | admission | start fails `host-port-remap-unsupported-here`; public `AGENT_REFUSED` |
| A source, enum value, option or repeated field a server does not know (a later addition of 2.4, sent to an earlier server) | submit | `INVALID_ARGUMENT` naming the field, for a source or a value; an unknown option or repeated field (`host_sockets`) is dropped, which the client sees in `Job.spec` (9) |
| A host path or its `sub_path` missing, not a directory, unreadable, or leading out of its root (or, for a `sub_path`, its host path) by a link | preparing | start fails `input-unavailable`, or `input-not-allowed` for the link; public `START_FAILURE_CODE_INPUT_UNAVAILABLE` / `AGENT_REFUSED` |
| A host path's lookup passes 60 s | preparing | start fails `input-unavailable` |
| An image layer's pull fails | preparing | as an image's pull fails today |
| An overlay or clone mount fails (the kernel refuses a lower, the upper's volume is not local) | preparing | start fails with the existing runtime failure code; it is the host's configuration |
| A read of a lazy lower fails while the job runs (`EIO`) | the job | the job's own result: the agent cannot tell one file's failed read from the job's |
| The job did not succeed, and after it ended a host path of its layers or mounts no longer answers its lookup (`ENOTCONN`, `EIO`, `ESTALE`, or no answer within 5 s) | after exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: input-unreadable`, exit code kept |
| A host port cannot be offered again after an agent restart | adoption | the run is stopped; `FAILED`, reason `INPUT_FAILED`, `input_failure: host-port-lost` |
| The host's service behind a host port refuses or drops a connection | the job | the job's own result |
| A program in a `no_exec` mount is run | the job | the job's own result (`EACCES`), as for any file it may not run |
| The job's writes fill its quota | the job | the job's own result (`ENOSPC`), as today |
| A target below a read-only mount that does not exist in that mount | assembly (preparing) | start fails `invalid-mount`, public `AGENT_REFUSED`: the spec's fault on this host's data |
| A mount source the job's own ids cannot search | assembly (preparing) | start fails `input-unavailable` |

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
  the enums `MountMode`, `MountPropagation`, `PortProtocol`, `START_FAILURE_CODE_INPUT_UNAVAILABLE`,
  `RESULT_REASON_INPUT_FAILED` and `JobResult.input_failure` (12) are additive. A client of an earlier release reads the
  new enum values as unspecified, as each enum's comment says. The dangerous direction is a new client against an
  earlier server: the server drops the unknown fields and runs the job without its layers. Servers upgrade before
  clients that use the fields (as before agents), and such a client checks that the job the submit answers with
  (`Job.spec`) lists them, and cancels it if not. The proto test of build item 3 shows the drop.
- **Growing.** Every later addition follows 2.4: a new source or enum value reaches an earlier server as nothing it
  knows and is refused by name; a new option or repeated field is dropped by an earlier server, which the same
  `Job.spec` check catches;
  a new capability keeps it from every agent that cannot do it. A client of an earlier release reading a job that uses
  a later source sees a `Mount` with no source, and one with a later enum value sees it as unspecified. The server
  stores and answers every mode, propagation and protocol in its explicit form, so an unspecified value in a job it
  returns can only be one this client does not know; each enum's comment tells the client to treat such a mount or port
  as one it cannot read, and not to resubmit it as read. A resubmit of a mount with no source is refused; a resubmit of
  any changed job under the same id is `job-id-conflict`.
  The designed additions take the numbers 2.2 and 2.4 give them, so no two releases give one number two meanings.
- **The stored row.** Five new tags; an earlier server keeps unknown tags through a rewrite (`Carried`), and a missing
  tag reads as empty. The row of a job without an image has the format byte 2 (2.5), which an earlier server leaves
  out as a later build's row.
- **Agent wire.** The three `JobStartSpec` fields are absent when empty. An agent of an earlier release ignores them,
  which is why placement and `start_of` require `layered-rootfs` (2.6). `Layer`, `MountSource`, `MountMode`,
  `Propagation` and `PortProtocol` have no catch-all: an agent that meets an unknown kind or value fails that one
  start's decode, as for `InputRef`. `JobMount` and `HostPort` ignore unknown fields, as every body of the agent wire
  does (`crates/js-agent-wire/src/lib.rs`); a later option reaches only agents that report its capability (2.4). An
  option at its default is left out of the text, so a start that uses no later option is the same text for every
  agent that reports `layered-rootfs`.
- **The agent's files.** The ledger sidecar does not change (4.6). The list of a run's mounts is a new file in a new
  directory, `<mounts>`, that no earlier release reads or sweeps; 4.6 says what a rollback leaves.
- **macOS.** A mount root becomes a read deny of every run (5.4). A job that read a tool under a mount root without
  mounting it no longer can; the operator keeps such tools outside the roots.
- **The tree wire** is retired additively (section 6); only the never-reported capability key is removed.
- **Kernel.** Linux 5.12 for `mount_setattr` (`openat2` needs 5.6, `open_tree` and `move_mount` 5.2); `layered-rootfs`
  is reported only when the agent's start-time check finds the four system calls (a `ENOSYS` means the capability is not
  reported, with a warning), so an older kernel runs jobs as before and never gets a layered one.

## 10. Tests

Each pull request of section 11 carries its own. Every test is hermetic (T12): host paths are directories in the test's
scratch directory; a slow or hung lazy file system is a fake opener behind the resolver's trait, released by the test
with an explicit event, never a real FUSE mount; every bound (60 s per lookup, 5 s after exit, 30 s for listeners) is
run on the agent's injected clock, never waited for (T10); a host port is a listener the test binds on `127.0.0.1`
port 0.

**Wire and server** (`js-agent-wire`, `js-proto`, `js-jobs`, `js-lease-server`, `js-sched-placer`, `js-caps`):

- goldens of a start with every new field and with each minimal form; validation of each rule of 2.5 at its boundary
  (16 and 17 layers, 64 and 65 mounts, 16 and 17 ports, a target of 4096 and 4097 bytes, `/`, each runtime place, a
  relative path, `..`, two targets equal in different case, a mount below another accepted, an empty image without
  layers refused and with layers accepted, a relative command with an empty image refused, a layer with no source and
  one with two, a host path not in normal form, with a NUL, and of 4096 and 4097 bytes, a port given twice, `HOST`
  isolation with each field, an unspecified mode stored and read as read-only; a mount with no source, as an earlier
  server meets a later one, refused "no source this server knows"; a `sub_path` absolute, with `..`, not in normal
  form, and of 4096 and 4097 bytes refused, an empty one and a relative one accepted; `no_exec` stored and read back;
  an unspecified propagation stored and read as private, and an unknown value refused; a host port of 0 and 65536
  refused, 1 and 65535 accepted, for `host_port` and for `container_port`; an absent `container_port` read as
  `host_port`; two host ports with one job port refused (given, and one given and one absent at the same number), and
  two job ports of one `host_port` accepted; a job port equal to a `ports[i].container_port` refused; an unspecified
  protocol read as TCP, and an unknown value refused; a target with a non-ASCII byte, with `:`, with `,`, and with a
  NUL refused; a `sub_path` with a NUL refused; a `layers[i].image` with a tag and no digest refused; an unknown mode
  value refused; `SANDBOX_EXEC` with layers, and with a job port of its own, refused, and an unspecified `exec` set to
  `CONTAINER`; a job returned by `GetJob` holds every mode, propagation and protocol in its explicit form);
- idempotent resubmit: the same job with the same layers, mounts and host ports is the same job; a change of any one,
  or of their order, or of any option or job port, is `job-id-conflict`; a default spelled out (`READ_ONLY`, `PRIVATE`,
  `TCP`, a `container_port` equal to `host_port`) and the same default left out are the same job;
- previous release: an agent of the previous release decodes a start with layers and drops them (the hazard 2.6 guards
  against, pinned); a start without the fields is the previous text byte for byte; an unknown `Layer` or `MountSource`
  kind, and an unknown `MountMode`, `Propagation` or `PortProtocol`, each read as an unknown command beside intact
  siblings; an unknown field of `JobMount` or `HostPort` ignored and the rest of the mount read; a mount and a host port
  with every option at its default have the text of a golden with none of them written; the previous proto envelope and
  the new one read each other (the `isolation.rs` pattern), and a previous server drops the fields (9);
- placement: a job with any field requires `layered-rootfs`, and one with layers, or with a host port whose job port
  differs from its host port, also `exec=container`, set when the client left it unspecified and refused when the
  client said `SANDBOX_EXEC`; one whose job ports all equal
  their host ports does not; placed on the reporting host of two and never on the other; `start_of` refuses a host
  without it (`RootfsNotReported`); a job without the fields starts as today;
- `end_of` maps `input-unavailable`, `input-not-allowed` and an unknown code, and a result with each `input_failure`
  and each outcome; the start failure detail round-trips through the stored row and `GetJob`;
- the tree wire's tests of section 6 pass unchanged; the removed key of section 6 is an unknown key to a request.
- flags: `--host-port` with 16 entries accepted and 17 refused, a range `LO-HI` accepted, a reversed range, a port 0
  and a port 65536 refused at start; `--mount-root` and `--mount-root-ro` accepted with `--runtime crun`;

**Agent, Linux** (`js-agent`, with fakes):

- `sub_path` (4.2): a `sub_path` below the host path accepted and the clone's root at it; a link in it to inside the
  host path accepted, and to outside the host path but inside the root `input-not-allowed`; a missing one and a file
  `input-unavailable`;
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
- assembly: a target below a read-only mount that does not exist in it refused `invalid-mount`; a mount source the job's
  own ids cannot search refused `input-unavailable`, and the search made as those ids, not the job user's; a failed overlay or clone mount from the fake failing the start with
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
- host ports: bytes both ways between a job-side client and a host-side listener, at the same port and at a job port
  that differs from the host port; the job's close and the host's close each close the other side; a port not in
  `--host-port` refused; a host side that refuses closes the job's connection; `--wait-listen` waits for the job ports'
  `LISTEN` and gives up at its bound; adoption re-makes a listener, and stops the run with `host-port-lost` when the
  port is taken.

The resolver, the forwarder's namespace step and the mounts are behind traits so the cases above run as the ordinary
user; the system calls themselves run in the root suites that exist.

**Root suites** (`crates/js-contain/tests/crun_root`, run by the existing `crun-root` job of `ci.yml`; no new job): a
container over a root of three lowers (an image's tree and two directories): it reads a file of each, sees the top one's
where two have the same name, writes a file and deletes a lower file and a lower directory (whiteouts in the upper), and
the lowers are unchanged after; a read-only mount refuses a write `EROFS`; a read-write mount's write reaches the host
directory; a read-write and a copy-on-write mount of a directory of mode 0700 owned by the job's own ids (LR5) are bound
and written; a copy-on-write mount at `/work` takes a write that does not reach the host directory; a program in a
`no_exec` mount fails to run `EACCES` and one in the same tree without it runs; a mount of a host path with a mount
below it shows that mount, and one the test makes on the host after the start does not appear (private); a read-only
mount of a source with a writable mount below it refuses a write below too (recursive); a set-uid program in a mount
runs without its set-id effect and a device node in one cannot be opened (`nosuid`, `nodev`), in a read-only, a
read-write and a copy-on-write mount; a copy-on-write mount with `no_exec` refuses to run a program; a mount with a
`sub_path` shows that directory and nothing beside it; a host port reaches a listener of the test's in the host's
namespace at level `none`; a clone of a shared source with a mount below it is unmounted, and the source's own mount
below it is still there; the lease directory is removed whole by `remove_tree` after the unmounts, and nothing of it
remains.

**Agent, macOS** (`js-mac-pool`, `js-agent`, in the macOS job that runs the workspace's tests), whichever candidate of
5.3 is chosen: the profile text of a run with a read-only and a read-write mount and a host port, asserted whole, at a
ceiling of `off` and of `loopback`, and with a `no_exec` mount; `layers` refused `rootfs-unsupported-here`; a host
port with a job port of its own refused `host-port-remap-unsupported-here`; a target outside `/work`, and a
read-only or read-write mount at `/work` itself, refused `mount-target-unsupported-here`; 64 listed entries accepted and
65 refused; a working directory below a read-only link refused `cwd-in-tree`; a start holding a tree input refused
`native-unsupported`; a `sandbox-exec` behaviour test in which a run reads its granted path, is denied the root's other
paths and a write to a read-only mount, and reaches a granted host port and not another. The chosen candidate brings
the semantics test of 5.3 (test 1) as a behaviour test of its own.

**Why this list is complete for the failure and boundary cases.** It takes each row of the failure table of section 8,
each rule of 2.5 at both sides of its bound, each refusal code of section 3 and each existing code this design gives a
new use (`image-not-allowed`, `invalid-mount`, `native-unsupported`, `cwd-in-tree`, `too-many-inputs-here`), each flag
of 4.1, each place a run ends (`remove`,
`remove_leftover`, `tear_down`, each end of the preparing stage, the adoption sweep) with mounts present, both
directions of each compatibility edge of section 9 (client and server, server and agent, previous release and this),
for the resolver each way a path can fail to be inside its root (no root, wrong kind, link out, missing, wrong type,
unreadable, swapped after the check) and a `sub_path` inside its host path, each option at its default and set, each
default spelled out and left out, each unknown kind, value and field an earlier reader can meet (2.4), and for each
bound one case inside and one past it. The designed additions of 2.4 bring their tests with their build items.

## 11. Build items

One subject per pull request, in this order; each names its tests from section 10.

1. **Retire the tree wire** (`js-agent-wire`, `js-caps`, `js-agent`): section 6, whole: the docs, the explicit macOS
   refusal of a tree input, the generalised `input_failure` doc, the removed key and its tests, `agent-wire.md`'s
   pointer.
2. **The agent wire** (`js-agent-wire`, `js-types`, `js-caps`): `Layer`, `JobMount` with `MountSource` (host path
   only), `MountMode`, `Propagation` (private only), `sub_path` and `no_exec`; `HostPort` with `host_port`,
   `container_port` and `PortProtocol` (TCP only); the three
   `JobStartSpec` fields, `image` optional with layers, the shared path rules moved to `js-types` (a `sub_path` checked
   by the same code as a target), the limits, the refusal codes of section 3 (`host-port-remap-unsupported-here`
   among them), the `layered-rootfs` key, `Layer`, `MountSource`, `MountMode`, `Propagation` and `PortProtocol` in
   `agent-wire.md`'s list; goldens and previous-release tests. *Changed by this amendment:* the mount is a source and
   options instead of a path and a mode, and a host port is a message of three fields instead of a number.
3. **The public API and the server** (`js-proto`, `js-jobs`, `js-lease-server`, `js-sched-placer`, `js-client`,
   `js-cli`): the proto fields and enums of 2.2 (`Mount.source` a `oneof` with `host_path` alone, `sub_path`,
   `no_exec`, `MountPropagation`, `HostPort` with `container_port` and `PortProtocol`), the stored tags, `validate`
   with the rules of 2.5, `same_job_as` with the defaults of 2.5, the read view, the inserted constraints
   (`exec=container` for a job port of its own among them), `start_of` and `RootfsNotReported`, the start failure
   detail and `end_of`; the regenerated client and CLI surface and their goldens (`cargo xtask gen-job-surface`),
   `docs/job-surface.md`. Nothing reaches an agent that does not report the key, and none does yet. *Changed by this
   amendment:* the shapes of `Mount` and `HostPort`, their validation and their comparison. The designed additions of
   2.4 are not in it.
4. **Mounts in `js-contain`**: the resolver's system calls (`openat2`, `open_tree`, `move_mount`, `mount_setattr`), the
   overlay the agent mounts for a root and for a copy-on-write mount, the unmounts, `remove_tree`'s mount rule, the
   private `<mounts>`; the root suite cases. `MOUNT_ATTR_NOEXEC`, `HostMount`'s `noexec` flag and the `sub_path` open
   of 4.2 are part of the same calls and the same spec entry.
5. **The `crun` agent's file system** (`js-agent`): admission, the `js-paths` pool and the preparing stage, the
   multi-image prepare, the assembly, the list of mounts and every cleanup path, the integrity exclusion,
   `--image-allow-loopback`, accepting `--mount-root`/`--mount-root-ro` with `crun`, the after-exit check, the kernel
   check and the `layered-rootfs` report. `no_exec` and `sub_path` are carried through the assembly as the mode is.
6. **Host ports on `crun`** (`js-agent`, `js-contain` for the init's `--wait-listen`): `--host-port`, the forwarder,
   adoption. The listener binds the job port and the forwarder connects to the host port, which differ only in the
   number each uses.
7. **macOS** (`js-agent`, `js-mac-pool`), when the Macs are built and after the candidate of 5.3 is chosen: the native
   runtime's `prepare`, the links, the chosen candidate, the profile items of 5.4, the refusals.
8. **An end-to-end run on one Linux node** with a FUSE file system as a layer's and a mount's source, a registry on
   `127.0.0.1`, and a service on a host port: the job reads lazily, writes copy up, the upload reaches the service, the
   run directory is removed, the integrity check passes, and a restart of the FUSE file system's service while no job
   runs leaves later jobs working. A runbook entry under `docs/runbooks/`, not a test of the PR set (B9): it is
   `docs/runbooks/fuse-layers-and-mounts.md`. It also measures the forwarder's throughput for an upload of a build's
   size (13, Q2).

**Items 4 to 6 are unaffected by this amendment** in subject, order, dependencies and mechanism: no new system call,
pool, file, flag or cleanup path. Each carries its half of an option of 2.2, as the sentence added to it says, because
the option is a flag of a call the item already makes (`mount_setattr`, `openat2`, `bind`). Items 1, 7 and 8 do not
change, except that item 7 adds the macOS rows of 5.1 for the options and the remap refusal.

Item 1 stands alone. Item 3 needs 2, and 4 to 6 need 2; until 5 lands no agent reports `layered-rootfs`, so items 2
and 3 change no running job. How a client uses the fields (what it sends, what it prepares on the host at `PLACED`, its
own wall limits) is that client's design.

**Designed additions, built when a client needs one** (2.4), each one pull request across the wire, the proto, the
server and the agent, after items 2 to 6, with its own detected capability, its tests by the same pattern as section
10, and the proto number 2.2 and 2.4 give it:

9. **tmpfs mounts** (`Mount.tmpfs`, `mount-tmpfs`), with `mount-source-unsupported-here` on macOS.
10. **Image mounts** (`Mount.image`, `mount-image`).
11. **UDP host ports** (`PORT_PROTOCOL_UDP`, `host-port-udp`).
12. **Host sockets** (`JobSpec.host_sockets`, `HostSocket`, `--host-socket`, `host-sockets`), on Linux (4.5) and, with
    item 7, on macOS (5.4).
13. **Receiving propagation** (`MOUNT_PROPAGATION_RECEIVE`, `mount-propagation-receive`), only if a client shows a
    mount it makes below a source after a job's start that the job must see.
14. **Id-mapped mounts** (`Mount.id_mapping`, `IdMapping`, `--map-id`, `mount-idmap`), on the kernels and file systems
    that support them; until then a provider presents writable trees owned by the job's container identity (LR5).

## 12. Decisions

Each is decided here; the owner may overturn any.

- **LR1. The operator's allow-list of host paths is the existing mount-root table, read as prefixes.** A second list
  would be a second way to say the same thing; the table already has the canonicalisation and its tests, and only the
  race kept it off `crun` (4.1, 4.2).
- **LR2. Jobs name absolute host paths, not a root and a relative path.** A client passes a file path; the
  roots bound what it may be. The legacy named form stays for the legacy job form.
- **LR3. The agent mounts the root's overlay as root and gives it to `crun`**, as Docker's daemon does; `crun` cannot
  overlay `/` (4.3). Cleanup is the agent's, as the mounter (4.6).
- **LR4. An image layer is the image's flattened tree.** The store keeps one tree per image; per-layer storage with
  whiteouts kept is not asked for (B20). Two image layers stack as whole trees.
- **LR5. The agent does not change the ownership of a host path; id mapping is a named, designed option.** Id-mapped
  mounts of arbitrary file systems are not available on every kernel and file system a host may use, so until build
  item 14 a provider presents writable trees owned by the job's container identity (4.4). `Mount.id_mapping` (2.4) is
  the additive option that lifts this where the host supports it, behind its own capability and an operator's
  `--map-id` list; it never chowns a file.
- **LR6. The allow-lists are checked by the agent, not advertised.** Prefix lists do not fit a capability value, and a
  pool's hosts share a configuration; a mismatch is a configuration mistake with a named refusal (2.6).
- **LR7. Every host path is resolved with `openat2` beneath its root and used through a clone** (4.2), so no path text
  is read twice and none reaches a mount option.
- **LR8. Host ports are a listener the agent holds in the job's namespace**, not a firewall exception or a process in
  the namespace (4.5); they work at every level, `none` included.
- **LR9. A host port is a message of a host port, a job port and a protocol**, named as `PortForward` names its ports.
  The job port defaults to the host port, which is all macOS can offer (5.4), so a job that gives none runs on both
  platforms; one that gives its own runs only in a container (2.6). The protocol is TCP now; UDP takes the next value.
- **LR10. On macOS a read-only or read-write mount is a link; how a copy-on-write mount is made is not chosen** (5.3).
  The owner decides it from measurements of a lazy provider's real macOS mount. Every candidate realises the same
  `COPY_ON_WRITE` mode, so the spec does not change whichever wins.
- **LR11. The agent checks a failed job's host paths once after it ends**, and nothing while it runs (section 8).
- **LR12. The job's lifecycle is a documented guarantee any client may rely on; no new state or watch for "creating".**
  `PLACED` with a host and `STARTING` are the moments a client can use, and `JobTimes` already gives the time spent
  preparing. Section 7 states, as rules a client can rely on, what the existing states promise: forward only (with the
  whole-host requeue as the one way back), a read may miss a state, the host is known from `PLACED`, inputs are first
  touched on that host while creating.
- **LR13. The tree wire is retired additively; only the never-reported capability key is removed** (section 6).
- **LR14. `input-unavailable` is a public start failure code of its own; a refused allow-list is not.** The first is
  the host's state and worth a retry elsewhere; the second is a configuration mistake, named in the detail.
- **LR15. Images keep today's failure mapping.** A failed pull of an image layer fails as a failed pull of the image
  does, so one rule covers both.
- **LR16. The agent's mounts live in `/run/js-agent/mounts`, and their list in a file there, not in the ledger** (4.3,
  4.6). The scratch root is swept by every release, and an earlier release reads the ledger strictly.
- **LR17. A mount's source is a `oneof`, now, with one member.** A later source (tmpfs, an image) is a new member, not a
  second message or a field that changes what `host_path` means; an earlier server meets it as a mount with no source
  and refuses it by name. `Mount`'s numbers are set with room for nine sources before its other fields (2.2), which is
  free while nothing is built.
- **LR18. Options are named fields with defaults, never a list of option strings.** Each is validated at submit and
  means the same on every runtime; `nosuid` and `nodev` are not options, since no job may clear them, and `no_exec` is
  one, since a job may want it and no job is harmed by it. A field whose only value would be "on" for good is not added;
  propagation and protocol have one value now and a designed second (2.4), which is why they exist.
- **LR19. Propagation is private, and the field exists now.** Private is what 4.2 makes and what a lazy provider's tree
  needs; naming it lets a receiving value be added later (2.4) without a second field. Sharing a job's mounts with the
  host is refused for good.
- **LR20. A host socket is a message of its own with an allow-list of its own** (2.4): it gives a service, not a tree;
  no mount option applies to it; and an operator who lets jobs read a tree should not thereby let them connect to a
  socket in it. It is designed now and built when a client needs it, as the faster path of Q2.
- **LR21. Every later addition is refused or seen to be dropped where it is not known, and placed only where it is.**
  An unknown source or value is `INVALID_ARGUMENT` at an earlier server; an unknown option or repeated field is dropped
  there, which the client's `Job.spec` check sees; an unknown kind or value fails the decode of that one start at an
  agent; and each addition has a detected capability of its own that placement requires, so no agent that cannot do it
  is sent it (2.4, 2.6).
- **LR22. Volumes the runtime keeps, network and device file systems, and free option strings are not part of the
  interface** (2.1). Each is something Docker, the OCI runtime or Kubernetes has; each would make jobscheduler a storage
  client or the owner of state between jobs, or would give a job options no one validates. A provider mounts such
  things on the host, and a job names the directory. Path expressions and facts about the job as files are deferred,
  not refused; id mapping is a designed option (LR5).

## 13. Open questions

- **Q1. Which candidate makes a copy-on-write mount on macOS** (5.3): a union view, APFS clones, or copy on first
  write. It is decided after a lazy provider's real macOS mount is measured, by the five tests of 5.3.
- **Q2. The forwarder's throughput.** Every byte of an upload through a host port is copied by the agent. If build item
  8 measures it short of what the uploads it is built for need, the next step is `splice(2)` between the two sockets,
  then host sockets (2.4, build item 12) for providers that offer one: the job then connects to the provider's socket,
  and the agent copies nothing. Neither changes a shipped field.
- **Q3. Retrying a start that failed `input-unavailable`** on another host is the client's today (a start failure ends
  the job). Whether the server should place it again belongs with `retries-and-runs.md`, not here.
