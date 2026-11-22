# Input trees on a Mac: provider mounts and copy-on-write mounts

Status: **for the owner's decision; nothing here is built.** It settles question Q1 of `docs/design/job-rootfs.md`
(section 13): how a native macOS job gets a `COPY_ON_WRITE` mount. It also settles how the macOS agent takes a mount
provider's tree at all, because the tree a client most wants on a Mac is a provider's. The wire, the proto, the server
and placement do not change (section 6). It is written to two rulings of the owner:

- **One agent program per Mac**, with a root supervisor per run and no helper. Its design is
  `docs/design/mac-one-program.md`, in komira-ai/jobscheduler#991, not yet merged.
- **A new account per job**, deleted afterwards, in place of the pool of reused accounts. Its design is
  `docs/design/mac-account-per-job.md`, not yet merged.

Both land before this design's agent items (section 10), so the files they name will be there when it is built.

**This design adds no privacy grant and no device-management profile, for any program.** The one grant native jobs
need at all is the account-per-job design's Full Disk Access for the agent's program, which deletes accounts. This
design neither adds to it nor needs it for the trees.

## Decisions for the owner

Each decision gives the options and the recommendation; the sections cited hold the evidence.

**D1. A provider's tree is not read lazily on a Mac: a job's start waits until the whole tree is on the Mac's disk.**

- Options:
  - (i) accept that; or
  - (ii) keep the lazy NFS mount and grant every program a job runs the right to read it; or
  - (iii) build a lazy local file system of our own (an FSKit module).
- Recommendation: (i).
  - (ii) cannot be done. macOS refuses reads of a network volume to Apple's own programs (section 1), every job
    program answers for itself, and a program a build has just made has no signature to grant (2.1).
  - (iii) is a signed system extension on every Mac, a file system with overlay semantics of our own, and an unknown
    answer from the privacy control. It is the next step only if section 11's bound is passed.
- Cost, from a stand-in measured on two Macs (section 11):
  - A warm tree of 46,362 files takes about 6 s to write.
  - A tree the cache does not hold costs its download, as on any route.
  - A tree already written for an earlier job costs nothing more (D3).

**D2. tierstore writes each tree whole, as plain files, on the Mac's own disk, and answers the agent with that
directory.** This needs the tierstore owner's agreement.

- What tierstore decided: C9 (komira-ai/tierstore `docs/design/virtual-build-fs.md`) reads "NFSv3 now, FSKit later if
  it measures better" for the macOS frontend, and "the writable directory of a run is the client's, not the core's".
- What D2 changes: for jobs on a Mac, a written local tree replaces the NFS frontend. The trees stay read-only and the
  writable directory stays the client's, so that half of C9 holds. The frontend half does not: the NFS frontend
  cannot serve a job.
- Options:
  - (i) tierstore writes local trees;
  - (ii) the agent fetches each tree through the cache's S3 front;
  - (iii) no provider trees on a Mac.
- Recommendation: (i).
  - (ii) would make jobscheduler a client of one store's interface and tree format, which job-rootfs.md keeps out of
    its code, wire, flags and capabilities. It would also do the provider's work a second time at the same cost.
  - (iii) leaves the Macs without the trees clients build with.
- No protocol change: `Mount` already answers "an absolute path of a directory on the host" (`provider.proto`).

**D3. The trees directory is `/usr/local/tierstore/job-trees`, counted in tierstore's one cache budget.**

- The directory is on the Mac's data volume, the volume of the homes. It sits beside the cache directory, not in it,
  and is named apart from the cache's own `trees/`.
- Options:
  - (i) one budget: the cache's `local_cache.budget`, 200 GiB in the Mac's file; or
  - (ii) a second budget of its own.
- Recommendation: (i). Both hold bytes of the same trees, and a second number is a second thing to size on Macs whose
  disks are small.
- Counting: each written tree counts at its logical size, whether its files are clones that share blocks with the cache
  or copies. That overstates the space and never understates it. Counting the directory against the budget is a change
  to tierstore's accounting (section 5).
- Use of space:
  - one written tree per tree key, shared by every job that names it;
  - a tree is held while any job holds it, and is never evicted then;
  - once no job holds it, it stays for the next job until space is needed;
  - when the budget is reached, unheld trees go first, oldest release first, then cache objects by the cache's own
    marks.
- At the cap: if the trees that jobs hold fill the budget alone, `Mount` answers `UNAVAILABLE` ("no space"). The
  start fails `input-unavailable` when the agent's `--mount-provider-wait` ends, 120 s by default, and the client runs
  it again.
  - The wait is accepted. A distinct answer would be a new provider code, and a full cache on a Mac is a configuration
    to fix, not a state to optimise (B20).

**D4. Do not wait for the measurement of what a program that is not Apple's gets from the privacy control.**

- Options:
  - (i) decide now; or
  - (ii) wait.
- Recommendation: (i). Whatever the agent's program is allowed, a job's programs are refused. No job reads a network
  volume under this design, and no process of the agent does either (2.1). The answer can change only how a client
  outside jobs reads the mount.

**D5. A copy-on-write mount is one clone of the whole tree into the job's new home, then one walk that gives every
entry to the job's account. The source must be on the homes' volume.**

- Options:
  - (i) this, candidate (b) of job-rootfs.md 5.3;
  - (ii) a union file system of our own, (a);
  - (iii) per-file links with copy on first write, (c).
- Recommendation: (i).
  - It is an ordinary tree, so every program sees ordinary semantics.
  - Nothing is mounted in the home, and nothing is installed.
  - It measured 3.5 to 3.9 s for 46,362 files (section 11).
  - The walk is the existing `clone_writable` of `js-treestore`, extended to remove extended attributes (3.2).
  - A source on another volume would need a byte copy, a second path that no Mac needs: each Mac has one data volume.
    It is refused instead (section 7).
  - (ii) is the lazy file system of D1 (iii).
  - (iii) denies a program that opens an input for writing instead of giving it a copy (2.2).

## What changes for a user

Today a native job on a Mac (sandboxed under `sandbox-exec`, in `exec/` of its account's home) can have `READ_ONLY` and
`READ_WRITE` mounts of host paths, as links in `exec/` (job-rootfs.md 5.2). It is refused a mount whose source is a
provider (`input-not-allowed`, "a Mac is given no mount provider") and every `COPY_ON_WRITE` mount
(`rootfs-unsupported-here`). After this design:

- a `READ_ONLY` mount may name a provider, as on Linux. It is a link to the provider's written tree (D2);
- a `COPY_ON_WRITE` mount at `/work` or below it, of a host path or of a provider's tree, is a **private copy**, made
  in the job's own new home before its first process and owned by the job's account (D5). The job may change anything
  in it. Nothing it does reaches the source. The copy goes when the account is deleted;
- no job, and no process of the agent, reads a network volume. The agent refuses at start a mount root on one, and
  refuses a start whose source is on one (2.1).

**An example.** A client that wants a tree from tierstore as its writable working directory, and a second tree
read-only inside it, passes this file as `--spec.mounts-file`. It uses the existing fields (`docs/job-surface.md`),
with the job's other fields as for any native Mac job:

```json
[
  {"provider": {"provider": "tierstore", "ref": "cas/l/<64 hex digits of the tree list>"}, "target": "/work",
   "mode": "MOUNT_MODE_COPY_ON_WRITE"},
  {"provider": {"provider": "tierstore", "ref": "cas/l/<64 hex digits of the SDK's tree list>"}, "target": "/work/sdk"}
]
```

Once tierstore takes `COPY_ON_WRITE` (section 5, change 1), the same file runs on a Linux host. On a Mac:

- the job starts in a copy of the first tree;
- the second is a read-only link at `sdk` inside the copy;
- the job requires `mount-provider.tierstore`, as on Linux.

What does not change:

- Layers stay refused on a Mac (`rootfs-unsupported-here`; placement never sends them, job-rootfs.md 2.6).
- The rules for mount targets change in two ways only: a copy-on-write mount at `/work` or below it is taken, and a
  read-only or read-write mount may lie below a copy-on-write target (3.1).
- The Seatbelt view of the other mounts, and every Linux behaviour, stay the same.
- A job that names none of these runs as today.

Marks used below: *read* means read in code, a doc or a Mac; *measured* means measured on a Mac and cited; *inferred*
means reasoning.

## 1. What a Mac has today

- **macOS refuses reads of a network volume to Apple's programs.**
  - *Measured* on a Mac Studio with macOS 27.0.1, on tierstore's loopback NFS mount: `ls`, `sh`, `shasum` and
    `python3` get `EPERM` for every user, from a shell and from a launchd job alike.
  - No request reaches the file-system service. The privacy daemon (TCC) logs its Network Volumes service with
    "Platform binary prompting is Deny".
- **Every job program answers for itself.** Each is started with its privacy responsibility disclaimed
  (`docs/design/spawn-disclaim.md`; under the one-program ruling the run's supervisor starts the job so,
  the one-program design, 4.2). No grant of the agent's reaches it, and macOS checks the job's own program: `/bin/sh`,
  `python3`, Apple's toolchain, or a program a build has just made.
- **tierstore on a Mac serves read-only trees, through that NFS mount.**
  - Its provider accepts `MOUNT_MODE_READ_ONLY` and `UNSPECIFIED`, and refuses every other mode `FAILED_PRECONDITION`
    (*read*: komira-ai/tierstore `crates/tierstore/src/server/provider/sources.rs`).
  - It answers `Mount` with a directory of its NFSv3 loopback mount, `<mountpoint>/mount-provider/<agent>/<run>/<index>`,
    the agent and the run in hex (`provider/names.rs`). The mount is
    `vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard` (`treefs/nfs/mount.rs`).
  - Its cache keeps an object either as a whole file (`obj/<aa>/<bb>/<hex>`) or as a span of a pack file, and a file
    of a tree list of version 2 is a span (tierstore `crates/tierstore/docs/operations.md`, the `span` request).
  - Its macOS package installs and starts the cache alone (`packaging/macos-pkg/tierstore.plist`). The cache's budget in
    the Mac's file is 200 GiB (`packaging/macos/tierstore.toml`).
- **The OS has no union mount to build on.**
  - `MNT_UNION` gives lookup fallthrough only (virtual-build-fs.md section 4.2).
  - A second NFS server of ours over tierstore measured 1.6 times slower on a cold `stat` and 25 times with eight
    threads (*measured*: virtual-build-fs.md section 8, macos E18c).
  - The Macs have no third-party FSKit module, no FUSE-T and no macFUSE (*read*: `systemextensionsctl list`,
    `pluginkit -m -p com.apple.fskit.fsmodule`, `/Library/Filesystems` on a macOS 26.4 and a 27.0.1 host).
- **Clones work within one volume, and a whole tree clones in one call.**
  - `clonefile(2)` of a directory clones its whole tree. *Measured* for 46,362 files in 12,394 directories (section
    11): 0.5 to 0.9 s. One call per file took 6.7 to 7.6 s.
  - `js-treestore` already clones a tree writable for a job: `FlagSealer::clone_writable` (`mac/seal.rs`).
    - It clones with one call, then, top-down by descriptor, clears flags and the ACL, gives each entry to an owner, and
      sets directories 0700 and files 0700 or 0600, dropping setuid and setgid.
    - It refuses a hard link, a device or a pipe, and nesting past `js_bundle::MAX_DEPTH` (64).
    - The agent uses it for bundles today (`native/bundle.rs`).
  - `/Users`, `/private/var`, `/usr/local` and `/tmp` are one APFS volume on these hosts (*read*: `stat -f %d`).
- **The native runtime already has the hooks a copy needs.**
  - `Runtime::resolve_fs` and `Runtime::assemble_fs` run off the lifecycle's worker and answer `Pending` until done
    (`crates/js-agent/src/lifecycle/runtime.rs`). The native runtime implements the first, answered at once, and has
    the default of the second.
  - The lifecycle waits for an assembly under way rather than discard it (`lifecycle/machine/prepare.rs`, `ending`);
    `assemble_fs` is given no stop. `discard_fs` is called at the cleanup of every run that reached the preparing stage
    (`lifecycle/machine/cleanup.rs`).
  - Today the account is lent from the pool in `create_and_start`, on the worker (`native/mod.rs`). Before it, the
    start's own checks run (`check_run_env`, the body, the working directory, the Xcode, the bundle), and the journal
    line is synced before any account exists.
  - A run with no mounts and no images never reaches `resolve_fs` or `assemble_fs` (`prepare.rs`).
- **The provider client is not tied to Linux.**
  - `crate::provider`, `lifecycle/mount_providers.rs` and the `providing` stage have no `cfg` gate.
  - The three flags (`platform/mod.rs`, `LINUX_ONLY`) and the hello key `mount-provider.<name>`
    (`contract/container.rs`) are Linux-only, and on a Mac `Jobs::providers` answers none (`platform/macos.rs`).
  - The lifecycle sends a mount's own mode in `Prepare` (`providing.rs`). A `COPY_ON_WRITE` mount of a tierstore tree is
    therefore refused `provider-refused` today, on Linux as well.

## 2. The decision

### 2.1 Who reads a provider's tree, and the privacy control

| Process | What it reads | What it would need through the NFS mount |
|---|---|---|
| the provider (tierstore's cache, as `_tierstore`) | its own cache, on the local disk | nothing: it never reads its own mount |
| the agent (`js-agent`, root, a launchd daemon) | the source of a copy-on-write mount, to copy it (3.1) | a Network Volumes grant for its program, by a profile naming its signature |
| the run's supervisor (`js-agent __supervise-native`, root, one per run) | nothing of the tree: it starts the job in `exec/` | nothing |
| the job (its own account) | a `READ_ONLY` link's target | a grant for every program it runs, including one a build has just made, which has no stable signature to grant. A grant to `/bin/sh` would reach every job of the Mac. This cannot be given |

So no job can read through the mount. With the mount kept, every tree would have to be copied by the agent, the
read-only ones too. That copy would need a grant for the agent, a root daemon for the hard mount, and a full NFS read of
every tree, never a clone (`EXDEV`).

**The decision (D2): no network volume on the path.**

- The provider writes each tree whole into a directory on the Mac's local disk, and answers `Mount` with it.
- The provider reads only its cache. The agent and the job read only local APFS files.
- The directory is beneath a root the operator lists with `--mount-root-ro`, on the homes' volume, so a copy of it is a
  clone (D5). For tierstore it is `/usr/local/tierstore/job-trees/<key>`.
- A `READ_ONLY` mount is the link of job-rootfs.md 5.2 to it. Its files are `_tierstore`'s, modes 0644 and 0755, so a
  job reads and cannot write them. Seatbelt grants the run that one directory and denies it the rest of the root
  (job-rootfs.md 5.4).
- None of these reads needs Full Disk Access (*inferred*).
  - The provider's trees are not in a place the privacy control protects, and writing in a home the agent made needs
    none either, as the agent's `exec/` does today.
  - A copy-on-write source that an operator's root puts under a protected folder (Desktop, Documents) would need one.
    After the one-program switch the agent's program holds Full Disk Access for the account-per-job design anyway, so
    that case needs nothing more.
  - Deleting an account's home and per-user folders does need the grant. That is the account-per-job design's grant,
    not this one's.

**The agent enforces the rule generically.**

- Every root of `--mount-root` and `--mount-root-ro` must be on a local volume (`statfs`, `MNT_LOCAL`), or the agent
  refuses to start, naming the flag.
- A source whose resolved directory is not on a local volume is refused `input-not-allowed` at the lookup.
- A configuration that no job could use therefore fails by name, once, instead of as `EPERM` inside each job.

### 2.2 Copy-on-write: candidate (b), the whole tree in one clone

A copy-on-write mount must give the job, at `exec/<rest>`, a tree that reads as the source. The job may create, change,
rename and delete anything in it. No change may reach the source. The tree is owned and writable by the job's account,
and goes with the run.

- **(b), chosen.** One `clonefile(2)` of the source directory into the job's new home, then one walk over the copy
  (3.2). The source must be on the homes' volume.
  - The result is an ordinary APFS tree. Opening an input read-write, truncating, appending, renaming a file or a
    directory, deleting, hard-linking, changing a mode and `mmap`ing for writing all behave as on any directory. Renaming
    a directory succeeds where the Linux overlay answers `EXDEV` (job-rootfs.md 4.4).
  - Nothing is mounted in the home, so the account's deletion crosses no mount (mac-lease-teardown.md, the deletion's
    step 1).
  - Nothing is installed.
- **(a), rejected: a union view.** It is a file system of ours with copy-up, whiteouts and rename, served from FSKit or
  FUSE-T. That means a signed system extension approved on every Mac, or FUSE-T, whose licence is not permissive, and an
  unmount before the account's deletion. A hung mount leaves job processes in state `U` until a forced unmount
  (*measured*: macos E16).
- **(c), rejected: per-file links with copy on first write.** Seatbelt cannot turn a write through a link into a copy,
  so a program that opens an input for writing is denied rather than given a copy. A client that knows which files it
  writes can still do this itself, over a `READ_ONLY` mount.
- **A provider-made writable tree, rejected.** It contradicts C9's "the writable directory of a run is the client's",
  and the protocol carries no account to give it to.
- **Hard links into the source, rejected.** A write through one changes the source's inode.
- **Sending `READ_ONLY` to the provider for a copy-on-write mount, rejected.** The agent would tell a provider something
  other than what the job asked, on one platform only. Section 5 changes the provider instead.

## 3. The mechanism

### 3.1 The flow of one run

Every step is the agent's program, `js-agent`, as root, except where it says the supervisor or the job.

1. **Admission** (`check`, no I/O).
   - A `COPY_ON_WRITE` mount is no longer refused for its mode. Its target must be `/work` or strictly below it
     (`mount-target-unsupported-here` otherwise). `/work` itself is taken for this mode only.
   - A `READ_ONLY` or `READ_WRITE` mount may lie below a `COPY_ON_WRITE` target. A mount below a link, a copy-on-write
     mount below another, and a read-only or read-write mount at `/work` stay refused `mount-target-unsupported-here`.
   - Every mount counts toward `MAX_LISTED`, as now.
2. **Providers** (the lifecycle's `providing` stage, as on Linux). `Prepare` at admission, then `Mount` until the
   provider answers its written tree, within `--mount-provider-wait`.
3. **Lookup** (`resolve_fs`, on the worker). Each source is canonicalised. It must be a directory beneath a root of the
   agent, on a local volume (2.1). A copy-on-write mount needs only a read-only root, and its source must be on the
   homes' volume (`statfs`). These are metadata calls on local volumes, answered at once.
4. **The account, for a run with a copy-on-write mount** (`assemble_fs`, first ask, on the agent's pool `js-copies` of
   4 threads; the account-per-job design, step 2).
   - First, the start's own checks that `create_and_start` runs today: the environment, the body, the working
     directory, the Xcode, the bundle. A start they refuse makes no account.
   - Then, in today's order: the lease is named and journalled and the journal synced, the ledger's
     `created <lease> <uid>` is written, and the account and its home are made (mode 0700, the account's, the lease
     group).
   - The account is made here because its copy needs it, and here is off the worker. `create_and_start` finds the lease
     and the account by run id.
   - A run with no copy-on-write mount gets its account in `create_and_start`, as the account-per-job design builds it.
     A run without mounts never reaches `assemble_fs` (section 1). Both places call one function.
5. **The copy** (the same `assemble_fs`, after step 4, on the same pool), for each copy-on-write mount:
   - The source must be on the homes' volume. A provider's tree on a Mac is, by D2. A host path on another volume is
     refused `input-not-allowed` at step 3.
   - The agent, as root, clones the source into `<home>/exec/<rest>` with `clone_writable`, owner the account and the
     lease group (3.2). For a mount at `/work` the clone is `<home>/exec` itself.
   - No process of the account exists yet, so nothing can race the walk.
   - The source is read as root and never written.
   - The lifecycle ignores the first ask's answer and asks again (`prepare.rs`), so a failure is kept by run id and
     given at every later ask.
6. **The start** (`create_and_start`, on the worker).
   - The agent makes `exec/` and `tmp/` where no copy made them, stages the start's `Mount` inputs into `exec/`, and
     makes the links of the other mounts. A link below a copy-on-write target lands in the copy. A name that the copy
     already holds fails the start, as `link_all` does today.
   - It starts the run's supervisor: `js-agent __supervise-native`, root, in a process group of its own (the
     one-program design, 4.2).
   - The supervisor starts the job as the account, in `exec/`, with its responsibility disclaimed. It reads nothing of
     the trees.
7. **The end** (`remove`, as the one-program and account-per-job designs give it).
   - The agent ends the supervisor, kills the account's processes, collects outputs, and deletes the account. The
     deletion takes the home whole, copy included, and needs Full Disk Access, which is that design's grant.
   - Only once the account is shown gone is its uid free.
   - The provider's sources are released after the run's directories are removed (`cleanup.rs`, `Stage::Scratch`), as
     on Linux.
   - `discard_fs`, which the lifecycle calls at every cleanup, finds nothing left for such a run.
8. **A run that never starts, after step 4** (refused, cancelled or failed).
   - `discard_fs` hands the account's deletion, copy included, to the pool. It answers at once, because it is called on
     the worker (`cleanup.rs`).
   - The lifecycle then frees the run's slot, while the deletion may still run.
   - A deletion that fails holds the account, as the account-per-job design says for any deletion: its lease stays
     live, its uid is not given again, and the agent admits one native job fewer until the deletion is repeated and
     shown complete. Two held accounts taint the host.
9. **The agent's start.** The agent adopts first and resets second (the one-program design, 4.2).
   - A run whose supervisor outlived the agent is adopted. Its account and its copy are left as they are. They go at
     the run's `remove` (step 7), and that holds also when the supervisor dies later and the run ends `GONE`.
   - A lease whose `supervisor` file names a live supervisor that the journal does not know (a crash between the
     supervisor's start and the journal line) has that supervisor ended first. Then its uid is killed and its account
     deleted.
   - A lease the server still holds for the host is kept until the server lets it go, as the boot reset keeps a held
     lease.
   - Every other account the ledger names is deleted, home and copy included. That covers an account made at step 4
     whose run never started, and a copy cut off half-way by a crash.
   - No copy lives outside a home, so there is no other sweep.

### 3.2 What the copy makes

- The copy is `js-treestore`'s `FlagSealer::clone_writable` (`mac/seal.rs`): one `clonefile`, then a walk by
  descriptor. It is extended in one way: the walk also removes each entry's extended attributes. It is not written a
  second time.
- Admission's rules, the local-volume and same-volume rules, and the order of steps 4 to 9 move into a module of
  `js-agent` that is built on every platform, over an injected port. They are tested in the Linux pull request check,
  where coverage is measured. The macOS side of the port is `clone_writable`, account creation and `statfs`.

| Source entry | In the copy |
|---|---|
| directory | mode 0700, the account's |
| regular file | mode 0700 if the owner's execute bit was set, else 0600, the account's; setuid and setgid dropped |
| symbolic link | the same text, never followed; the account's |
| a file with more than one link (a hard link) | refused `invalid-mount`, naming it, as `clone_writable` refuses it. A provider's written tree has none (section 5) |
| FIFO, socket, device | refused `invalid-mount`, naming the entry |
| nesting deeper than 64 directories | refused `invalid-mount`, as `clone_writable` refuses it |
| modification time | kept: a clone keeps it, and the walk changes only owner, mode, flags and attributes |
| file flags, ACL | cleared, as `clone_writable` does |
| extended attributes | removed (the extension). An attribute the system will not let root remove is left, and the agent's log names it (*inferred* that some, such as `com.apple.provenance`, are so protected) |

- Names: the source and the copy are on one volume, so the copy has the source's names and the volume's handling of
  case, and no collision can arise.
- *Inferred*, to be pinned by the macOS tests: a clone keeps the source's owner when root makes it, because
  `clone_writable` passes no `CLONE_NOOWNERCOPY`. It also keeps extended attributes, as `clonefile(2)` does.
- The agent refuses to start when the homes' volume ignores ownership (`MNT_IGNORE_OWNERSHIP`), the check
  `js-treestore`'s `mac/volume.rs` already makes for its own volume.

### 3.3 The Seatbelt view

- The copy is in the run's home, which the profile lets the run write already. It is not in the run's read-only list.
- No read grant of the source is emitted. The source's root stays a read deny of every run (job-rootfs.md 5.4).
- A `no_exec` copy-on-write mount is a `MountGrant` of the copy's path with `write` false and `no_exec`.
  - It emits the two denies of a `no_exec` link, and read allows of a path the run may read already.
  - `write` false does not make the copy read-only: the home's own grant keeps it writable.
  - For a mount at `/work` the path is `exec/` itself, so nothing in the working directory may be run.
  - No new policy item is needed.

### 3.4 How long it takes

- A copy is bounded by the server's start timeout over the whole preparing stage (900 s by default,
  `crates/js-lease-server/src/job_expiry.rs`), and by nothing of its own.
  - Every source is local, so a copy ends, or fails with the disk's error.
  - A progress bound would guard against a hung network mount, which 2.1 keeps off the path (B20).
- The provider's write is bounded by `--mount-provider-wait` (120 s by default, at most 900 s), as on Linux.
- **A stop waits for the copy.** The lifecycle waits for an assembly under way, and `assemble_fs` is given no stop. A
  job cancelled during its copy ends when the copy does: 3.5 to 3.9 s for 46,362 files (section 11).

## 4. Flags

No new flag. On a Mac:

| Flag | Change |
|---|---|
| `--mount-provider NAME=SOCKET` | taken on macOS, with the Linux meaning (`crates/js-agent/src/lib.rs`); leaves `LINUX_ONLY` |
| `--mount-provider-user NAME=USER` | taken on macOS. A name is looked up with `getpwnam_r` (Directory Services): a Mac's service accounts are not in `/etc/passwd`, and `_tierstore`'s uid is the highest free from 399 down to 300, so it differs from Mac to Mac |
| `--mount-provider-wait SECS` | taken on macOS, unchanged (1 to 900, 120 by default) |
| `--mount-root NAME=PATH`, `--mount-root-ro NAME=PATH` | must be on a local volume on macOS, or the agent refuses to start (2.1). The operator lists `/usr/local/tierstore/job-trees` as a read-only root |

The socket check needs nothing new. tierstore's socket is `_tierstore`'s, mode 0600, in a directory of `_tierstore`
with mode 0700 (*read*: komira-ai/tierstore `packaging/macos/README.md`), which the check takes
(`crates/js-agent/src/provider/socket.rs`).

## 5. What the provider side must do

These are requirements of the provider interface, for any provider; tierstore is the one there is. Each is tierstore's
to build and test (build item 5).

1. **Accept `COPY_ON_WRITE` as it accepts `READ_ONLY`.**
   - The agent keeps the job's changes on every platform, so a provider of read-only trees serves the same tree.
   - This also fixes the same refusal on Linux.
   - No field changes.
2. **On a Mac, answer `Mount` with a local directory that holds the whole tree** (D2, D3).
   - **Where:** `/usr/local/tierstore/job-trees/<key>`, one directory per tree key, shared by every source that names the
     key. The package's `postinstall` makes `job-trees` (`_tierstore`, 0755) beside the directories it makes today. Its
     parents are 0755. The name keeps it apart from the cache's own `trees/`.
   - **How it is written:** from the cache, on the same volume.
     - A file that the cache holds as a whole object is cloned from it (`clonefile`).
     - A file that is a span of a pack is copied from the span.
     - Either way the file ends `_tierstore`'s, 0644 or 0755, with no extended attribute. A tree list names files, not
       inodes, so the written tree has no hard links.
     - The tree is written into `job-trees/.partial-<key>` and renamed to `job-trees/<key>` when whole, so no reader
       ever sees part of a tree.
   - **Answers and concurrency:**
     - `Prepare` answers at once and works in the background, as the protocol says.
     - There is one writer per key. A second `Prepare` of a key being written joins the write in progress.
     - `Mount` answers `UNAVAILABLE` until the rename, within tierstore's 25 s (the protocol allows 30), and the agent
       asks again until its `--mount-provider-wait` ends.
   - **Sharing and space:**
     - A source holds its key's tree from `Prepare` to `Release` or `Sweep`, and never longer than the placement's life
       (`trees.ttl_max`, 24 hours), as a placement is held today.
     - A held tree is never evicted.
     - An unheld one stays until space is needed, oldest release first.
     - The written trees count at their logical size against `local_cache.budget`, which is a change to the cache's
       accounting: the directory is outside `local_cache.path` (D3).
   - **At the cap:** when the trees held by sources fill the budget alone, `Mount` answers `UNAVAILABLE` ("no space").
     The agent's wait ends `input-unavailable`.
   - **Never** a directory of a network file system, which the agent refuses (2.1).

Rollout order:

1. The provider side (1 and 2).
2. The two designs this one rests on: one program, then accounts per job.
3. The agent's build items.
4. Each Mac's flags.

During the roll:

- An agent of the previous release refuses each of these mounts by name (`rootfs-unsupported-here`,
  `input-not-allowed`). Placement does not filter it out, as today.
- A provider of the previous release answers with its NFS directory, and the new agent refuses it by name
  (`input-not-allowed`, "not on a local volume").

## 6. Wire, capabilities and placement

- **No wire change.** The fields, the mode, the refusal codes and the provider protocol are all as they are.
- **One existing key, now reported on a Mac.** `mount-provider.<name>` is reported by the native hello for each
  `--mount-provider`. Its report moves out of `contract/container.rs` into the shared contract.
- **Placement is unchanged.**
  - A copy-on-write mount does not require `exec=container`. Only layers and a remapped host port do (`rootfs.rs`,
    `needs_container`).
  - A provider's mount goes only to a host that reports the provider's key (`job_start.rs`, `MountProviderNotReported`).
- **No new key for copy-on-write on a Mac.** During the roll an agent of the previous release refuses by name, and a
  client runs the job again (job-rootfs.md, Q3); a key for one roll would be machinery for a configuration window
  (B20).

## 7. Refusals and failures

No new code. Every case has a named refusal; none gives a different answer silently.

| Case | Learnt | Result |
|---|---|---|
| A mount root on a network volume; a homes' volume that ignores ownership | agent start | the agent does not start; the error names the flag and the volume |
| The account-per-job design's Full Disk Access not in force | admission | `no-full-disk-access`, that design's refusal of every native job, unchanged here |
| A provider the agent was not given | admission | `input-not-allowed`, as on Linux |
| A copy-on-write target outside `/work`; a `READ_ONLY`/`READ_WRITE` mount at `/work`; a mount below a link; a copy-on-write mount below another | admission | `mount-target-unsupported-here` |
| Layers | admission | `rootfs-unsupported-here`, unchanged |
| More mounts than `MAX_LISTED` | admission | `too-many-inputs-here`, unchanged |
| The provider refuses the ref or the mode (a provider before section 5's change 1, for a copy-on-write mount) | preparing | `provider-refused`, as on Linux |
| The provider's tree is not whole within `--mount-provider-wait`, the cap included | preparing | `input-unavailable`, as on Linux |
| A source missing, not a directory, or beneath no root | preparing | `input-unavailable` / `input-not-allowed`, as job-rootfs.md section 8 |
| A source on a volume that is not local | preparing | `input-not-allowed`, naming the path and its file system |
| A copy-on-write source on a local volume other than the homes' | preparing | `input-not-allowed`, naming the path and both volumes |
| The account cannot be made | preparing | the start fails with the runtime's failure, naming the step |
| A read of the source fails during the copy, or the homes' volume is full | preparing | `input-unavailable`, naming the entry and the error |
| A hard link, FIFO, socket or device in a copy-on-write source, or nesting deeper than 64 | preparing | `invalid-mount`, naming the entry |
| The job's reads through a `READ_ONLY` link fail while it runs | the job | the job's own result, then the after-exit check (job-rootfs.md section 8) |

## 8. What a submitter sees on a Mac, and how it differs from Linux

The spec is the same on both platforms. On a Mac:

- **The start waits for the whole tree.** A provider's tree is written whole before the job's first process (D1).
  - Warm, by the stand-in of section 11, that is about 6 s for 46,362 files the first time the Mac meets the tree, and
    nothing for later jobs while it is kept.
  - A copy-on-write mount adds 3.5 to 3.9 s for the same tree.
  - Making and deleting the job's account cost more on top. They are measured in the account-per-job design, not
    here.
- **A cancel during the copy waits for the copy** (3.4).
- **Modes.**
  - In a copy, directories are 0700 and files 0600, or 0700 when the owner could execute them.
  - setuid and setgid are dropped.
  - Every entry is the job's account's.
  - On Linux a writable tree keeps its modes, owned as its provider presents it (job-rootfs.md LR5).
- **Hard links, FIFOs, sockets, devices** and nesting deeper than 64 in a copy-on-write source are refused. A
  provider's written tree has none of them. On Linux an overlay keeps hard links one inode until written.
- **Renaming a directory** of the source succeeds. On Linux it is `EXDEV`.
- **Extended attributes, ACLs and file flags** are not in the copy.
- **Sources** must be on a local volume, and a copy-on-write source on the homes' volume.

A client that needs the Linux behaviour (laziness, kept modes, hard links, overlay semantics) asks for
`exec=container`. Placement then sends the job only to a host that runs containers, never to a Mac's native agent. When
no such host reports the provider's key, the submit is refused `no-capable-host`, as for any constraint no host meets.

## 9. Tests

Each is a Rust behaviour test. None asserts a time: the pool is driven step by step and the clock is injected.

**Where they run.**

- The shared module's tests and the placement test run in every pull request's Linux check, where coverage is
  measured.
- The macOS tests run today in the beta stage on Apple silicon, after a push to main. B9 and B9-linux keep macOS out of
  pull request checks.
- The owner has ruled that once the Macs are on the farm, Mac tests run in every pull request. The pull request that
  adds that job also changes, in the same pull request:
  - B9, a shared-core rule: `AGENTS.md`, `docs/agents/shared-core.md` and `docs/agents/shared-core.sha256`, held
    together by `cargo test -p xtask --test agents_md`, and the same core in every repository that carries it;
  - B10, if the job runs on our own Macs rather than a hosted runner;
  - B9-linux and its check, `cargo test -p xtask --test pr_linux_only`.
- From then on the macOS tests below run in every pull request.

Every test below uses the repository's scratch helpers for its directories (T12). None touches the host's real homes,
`/usr/local/tierstore` or a service's directory, and the package test runs `postinstall` against a scratch target.

**Owner: jobscheduler, shared module (`js-agent`; every pull request, covered).** These run over the injected port:

| Test | Asserts |
|---|---|
| `copy_on_write_target_outside_work_refused` | `mount-target-unsupported-here`, exact text, and `/work` itself taken for this mode only |
| `mount_below_a_link_and_copy_below_copy_still_refused` | `mount-target-unsupported-here`, exact text |
| `agent_refuses_a_mount_root_on_a_network_volume` | the start check for `--mount-root` and `--mount-root-ro`, exact message, and a local root taken |
| `agent_refuses_a_homes_volume_that_ignores_ownership` | the start check, exact message |
| `source_on_a_network_volume_is_input_not_allowed` | a host path, and a provider's directory, below a local root but on a network file system, exact detail |
| `copy_on_write_source_off_the_homes_volume_is_input_not_allowed` | exact detail, naming both volumes |
| `start_checks_run_before_the_account_is_made` | a start refused for its environment, working directory, Xcode or bundle makes no account and writes no ledger line |
| `journal_then_ledger_then_account_then_copy` | the order of step 4, and every entry of the copy is the account's |
| `run_without_copy_on_write_gets_its_account_at_the_start` | no `assemble_fs` account; `create_and_start` makes it through the same function |
| `first_ask_failure_is_given_again` | a copy that fails at the first ask fails every later ask of the same run |
| `copy_failure_is_named` | a refused entry gives `invalid-mount` naming it; `EIO`, `EACCES` and `ENOSPC` give `input-unavailable` with the exact detail |
| `run_never_started_has_its_account_and_copy_deleted` | `discard_fs` after a failed copy and after a cancel hands the deletion to the pool; a failed deletion holds the account |
| `discard_after_remove_does_nothing` | `discard_fs` of a run whose account `remove` deleted |
| `agent_start_keeps_an_adopted_runs_copy_and_deletes_the_rest` | an adopted run keeps its account and copy; a lease with an unjournalled live supervisor has it ended before its account goes; a lease the server holds is kept; an account whose run never started, and one cut off half-way, are deleted |
| `copy_at_work_is_the_working_directory` | the clone is `exec/` itself; a staged input or link whose name the copy holds fails the start |
| `link_below_a_copy_on_write_target_lands_in_the_copy` | the link is made after the copy, inside it |
| `provider_tree_not_whole_within_the_wait_is_input_unavailable` | `Mount` answering `UNAVAILABLE` past `--mount-provider-wait` |

**Owner: jobscheduler, placement (`js-lease-server`; every pull request):**
`copy_on_write_provider_job_goes_only_to_a_host_reporting_the_key`, in `agent_api_job_cases.rs`.

**Owner: jobscheduler, macOS (`js-agent`, `js-treestore`; the beta stage now, every pull request once the Macs are on
the farm).** These are the macOS-only parts, behind `cfg(target_os = "macos")`:

- `js-agent`:
  - `macos_takes_the_provider_flags`, which holds `LINUX_ONLY` and the macOS flag list to clap;
  - `native_hello_reports_the_provider_key`;
  - `native_start_naming_an_unconfigured_provider_is_input_not_allowed`;
  - `copy_on_write_is_planned_not_refused`;
  - `copy_is_not_in_the_read_only_list`, with the profile text asserted whole;
  - `no_exec_copy_is_denied_exec`;
  - `statfs_reads_a_local_directory_as_local`.
- `js-treestore`, `clone_writable` with the real `clonefile`:
  - `clone_writable_removes_extended_attributes`, with a real quarantine attribute;
  - `clone_writable_keeps_link_text_and_modification_times`;
  - the existing tests of its modes, owner, flags, ACL and refusals of hard links, devices, pipes and depth, which stay
    as they are.
- The semantics test, as a `sandbox-exec` behaviour test: a program that opens an input read-write and changes it,
  truncates and appends, renames a file and a directory, deletes both, changes a mode and `mmap`s a file for writing.
  The source is unchanged after.

**Owner: tierstore (its repository, tierstore's maintainers):**

- `prepare_accepts_copy_on_write_as_read_only`;
- `prepare_still_refuses_read_write`, with the reason asserted by exact equality;
- on macOS:
  - `mount_answers_the_whole_written_tree_and_never_a_part`;
  - `mount_answers_unavailable_within_25_s_while_the_tree_is_written`;
  - `whole_objects_are_cloned_and_spans_copied`;
  - `concurrent_prepares_of_one_key_write_once`;
  - `a_held_tree_is_never_evicted_and_unheld_trees_go_oldest_release_first`;
  - `written_trees_count_at_logical_size_in_the_budget`;
  - `mount_at_the_cap_with_every_tree_held_is_unavailable_no_space`;
- the package test that the `postinstall` makes `job-trees` with its owner and mode, against a scratch target.

**Why this list covers the failure and boundary cases.**

- It takes each row of section 7, and each entry kind of the table in 3.2.
- It takes each place a run's account and copy end: removed after the job, discarded before it, held after a failed
  deletion, and found by a new agent with a live supervisor, with an unjournalled one, and with none.
- It takes each nesting of the new mount below another.
- It takes both sides of the local-volume and same-volume rules.
- For the provider it takes the first writer and a concurrent one, a tree still being written, a held and an unheld
  tree, and the cap.
- It takes the one compatibility edge: a previous-release agent's refusal, which is unchanged and already tested.

## 10. Build items

One subject per pull request, in this order. Items 3 and 4 come after the one-program switch (the one-program design,
section 7). Item 4 also comes after accounts per job (the account-per-job design, step 2), whose account it makes at
assembly.

1. **The shared module** (`js-agent`): the admission rules of `native/mounts.rs` `check`, moved into it unchanged; the
   port; and the shared-module tests that need no later item. No behaviour change.
2. **`clone_writable` removes extended attributes** (`js-treestore`), with its macOS tests.
3. **Providers on a Mac** (`js-agent`):
   - the three flags on macOS, and `getpwnam_r` for a user named by name;
   - `Jobs::providers` from the flags, and the hello key moved to the shared contract;
   - the local-volume rule;
   - `plan_of` taking a provider's directory.

   The "No provider reaches a Mac" text of `native/mounts.rs` goes. So do three phrases of `js-agent`'s rustdoc flag
   table: the three flags' "Linux build only", `--mount-provider`'s "taken with the crun runtime only", and
   `--mount-provider-user`'s "looked up in `/etc/passwd`".
4. **Copy-on-write on a Mac** (`js-agent`, `js-mac-pool`):
   - `check`, and the same-volume and ownership rules;
   - `assemble_fs` running the start's checks, then making the account and the copy on the pool;
   - `create_and_start` finding them;
   - `discard_fs` and the agent's start;
   - the `no_exec` grant of 3.3.

   The module docs of `native/mounts.rs` lose "its mechanism is not chosen". job-rootfs.md 5.1's rows and
   `docs/job-surface.md` gain the differences of section 8 (T19).
5. **tierstore** (komira-ai/tierstore, before item 3 is used): section 5 and its tests, owned by tierstore's
   maintainers. D2 needs the agreement of tierstore's owner, who owns C9.
6. **A measurement with the real programs on one idle Mac** (a runbook under `docs/runbooks/`, not a test of the pull
   request set).
   - It repeats section 11 with tierstore writing the tree from its cache, cold and warm, and with the agent's copy made
     as root to another account.
   - Neither can be measured now: the provider's local write does not exist until item 5. Handing a tree to another
     account needs root, which the measurement's user did not have.

## 11. Measured, and the bound past which this design is wrong

*Measured* on two idle Macs (macOS 27.0.1 and 26.4), as an unprivileged user, by a Python stand-in, not by tierstore or
the agent. The real programs cannot be measured yet: the provider's local write is build item 5, and the walk to
another account needs root (build item 6). The tree was synthetic but of a large build's shape: 46,362 files in 12,394 directories, 463 MB of data, one
file in ten executable. The source was in the page cache. Three runs on each Mac:

| Step | 27.0.1 | 26.4 |
|---|---|---|
| the provider's write, stood in for by a byte copy of every file (read, write, mode, times) | 5.5 to 6.0 s | 5.3 to 6.6 s |
| the same with `cp -R` | 8.0 to 8.6 s | 7.1 to 8.5 s |
| one `clonefile` of the whole tree | 0.5 s | 0.8 to 0.9 s |
| the walk of 3.2 over the clone (`lchown` to the same user, mode, flags, attribute check; 58,755 entries, the root excluded) | 3.0 to 3.1 s | 2.8 to 3.0 s |
| one `clonefile` per file, for comparison | 6.7 s | 7.5 to 7.6 s |
| removing a cloned tree | 3.1 to 3.2 s | 4.2 to 5.4 s |

Read as:

- **The provider's write** of a warm tree of this size is about 6 s. A tree the cache does not hold costs its download
  on top, as on any route.
- **The copy of a copy-on-write mount** is one clone and the walk: 3.5 to 3.9 s, summed run by run. An `lchown` to
  another account as root is *inferred* to cost about the same.
- **The removal** is part of the account's deletion.
- A compiled walk is expected to be faster than Python's (*inferred*). Build item 6 measures the real programs.

**The bound.** This design is wrong for a client when the time from the provider's `Prepare` to the end of the copy,
on these Macs and warm, for that client's median tree, passes **30 s**.

- The time excludes making the job's account, which the account-per-job design measures and every native job pays
  with or without trees.
- 30 s is three times what the stand-in measured for a tree of 46,362 files (about 6 s to write and 4 s to copy).
- It is about seventeen times the 99th percentile from placement to running of a Linux container start, 1.8 s
  (`docs/design/start-latency.md`). That is a different stretch of a start, given for scale.
- Past it, the start costs more than many build actions take. The answer is then the lazy local file system of D1
  (iii), not a faster copy.
- Below it the mechanism stands. A tree that several jobs share costs its write once.

**Still being measured, not waited for (D4):** what a program that is not Apple's gets from the privacy control on the
NFS mount. Under this design no job and no process of the agent reads that mount, so the answer cannot change it.
