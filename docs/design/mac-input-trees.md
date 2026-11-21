# Input trees on a Mac: provider mounts and copy-on-write mounts

Status: **decided for the owner's review; nothing here is built.** It settles question Q1 of
`docs/design/job-rootfs.md` (section 13): how a native macOS job gets a `COPY_ON_WRITE` mount. It also settles what the
macOS agent needs to take a mount provider's tree at all, which Q1 depends on, because the tree a client most wants on
a Mac is a provider's. The wire, the proto, the server and placement do not change (section 6). job-rootfs.md 5.3
keeps the three candidates it compared and points here for the choice.

**What changes for a user.** Today a native job on a Mac (a pool user under `sandbox-exec`, in `exec/` of its home)
can have `READ_ONLY` and `READ_WRITE` mounts of host paths, as links in `exec/` (job-rootfs.md 5.2). It is refused a
mount whose source is a provider (`input-not-allowed`, "a Mac is given no mount provider") and every `COPY_ON_WRITE`
mount (`rootfs-unsupported-here`). After this design:

- a `READ_ONLY` mount may name a provider, as on Linux. It is the same link, to a directory the provider has written
  whole on the Mac's own disk (2.1);
- a `COPY_ON_WRITE` mount at `/work` or below it, of a host path or of a provider's tree, is a **private copy** of the
  tree. The agent makes it before the job's first process, in the job's own home, and the job's user owns it. The job
  may change anything in it. Nothing it does reaches the source. The copy goes with the home;
- no job, and no process of the agent, reads a network volume. The agent refuses at start a mount root on one, and
  refuses a start whose source is on one (2.1).

What does not change: layers stay refused on a Mac (`rootfs-unsupported-here`, and placement never sends them,
job-rootfs.md 2.6). The rules for mount targets change in two ways only: a copy-on-write mount at `/work` or below it
is taken, and a read-only or read-write mount may lie below a copy-on-write target (3.1). The Seatbelt view of the other
mounts and every Linux behaviour stay the same. A job that names none of these runs as today.

What a Mac gives up: a provider's tree is not read lazily on a Mac. The provider writes it whole before the job's first
process (2.1).

Marks used below: *read* means read in code, a doc or a Mac; *measured* means measured on a Mac and cited; *inferred*
means reasoning.

## 1. What a Mac has today

- **macOS refuses reads of a network volume to Apple's programs.**
  - *Measured* on a Mac Studio with macOS 27.0.1, on tierstore's loopback NFS mount. `ls`, `sh`, `shasum` and
    `python3` get `EPERM` for every user, from a shell and from a launchd job alike.
  - No request reaches the file-system service. The privacy daemon (TCC) logs its Network Volumes service with
    "Platform binary prompting is Deny".
  - What a program that is not Apple's gets, and what a device-management profile would have to grant, is being
    measured. Section 2.1 does not depend on the answer.
- **Every job program is started with its privacy responsibility disclaimed** (`docs/design/spawn-disclaim.md`): today
  by the helper's `run` verb, and by the agent program once the helper is folded into it.
  - A job answers for itself, so no grant of the agent's reaches it.
  - macOS therefore checks the job's own program: `/bin/sh`, `python3`, Apple's toolchain, or a program a build has
    just made.
- **tierstore on a Mac serves read-only trees, through that NFS mount.**
  - Its provider accepts `MOUNT_MODE_READ_ONLY` and `UNSPECIFIED`. It refuses every other mode `FAILED_PRECONDITION`
    with "tierstore makes read-only trees" (*read*: komira-ai/tierstore `crates/tierstore/src/server/provider/sources.rs`).
  - It answers `Mount` with a directory `<mountpoint>/mount-provider/<agent>/<run>/<index>` of its NFSv3 loopback
    mount, the agent and the run in hex (`provider/names.rs`).
  - The mount is `vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard` (*read*:
    `crates/tierstore/src/server/treefs/nfs/mount.rs`). It is hard: a read waits for the server for good.
  - Its owner decided that a job's writable directory is the client's, not the store's (tierstore
    `docs/design/virtual-build-fs.md`, decision C9). tierstore will build no writable view on macOS.
  - The macOS package installs and starts the cache alone (`packaging/macos-pkg/tierstore.plist`). The NFS frontend
    (`tierstore fs`) is not in it.
- **The OS has no union mount to build on.**
  - `MNT_UNION` gives lookup fallthrough only: no merged listing, no copy-up, no delete of a lower file.
  - A second NFS server of ours with tierstore below it measured 1.6 times slower on a cold `stat`, 25 times slower
    with eight threads, 1.8 times slower for `clang` and 1.5 times slower for `cargo` (*measured*: virtual-build-fs.md
    section 8, macos E18c). `MNT_UNION` is in its section 4.2.
  - The Macs have no third-party FSKit module, no FUSE-T and no macFUSE, and 0 system extensions (*read*:
    `systemextensionsctl list`, `pluginkit -m -p com.apple.fskit.fsmodule` and `/Library/Filesystems` on a macOS 26.4
    and a 27.0.1 host).
- **Clones work within one volume.**
  - Within one APFS volume a clone takes 8 to 12 µs per file: 1.5 s for 190,000 files (*measured*: macos E3,
    virtual-build-fs.md section 1).
  - `clonefile(2)` from the NFS mount to APFS is `EXDEV` (*measured*: macos E18).
  - `/Users`, `/private/var`, `/usr/local` and `/tmp` are one APFS volume on these hosts (*read*: `stat -f %d`). That
    volume holds both the homes and tierstore's directories.
- **The native runtime already has the hooks a copy needs.**
  - `Runtime::resolve_fs` and `Runtime::assemble_fs` run off the lifecycle's worker and answer `Pending` until they are
    done (`crates/js-agent/src/lifecycle/runtime.rs`).
  - The native runtime implements the first. It answers `Ready` at once, after `canonicalize` calls made on the worker
    (`native/mod.rs`, `mounts::resolve`). It has the default of the second.
  - The lifecycle waits for an assembly under way rather than discard it (runtime.rs, `discard_fs`;
    `lifecycle/machine/prepare.rs`, `ending`). `assemble_fs` is given no stop.
  - `discard_fs` is called at the cleanup of every run that reached the preparing stage, including a run that ran
    (`lifecycle/machine/cleanup.rs`).
  - The pool user is lent only when the run is created, in `create_and_start` before `launch`, through the helper's
    `user-create` today. The uid that will own the copy is therefore not known while the start is preparing.
  - `create_and_start` and `launch` run on the lifecycle's worker (`native/mod.rs`).
- **The provider client is not tied to Linux.**
  - `crate::provider`, `lifecycle/mount_providers.rs` and the lifecycle's `providing` stage have no `cfg` gate.
  - Only the three flags are Linux-only (`platform/mod.rs`, `LINUX_ONLY`; declared in `platform/linux.rs`).
  - So is the hello key `mount-provider.<name>`, reported from `contract/container.rs`.
  - On a Mac, `Jobs::providers` answers none (`platform/macos.rs`).
  - The lifecycle sends a mount's own mode in `Prepare` (`providing.rs`). A `COPY_ON_WRITE` mount of a tierstore tree
    is therefore refused `provider-refused` today, on Linux as well.

**One agent program per Mac.** The owner ruled that a Mac runs one agent program, with no separate helper. The model
is the Linux agent, whose runs are kept by "a supervisor of its own (the agent binary in helper mode) that outlives the
agent" (`jobscheduler-design.md`, the Linux runtime). How the Mac's supervisor is built is designed apart from this
document. This design needs only two things of it: the agent's own process does every step of section 3, and the job is
started in `exec/` after step 4 of 3.1. Today the helper's `run` verb does that.

## 2. The decision

### 2.1 Who reads a provider's tree, and the privacy control

Four processes could read a tree on a Mac:

| Process | What it reads | Through the NFS mount, it needs |
|---|---|---|
| the provider (tierstore's cache, its own program, as `_tierstore`) | its cache, on the local disk | nothing: it never reads its own mount |
| the agent (root, a launchd daemon) | the source of a copy-on-write mount, to copy it (3.2) | a Network Volumes grant for the agent's program, by a device-management profile naming its code signature. Whether a program that is not Apple's is refused like Apple's is being measured |
| the run's supervisor (the agent program; today the helper's `run` verb) | nothing of the tree: it starts the job in `exec/` | nothing |
| the job | a `READ_ONLY` link's target; nothing of a copy-on-write source | a grant for **every program the job runs**: `sh`, `python3`, the compiler, and programs the build itself has just made. Each answers for itself (section 1). A grant names a code signature, and a program a build has just made has no stable one. A grant to `/bin/sh` would reach every job of the Mac. This cannot be given |

So a job can never read through the mount. With the mount kept, every provider tree would have to be copied by the
agent, the read-only ones too. The agent would need a device-management grant, the Mac a root daemon serving the hard
mount, and every copy a full read through the network file system. The copy would also be a byte copy, never a clone
(`EXDEV`, section 1).

**Decision: no network volume on the path. The provider writes each tree whole, as plain files, into a directory on
the Mac's local disk, and answers `Mount` with that directory.**

- The provider reads only its own cache. The agent and the job read only local APFS files. Nothing is granted, and the
  NFS frontend is not run on a Mac.
- The directory lies on the homes' volume. It is beneath a root the operator lists to the agent with `--mount-root-ro`,
  and tierstore's directories are already on that volume (section 1). For tierstore it is a new directory
  `/usr/local/tierstore/trees`, which its package makes `_tierstore`'s, mode 0755. Every directory above it is mode
  0755, so a pool user can reach the one tree Seatbelt grants it (section 5).
- A `READ_ONLY` mount is the link of job-rootfs.md 5.2 to that directory. The files are the provider's (`_tierstore`),
  modes 0644 and 0755, so the job reads and cannot write them. Seatbelt grants the run that one directory, and denies
  it the rest of the root (job-rootfs.md 5.4).
- A `COPY_ON_WRITE` mount is the agent's copy of it (2.2). Because the source is on the homes' volume, the copy is a
  clone per file.
- The protocol does not change. `Mount` answers "an absolute path of a directory on the host, beneath a directory the
  operator lists to the agent as a mount root" (`provider.proto`). It answers once the directory holds the whole tree,
  within the agent's `--mount-provider-wait`.
- This is tierstore's change, on macOS only (build item 4). Its trees stay read-only, so decision C9 stands.

**The agent enforces the rule generically.** Every root of `--mount-root` and `--mount-root-ro` must be on a local
volume (`statfs`, `MNT_LOCAL`); the agent refuses to start otherwise, naming the flag. A source whose resolved directory
is not on a local volume (a network mount below a root) is refused `input-not-allowed` at the lookup. Either way, a
configuration that no job could use fails by name, once, instead of as `EPERM` inside each job.

Rejected:

- **Keeping the mount, with device-management grants.** As the table shows, the jobs cannot be granted. It would also
  need a grant for the agent, a root daemon for the mount, and a full NFS read of every tree. tierstore's NFS frontend
  measured about 20 times slower than APFS on a cold `stat` (virtual-build-fs.md section 8).
- **The agent fetching trees through the cache's S3 front** into the run's directory. The agent would become a client
  of one store's object interface and tree-list format. job-rootfs.md keeps every such name out of jobscheduler's code,
  wire, flags and capabilities, and the provider interface exists so that it stays out. It would also do the
  provider's work a second time. It is rejected on that ground alone: its cost, a full read of the tree into the local
  disk, is the same as the decision's.

**What it costs.**

- **Laziness.** On a Mac a provider's tree is fetched and written whole before the job's first process, the files the
  job never reads included. The mount was the only lazy route, and the privacy control closes it to jobs.
- **Time.** A tree cold in the cache costs its fetch from upstream, as on any route. A warm tree costs a local write of
  its bytes and entries, which is not yet measured (build item 5). Inferred from the measured figures for a tree of
  46,362 files: 0.45 s to clone it, and 2.0 s to build it as one link per file, which is about what making its entries
  costs. A copy-on-write mount adds one clone per file on top.
- **Disk.** One more copy of each tree's bytes while a run holds it, beside the cache, unless the provider shares one
  written tree between runs (its choice). The clones of copy-on-write mounts share blocks. Disk caps are exempt from
  B14.

### 2.2 Copy-on-write: candidate (b), made per file

**Candidate (b) of job-rootfs.md 5.3, made per file: a private copy, built by the agent off the worker.**

- A `READ_ONLY` or `READ_WRITE` mount stays the link of job-rootfs.md 5.2. A provider's tree is the directory of 2.1.
- A `COPY_ON_WRITE` mount is copied whole into the run's home before the job runs. Each regular file is cloned when the
  source is on the volume of the homes (a provider's tree, by 2.1, and a host path on that volume), and its bytes are
  copied otherwise.

Why, against each candidate's tests (job-rootfs.md 5.3, tests 1 to 5):

1. **Semantics.**
   - The copy is an ordinary APFS tree that the job's user owns. Opening an input read-write, truncating, appending,
     renaming a file or a directory, deleting, hard-linking, changing a mode and `mmap`ing for writing all behave as on
     any directory.
   - Nothing reaches the source, because the source is never opened for writing.
   - It passes test 1 by construction. It renames a directory where the Linux overlay answers `EXDEV`
     (job-rootfs.md 4.4): a superset of the overlay's behaviour, not a different answer.
   - (a) passes test 1 only once we have written copy-up, whiteouts and rename into a file system of our own.
   - (c) fails test 1 by construction: Seatbelt cannot turn a write through a link into a copy.
2. **Laziness.**
   - None of the three can be lazy on a Mac. (a) would serve a network file system, which the privacy control refuses
     to the job's programs as it refuses tierstore's (2.1). Kept lazy, (c) would link into that mount.
   - A client that wants to avoid copying a large tree mounts it `READ_ONLY` and makes its own writable directory
     beside it (virtual-build-fs.md). That is (c), done by the client, which knows which files it writes.
3. **Cost.** One clone per file for a tree on the homes' volume: 1.5 s for 190,000 files (*measured*). A host path on
   another local volume costs a byte copy.
4. **Teardown.**
   - Nothing is mounted in the home, so `user-delete`, which refuses to cross a mount
     (`docs/design/mac-lease-teardown.md`), and `kill-uid` stay as they are.
   - No process blocks in a hard mount: nothing on the path is one (2.1).
   - (a) needs an unmount before `user-delete`, and a way to end processes blocked mid-read in a hard mount. Those stay
     in state `U` until a forced unmount (*measured*: macos E16).
5. **Fleet.**
   - Nothing is installed or approved for (b): `clonefile`, `read`/`write` and `lchown` are the OS's own.
   - (a) needs a signed FSKit module enabled on every Mac, or FUSE-T, whose licence is not permissive.

Rejected besides:

- **A provider-made writable tree** (the provider answers `COPY_ON_WRITE` with a per-run directory the job may write):
  it contradicts tierstore's decision C9. The protocol also carries no uid to give the tree to. The agent's clone of
  the provider's local tree (2.1) costs one clone per file and needs neither.
- **Hard links into the source**: a write through one changes the source's inode.
- **Sending `READ_ONLY` to the provider for a copy-on-write mount**: the agent would tell a provider something other
  than what the job asked, on one platform only. Section 5 changes the provider instead.

## 3. The mechanism

### 3.1 Where each step runs

1. **Admission** (`native/mounts.rs`, `check`, no I/O).
   - A `COPY_ON_WRITE` mount is no longer refused for its mode. Its target must be `/work` or strictly below it
     (`mount-target-unsupported-here` otherwise). `/work` itself is taken for this mode only: `plan_target` refuses it
     for every mode today.
   - Nesting is refused `mount-target-unsupported-here`, as now, with one change. A `READ_ONLY` or `READ_WRITE` mount
     may lie below a `COPY_ON_WRITE` target: the copy is a real directory of the home, and the link is made inside it
     after the copy (step 4).
   - A mount below a link, and a copy-on-write mount below another copy-on-write mount, stay refused. So does a
     `READ_ONLY` or `READ_WRITE` mount at `/work` itself.
   - Every mount counts toward `MAX_LISTED`, as now (`fs.mounts.len()`).
2. **Lookup** (`resolve_fs`, on the worker, as now).
   - The host path, or the directory the provider answered, is canonicalised. It must be a directory beneath a root of
     the agent, and on a local volume (2.1).
   - A copy-on-write mount needs only a read-only root (`--mount-root-ro`), as on Linux.
   - A provider's source is resolved at the ask whose `fs` carries the provider's directory. The first ask carries only
     the host paths (`lifecycle/job_fs.rs`, `JobFs::without_providers`).
   - The lookups stay metadata calls on local volumes, answered at once.
3. **Copy** (`assemble_fs`, new for the native runtime).
   - The first ask starts one copy per copy-on-write mount, on a pool of 4 threads of the agent's own (`js-copies`).
     The ask answers `Pending` until every copy has ended.
   - The lifecycle ignores the first ask's answer and asks again (`prepare.rs`). A failure is therefore kept by run id
     and given at every later ask. The state of a run's copies is kept by run id until `discard_fs`.
   - Each copy builds the tree in `<native-homes>/.js-agent-copies/<run>/<index>`. Its parent is made by the agent:
     root's, mode 0700, never through a link.
   - The parent is in the directory of the homes, so step 4's rename is one metadata operation. The agent refuses to
     start when `.js-agent-copies` and `--native-homes` have different `st_dev` (a volume mounted at that name).
   - No code lists the homes directory, so the extra entry meets no sweep. No run can read it: its user is not root,
     and the parent is 0700.
4. **Placing** (`launch`, after `exec/` and the staged inputs are made, before `link_all`):
   - Each staged tree is renamed to `exec/<rest>`. A copy at `/work` itself moves each top-level entry of the staged
     tree into `exec/`.
   - Each entry is then given to the run's user and the group `link_all` gives, with the existing hand-over
     (`HandOver`, `lchown`, never through a link). This is on the worker, as all of `launch` is. Its cost for a large
     tree is measured in build item 5 (section 11).
   - A name that is there already fails the start, as `link_all` does.
   - The links of the other mounts are made after the copies, so a link below a copy-on-write target lands in the copy.
   - The run's supervisor then starts the job in `exec/`; it reads nothing of the trees.

### 3.2 What the copy makes

The walk is one function over an injected file-system port. It lives in a module of `js-agent` that is built on every
platform, so its rules are tested in the Linux pull request check, where coverage is measured. The same module takes
the rules of admission (3.1 step 1, moved out of the macOS-only `native/mounts.rs`), the local-volume rule over an
injected `statfs`, and the staging, placing and sweep steps over the port. The macOS side is the port alone.

- Only the system calls are behind the port's macOS side: the clone, the clearing after it, and the hand-over.
- The macOS side clones a file with the `Cloner` of `js-treestore` (`mac/seal.rs`, `clone_file`; `js-agent` depends
  on the crate already) rather than add a second binding. A clone keeps the source's owner, mode, flags and extended
  attributes (the `Cloner` doc). So after each clone the port sets the mode of the table below, clears the flags,
  removes the ACL and every extended attribute, and the hand-over sets the owner. A byte copy is made with those
  results directly.
- It does not use `Sealer::clone_writable`: that clones a whole tree in one call, so it cannot treat entries one by
  one, and it refuses hard links.

| Source entry | Copy |
|---|---|
| directory | a directory, mode 0700 |
| regular file | a clone (`clonefile`, no follow) when `statfs` of the source root and of the staging parent name one volume; else a byte copy. Mode 0700 if the owner's execute bit was set, else 0600, as `clone_writable` gives. setuid and setgid are dropped |
| symbolic link | a link with the same text, never followed |
| hard links (one inode, two names) | two independent files |
| FIFO, socket, device | refused `invalid-mount`, naming the entry |
| two names that differ only in case | refused `invalid-mount`, naming both. The homes' volume ignores case, and a source on a case-sensitive volume may not |
| modification time | kept, for files and directories, so tools that compare times (make, ninja, cargo) see the source's order |
| extended attributes, ACLs, file flags | removed after a clone and never written by a byte copy: a tree a job may change keeps none of its source's quarantine or provenance marks |

The modes follow the ones the bundle store's writable clones already use (`js-treestore`, `clone_writable`). The
staged tree is root's until step 4 gives it away.

### 3.3 The Seatbelt view

- The copy is in the run's home, which the profile lets the run write already. It is not in the run's read-only list,
  which keeps the files, the staged inputs and the links.
- No read grant of the source is emitted, because the run never reads the source. The source's root stays a read deny
  of every run (job-rootfs.md 5.4).
- A `no_exec` copy-on-write mount is a `MountGrant` of the copy's path, read-only, with `no_exec`.
  - It emits the `(deny process-exec* ...)` and `(deny file-map-executable ...)` of a `no_exec` link.
  - It also emits that grant's read and ancestor-metadata allows, for a path in the run's own home. The run may read
    that path already, so they change nothing.
  - No new policy item is needed.

### 3.4 How long a copy may take

- A copy is bounded by the server's start timeout, which bounds the whole preparing stage from placement (900 s by
  default, `crates/js-lease-server/src/job_expiry.rs`). It has no bound of its own.
  - Every source is on a local volume (2.1), so a copy ends, or fails with the error the disk gives.
  - A bound on progress would guard against a hung network mount, which the local-volume rule keeps off the path
    (B20).
- A provider's own wait, before the copy, is bounded by `--mount-provider-wait`, as on Linux: it is 120 s by default
  and at most 900 s. An operator whose trees take longer to write raises it.
- **A stop waits for the copy.** The lifecycle waits for an assembly under way, and `assemble_fs` is given no stop.
  - A job cancelled while its copy runs therefore ends when the copy ends or fails.
  - For a clone that is about 1.5 s per 190,000 files.
  - If byte copies of host paths prove long, the next step is a stop that the lifecycle gives the runtime's assembly.
    That is a change to `Runtime`, which this design does not take now.

### 3.5 Cleanup

- A start that ends before step 4 (refused, cancelled, failed) has its staged trees removed after `discard_fs`, once
  the assembly has answered (3.4). `discard_fs` answers at once, on the worker (`cleanup.rs`), so it hands the removal
  to the copy pool and does not wait for it. A removal that fails is logged. The sweep when the agent starts is the
  backstop. `discard_fs` also removes the run's empty staging directory after step 4 has moved the trees out.
- After step 4 the copy is in the home and goes with it at `user-delete`, as the links do. That includes a copy that
  failed half-way through the hand-over, which is root's in part.
- An agent that starts removes everything under `<native-homes>/.js-agent-copies`, on the copy pool. No start is
  adopted before its copy is placed.
- A provider's source is released as on Linux: after the run is removed (`kill-uid`, `user-delete`), after
  `discard_fs`, and after the run's directories are removed (`cleanup.rs`, `Stage::Scratch`). The provider then removes
  the directory it wrote. The copy no longer needs the source once it is placed. Releasing the source earlier is an
  optimisation this design does not take.

## 4. Flags

No new flag. On a Mac:

| Flag | Change |
|---|---|
| `--mount-provider NAME=SOCKET` | taken on macOS, with the Linux meaning (`crates/js-agent/src/lib.rs`); leaves `LINUX_ONLY` |
| `--mount-provider-user NAME=USER` | taken on macOS. A user named by name is looked up with `getpwnam_r` (Directory Services), the call the Mac code already uses for its users (`js-mac-session`, `macos/host.rs`). A Mac's service accounts are not in `/etc/passwd`, and tierstore's `_tierstore` gets the highest free id from 399 down to 300, so its uid differs from Mac to Mac |
| `--mount-provider-wait SECS` | taken on macOS, unchanged (1 to 900, 120 by default) |
| `--mount-root NAME=PATH`, `--mount-root-ro NAME=PATH` | the root must be on a local volume on macOS, or the agent refuses to start, naming the flag (2.1). The operator lists the provider's tree directory as a read-only root, and it is a read deny of every run (job-rootfs.md 5.4) |
| `--native-homes PATH` | unchanged. It also holds the agent's `.js-agent-copies` |

The socket check needs nothing new. tierstore's socket on a Mac is `_tierstore`'s, mode 0600, in a directory of
`_tierstore` with mode 0700 (*read*: komira-ai/tierstore `packaging/macos/README.md`), which the check takes
(`crates/js-agent/src/provider/socket.rs`).

## 5. What the provider side must do

These are requirements of the provider interface, stated for any provider; tierstore is the one there is.

1. **Accept `COPY_ON_WRITE` as it accepts `READ_ONLY`.**
   - The agent keeps the job's changes on every platform: an overlay upper on Linux, the copy on a Mac. A provider that
     makes read-only trees therefore serves a copy-on-write mount with the same tree.
   - This also fixes the same refusal on Linux.
   - It changes no field: the value exists, and the proto's comment ("its changes are its own and never reach the
     tree") already says it.
2. **On a Mac, answer `Mount` with a local directory that holds the whole tree** (2.1).
   - The directory is plain files, owned by the provider, files 0644 or 0755 and directories 0755.
   - It is on the volume of the homes, beneath the directory the operator lists as a read-only mount root. For
     tierstore that is `/usr/local/tierstore/trees`, made by the package's `postinstall` (`_tierstore`, 0755) with the
     other directories it makes. Its parents are 0755.
   - The provider writes it from its cache. It answers `UNAVAILABLE` until it is whole, and the agent asks again within
     its wait.
   - It removes the directory at `Release` and `Sweep`.
   - It never answers with a directory of a network file system, which the agent refuses (2.1).

Rollout order: the provider side (1 and 2), then the agent's build items, then each Mac's flags.

- An agent of the previous release still refuses each of these mounts by name at start (`rootfs-unsupported-here`,
  `input-not-allowed`). Placement does not filter it out, as today.
- A provider of the previous release answers with its NFS directory, and the new agent refuses it by name
  (`input-not-allowed`, "not on a local volume").

## 6. Wire, capabilities and placement

- **No wire change.** The fields, the mode, the refusal codes and the provider protocol are all as they are.
  `MOUNT_MODE_COPY_ON_WRITE` exists in both the job proto and `provider.proto`.
- **One existing key, now reported on a Mac.** `mount-provider.<name>` is reported by the native hello for each
  `--mount-provider`, as the container hello reports it today. Its report moves out of `contract/container.rs` into
  the shared contract.
- **Placement is unchanged.**
  - A copy-on-write mount does not require `exec=container`. Only layers and a remapped host port do (`rootfs.rs`,
    `needs_container`).
  - A Mac already reports `layered-rootfs`, so such a job could already be placed there. What changes is that the
    agent no longer refuses it.
  - A provider's mount goes only to a host that reports the provider's key (`job_start.rs`, `MountProviderNotReported`).
- **No new key for copy-on-write on a Mac.** The agents of one pool are rolled together by one host group. During the
  roll an agent of the previous release refuses by name, and a client runs the job again (job-rootfs.md, Q3). A key
  for one roll would be machinery for a configuration window (B20).

## 7. Refusals and failures

No new code. Every case below has a named refusal, and none gives a different answer silently.

| Case | Learnt | Result |
|---|---|---|
| A mount root on a network volume | agent start | the agent does not start; the error names the flag and the volume |
| A provider the agent was not given | admission | `input-not-allowed`, as on Linux |
| A copy-on-write target outside `/work`; a `READ_ONLY`/`READ_WRITE` mount at `/work`; a mount below a link; a copy-on-write mount below another | admission | `mount-target-unsupported-here` |
| Layers | admission | `rootfs-unsupported-here`, unchanged |
| More mounts than `MAX_LISTED` | admission | `too-many-inputs-here`, unchanged |
| The source, or the provider's directory, missing, not a directory, or beneath no root | preparing | `input-unavailable` / `input-not-allowed`, as job-rootfs.md section 8 |
| The source, or the provider's directory, on a volume that is not local | preparing | `input-not-allowed`, naming the path and its file system |
| The provider refuses the ref or the mode (a provider before section 5's change 1, for a copy-on-write mount) | preparing | `provider-refused`, as on Linux |
| The provider's tree is not whole within `--mount-provider-wait` | preparing | `input-unavailable`, as on Linux |
| A read of the source fails during the copy, or the homes' volume is full | preparing | `input-unavailable`, naming the entry and the error |
| A FIFO, socket or device in the source; two names that differ only in case | preparing | `invalid-mount`, naming the entries |
| The rename into `exec/` fails (another volume under the homes) | creating | the start fails with the runtime's failure, naming the path: the host's configuration |
| The job's reads through a `READ_ONLY` link fail while it runs | the job | the job's own result, then the after-exit check (job-rootfs.md section 8) |

## 8. Differences from Linux a client may meet

The spec is the same on both platforms. A client that relies on any of these runs only in a container:

- **Laziness.** On a Mac a provider's tree is written whole before the job's first process (2.1). A copy-on-write
  mount of a host path is read whole as well.
- **Hard links** in the source are separate files in the copy. On Linux they stay one inode until written.
- **Renaming a directory** of the source succeeds on a Mac. On Linux it is `EXDEV`.
- **Case.** Two names that differ only in case are refused on a Mac.
- **Ownership.** The copy is the run's user's on a Mac. On Linux a writable tree is presented by its provider owned by
  the job's container identity (job-rootfs.md LR5).
- **Extended attributes** are not copied.
- **Network volumes** cannot be a mount's source on a Mac.

## 9. Tests

Each is a Rust behaviour test. None asserts a time: the copy pool is driven step by step.

**Where they run.**

- The shared module's tests and the placement test run in every pull request's Linux check, where coverage is
  measured.
- The macOS tests run today in the beta stage on Apple silicon, after a push to main. B9 and B9-linux keep macOS out of
  pull request checks.
- The owner has ruled that once the Macs are on the farm, Mac tests run in every pull request. The pull request that
  adds that job changes these rules in the same pull request:
  - B9, a shared-core rule: `AGENTS.md`, its canonical copy `docs/agents/shared-core.md`, and
    `docs/agents/shared-core.sha256`, held together by `cargo test -p xtask --test agents_md`, and the same core in
    every repository that carries it;
  - B10, if the job runs on our own Macs rather than a hosted runner;
  - B9-linux and its check (`cargo test -p xtask --test pr_linux_only`).
- From then on the macOS tests below run in every pull request.

**Shared module (every pull request, covered):**

| Test | Asserts |
|---|---|
| `copy_leaves_the_source_unchanged_after_the_copy_is_changed` | after the copy is written, truncated, appended to, renamed and deleted in, the source's listing and bytes are equal to before |
| `copy_gives_dirs_0700_files_0600_and_0700_for_an_execute_bit_and_drops_setuid` | the modes of the table in 3.2 |
| `copy_keeps_link_text_and_never_follows_a_link` | a link to a file outside the source is a link in the copy, and the outside file is not read |
| `copy_makes_hard_links_independent` | a write to one name leaves the other |
| `copy_keeps_modification_times` | files and directories keep the source's times |
| `copy_refuses_a_fifo_socket_or_device_as_invalid_mount` | the refusal and its exact detail |
| `copy_refuses_names_equal_but_for_case` | `invalid-mount` naming both entries, exact text |
| `copy_clones_on_one_volume_and_copies_bytes_across` | the port is asked to clone when the volumes match and never otherwise |
| `copy_read_error_is_input_unavailable_naming_the_entry` | `EIO`, `EACCES` and `ENOSPC` each give the exact detail |
| `copy_discarded_removes_its_staged_tree` | `discard_fs` after a failed copy and after a placed one; a failed removal is logged and found by the sweep |
| `clone_is_cleared_of_flags_acl_and_extended_attributes` | through the port, a source file with a quarantine attribute, a flag, an ACL and mode 0755 gives a copy with none of them and mode 0700 |
| `first_ask_failure_is_given_again` | a copy that fails at the first ask fails every later ask of the same run |
| `agent_refuses_a_copies_directory_on_another_device` | the start check, exact message |
| `agent_refuses_a_mount_root_on_a_network_volume` | the start check for `--mount-root` and `--mount-root-ro`, exact message, and a local root taken |
| `source_on_a_network_volume_is_input_not_allowed` | a host path, and a provider's directory, below a local root but on a network file system, exact detail |
| `staged_copies_removed_when_the_agent_starts` | leftovers of a previous process are removed, others' files are not |
| `copy_at_work_fills_the_working_directory` | each top-level entry is in `exec/`, and a name already there fails |
| `copy_on_write_target_outside_work_refused` | `mount-target-unsupported-here`, exact text, and `/work` itself taken for this mode only |
| `rename_into_exec_across_volumes_fails_naming_the_path` | the port's `EXDEV` at step 4 fails the start with the path |
| `provider_tree_not_whole_within_the_wait_is_input_unavailable` | `Mount` answering `UNAVAILABLE` past `--mount-provider-wait` |
| `link_below_a_copy_on_write_target_lands_in_the_copy` | the link is made after the copy, inside it |
| `mount_below_a_link_and_copy_below_copy_still_refused` | `mount-target-unsupported-here`, exact text |

**Placement (every pull request):** in `agent_api_job_cases.rs`,
`copy_on_write_provider_job_goes_only_to_a_host_reporting_the_key`.

**macOS (the beta stage now, every pull request once the Macs are on the farm).** These are the macOS-only glue, behind
the `cfg(target_os = "macos")` of `native` and `platform/macos.rs`, so the Linux check cannot run them:

- `macos_takes_the_provider_flags`, which holds `LINUX_ONLY` and the macOS flag list to clap;
- `native_hello_reports_the_provider_key`;
- `native_start_naming_an_unconfigured_provider_is_input_not_allowed`;

- `copy_on_write_is_planned_not_refused`;
- `copy_is_not_in_the_read_only_list`, with the profile text asserted whole;
- `no_exec_copy_is_denied_exec`;
- `same_volume_copy_is_a_clone`: the real port takes the clone path, not the byte copy, for a source on the homes'
  volume, and the copy's bytes equal the source's;
- `statfs_names_a_network_volume_not_local`: the real `statfs` of a local directory is local;
- job-rootfs.md 5.3 test 1, as a `sandbox-exec` behaviour test run against a host-path tree.

**tierstore (its repository):**

- `prepare_accepts_copy_on_write_as_read_only`;
- `prepare_still_refuses_read_write`, with the reason asserted by exact equality;
- on macOS, `mount_answers_a_local_directory_holding_the_whole_tree`, which also asserts the tree's modes and that
  `Release` removes it;
- the package test that the `postinstall` makes `/usr/local/tierstore/trees` with its owner and mode.

**Why this list covers the failure and boundary cases.**

- It takes each row of section 7, and each entry kind of the table in 3.2.
- It takes each place a copy ends: placed, discarded, failed, and found by a new agent.
- It takes each nesting of the new mount below another.
- It takes both sides of the local-volume rule, at agent start and at the lookup.
- It takes the one compatibility edge: a previous-release agent's refusal, which is unchanged and already tested.

## 10. Build items

One subject per pull request, in this order:

1. **The copy module** (`js-agent`, shared): the walk of 3.2 over its port; the admission rules of `native/mounts.rs`
   `check` moved into it unchanged; and the shared-module tests. No behaviour change.
2. **Providers on a Mac** (`js-agent`):
   - the three flags on macOS, and `getpwnam_r` for a user named by name;
   - `Jobs::providers` from the flags, and the hello key moved to the shared contract;
   - the local-volume rule for roots and sources;
   - `plan_of` taking a provider's directory.

   The "No provider reaches a Mac" text of `native/mounts.rs` goes. So do three phrases of `js-agent`'s rustdoc flag
   table: the three flags' "Linux build only", `--mount-provider`'s "taken with the crun runtime only", and
   `--mount-provider-user`'s "looked up in `/etc/passwd`", which becomes Directory Services on a Mac. The item gives
   `READ_ONLY` provider mounts.
3. **Copy-on-write on a Mac** (`js-agent`, `js-mac-pool`):
   - `check`, and `assemble_fs` driving the pool;
   - the staging parent and its start check;
   - placing and hand-over in `launch`;
   - `discard_fs`, and the sweep at start;
   - the `no_exec` grant of 3.3.

   The module docs of `native/mounts.rs` lose "its mechanism is not chosen". job-rootfs.md 5.1's row changes with it,
   and `docs/job-surface.md` gains the differences of section 8 (T19).
4. **tierstore** (komira-ai/tierstore, before 2 is used):
   - `Prepare` accepts `COPY_ON_WRITE`;
   - on macOS, `Mount` answers a local directory holding the whole tree (section 5);
   - the macOS package's `postinstall` makes `/usr/local/tierstore/trees`, and its documentation names that directory
     as the agent's read-only root.

   The NFS frontend is not installed on a Mac for jobs.
5. **A measurement on one Mac** (a runbook under `docs/runbooks/`, not a test of the pull request set): for a
   provider's tree of 46,362 files, cold and warm, the time from `Prepare` to a whole local tree; the clone of it; and
   the hand-over's time for the same tree. Run by the owner on an idle Mac.

Every item is the agent's: the one-program ruling adds no helper verb. If the helper still exists when items 2 and 3
are built, the hand-over and the `user-create` they call are the steps that move into the agent with it.

## 11. What is still measured, and what would change the decision

- **What a program that is not Apple's gets from the privacy control on the NFS mount**, and what a device-management
  profile would have to grant, is being measured.
  - It does not change 2.1: a job's programs cannot be granted, so the mount stays off a job's path whatever the agent
    is allowed.
  - If a granted agent could read the mount, it would allow a copy through the mount. That copy would be a full NFS
    read and never a clone, so it is still worse than the local tree.
- **The time to write a warm tree locally** (build item 5) decides how large a provider tree is worth sending to a Mac.
  It does not change the mechanism. If it is too long for the trees clients send, the provider can keep one written
  tree per tree key and share it between runs (its choice, no protocol change).
- **The hand-over's cost** for a large tree, on the worker. If it is large, the hand-over moves into the copy. The copy
  then needs the run's uid before the user is made: the agent would make the run's user at the start of preparing.
