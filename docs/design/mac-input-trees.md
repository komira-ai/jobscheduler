# Input trees on a Mac: provider mounts and copy-on-write mounts

Status: **proposed; every decision in it awaits the owner (D1 to D5).**

- **Step 1 (read-only whole trees) is being built.** It rests on two of those proposals: D1's answer (i), that a job's
  start waits for the whole tree, and D2, that the provider writes the tree on the local disk. Both await the owner,
  like the rest.
- **D1's option (ii) is untested, and a probe of it is running.** It would keep the lazy NFS mount by giving every
  program a job runs one inherited Network Volumes grant. Until the probe gives its verdict, nothing here says the mount
  cannot serve a job. The local trees of step 1 are the fallback if the probe fails.
- **The later steps (copy-on-write, D5; the trees' space, D3) are proposals only.**

This document answers question Q1 of `docs/design/job-rootfs.md` (section 13), how a native macOS job could get a
`COPY_ON_WRITE` mount, with a recommendation the owner has not yet decided. It also proposes how the macOS agent takes
a mount provider's tree at all, which step 1 builds. The wire, the proto, the server and placement do not change
(section 6).

## The rulings, and what is built first

The design is written to three rulings of the owner.

- **A native Mac job never gets administrator rights.**
  - Nothing in this design takes an administrator lease.
  - The job runs as its own account, which is never an administrator.
  - What runs as root is the agent's own work: making the job's links and its put directory, and, in a later step,
    the copy and its `lchown`. None of it is the job's.
- **One agent program per Mac**, with a root supervisor per run and no helper. Its design is
  `docs/design/mac-one-program.md`, komira-ai/jobscheduler#991, merged.
- **A new account per job**, deleted afterwards, in place of the pool of reused accounts.
  - Its design is `docs/design/mac-account-per-job.md`, merged with komira-ai/jobscheduler#1004.
  - #1004, merged, builds the step before it. It signs the macOS programs, grants the root helper alone Full Disk
    Access by a profile, reports the grant per host, and refuses every native job on a Mac without the grant. It keeps
    the pool reset as it is.
  - The new account per job is a later change, which that design says lands once the grant has been seen in force on a
    Mac.

**What is built first, and why.**

- **Step 1 is built now, on the agent as it is: today's helper and today's pool of reset accounts.**
  - The owner wants real builds on the Macs now.
  - A read-only tree needs no account to own, copy or delete anything of it. The provider owns the files, and any pool
    user may read them.
  - So step 1 waits for neither the one-program switch nor the new account per job.
  - #1004 is merged. Once its release and profile are on a Mac, every native job there needs the grant, step 1's
    included.
- **The copy-on-write step comes after both rulings are built.**
  - Its copy belongs to the job's own new account and goes when that account is deleted.
  - It is made by the one agent program.
  - Section 3.1 is written for that later state. It is the base case of the later step, not of step 1.

**Grants.** This design adds no privacy grant and no device-management profile, for any program. Native jobs on a Mac
need the Full Disk Access profile of komira-ai/jobscheduler#1004 for the root helper, and for the agent's program after
the one-program switch, with trees or without. Nothing in this design needs more.

## The steps

**Step 1, being built: read-only whole trees.**

- **tierstore** writes each tree whole, as plain files, into `/usr/local/tierstore/job-trees/<hex>` on the Mac's local
  disk, and answers `Mount` with that directory: komira-ai/tierstore#430 (`params.local_trees`). It has passed
  automated review, is flagged for the owner, and is open.
  - A trial passed on a Mac Studio: two sources of one tree were answered the same directory.
  - The unprivileged user `nobody` read the tree and ran its script, but could not write in it.
  - No partial directory was left.
- **The macOS agent** takes `--mount-provider` and gives a job a provider's tree as a `READ_ONLY` mount, under the
  local-volume rule of 2.1: komira-ai/jobscheduler#1006, open.
  - The link is the one of job-rootfs.md 5.2.
  - A `READ_WRITE` mount of a provider is refused `input-not-allowed`.
  - A missing put directory, a `READ_WRITE` mount of a host path, is made at the start below its writable root.
  - The provider-mode and local-volume rules are in a new module built on every platform, `native_fs`.
- **The client** sends its tree as a read-only provider mount (komira-ai/kbf#2403).
- **No copy-on-write.** A `COPY_ON_WRITE` mount stays refused `rootfs-unsupported-here` on a Mac.
- **The pool of reset accounts stays as it is**, with #1004's grant.
- **The trees directory has no eviction in step 1.** #430 keeps every tree, and an operator removes
  `job-trees/<hex>` by hand to free space. Decision D3 is the later step's.

**Not covered by this design: outputs.** This design is about a job's input trees. What a job writes for upload, its
put directory, was not part of it.

- In step 1 the provider ingests the outputs from a plain local directory after the job: komira-ai/tierstore#431
  (`local_cache.put_ingest`), merged.
- That is designed and built apart from this document.

**Later steps, proposals only**: copy-on-write mounts (D5), which come after both rulings are built, and the trees'
space (D3). D1 and D2 are step 1's own, and they also await the owner.

## The open decisions

Each needs the owner. The details and the evidence follow the list.

| | Decision | Options | Recommendation |
|---|---|---|---|
| D1 | Laziness on a Mac | (i) accept that a job's start waits for the whole tree; (ii) keep the lazy NFS mount: start the job not disclaimed from a signed starter that holds only a Network Volumes grant, so every program it runs inherits that one right (untested; probe running); (iii) a lazy local file system of our own (FSKit) | (i) for now, as the fallback; decide between (i) and (ii) on the probe's verdict. (iii) only past the bound of section 11 |
| D2 | tierstore writes whole local trees | (i) tierstore writes them; (ii) the agent fetches through the cache's S3 front; (iii) no provider trees on a Mac | (i), built in step 1, at least as the fallback of D1 (ii). Still missing: the owner's merge of komira-ai/tierstore#430, and a line in tierstore's decision C9 recording that the NFS frontend does not serve jobs on a Mac |
| D3 | Space of `job-trees` (a later step; step 1 has none) | (i) count it in the cache's one budget, with eviction of unheld trees; (ii) a budget of its own; (iii) keep removing trees by hand | (i) |
| D4 | Wait for the measurement of what a program that is not Apple's gets from the privacy control? | (i) decide without it; (ii) wait | (i) for the local trees, which no job or agent process reads over a network volume. D1 (ii)'s probe measures the question for its starter |
| D5 | Copy-on-write on a Mac (a later step) | (i) one clone of the whole tree into the job's new account's home, then one walk, candidate (b); (ii) a union file system, (a); (iii) per-file links with copy on first write, (c) | (i) |

## The decisions in detail

**D1. Laziness.** Recommended for now, as the fallback: a provider's tree is not read lazily on a Mac, so a job's
start waits until the whole tree is on the Mac's disk. The choice between (i) and (ii) waits for the probe of (ii).

- Options:
  - (i) accept that; or
  - (ii) keep the lazy NFS mount, with one inherited grant. This is **untested; a probe of it is running on a Mac
    Studio.** A small signed starter holds one right only, the Network Volumes grant, given by a device-management
    profile naming its signature. It starts the job *not* disclaimed, so macOS judges every program the job runs by
    the starter, its responsible process, and each inherits that one right; or
  - (iii) build a lazy local file system of our own (an FSKit module).
- Recommendation: (i) for now, and (ii) if its probe passes and the owner accepts its costs.
  - (ii) as first written here, a grant per program, cannot be given: a program a build has just made has no
    signature to grant (2.1). The inherited grant is the route that could work, and it is what the probe tests.
  - What (ii) would cost, if the probe passes:
    - a fourth program to sign, ship and name in a profile;
    - an exception to the rule that every job start is disclaimed (`docs/design/spawn-disclaim.md`), limited to that
      starter;
    - the hard-mount behaviour, in which a reader blocked mid-read stays unkillable until a forced unmount (*measured*,
      macos E16), to be handled before an account is deleted.
  - (ii) and the one-program ruling: the starter cannot be the agent's own program. After the one-program switch that
    program holds Full Disk Access (komira-ai/jobscheduler#1004's grant, moved to it), and a job started from it not
    disclaimed would inherit that grant too. So (ii) needs the starter as a program apart, holding only the Network
    Volumes grant. That is a second program, which the owner's ruling would have to allow.
  - (iii) is a signed system extension on every Mac, a file system with overlay semantics of our own, and an unknown
    answer from the privacy control. It is the next step only if section 11's bound is passed.
- Cost, from the stand-in of section 11 (not yet the real provider):
  - a warm tree of 46,362 files takes about 6 s to write;
  - a tree the cache does not hold costs its download, as on any route;
  - a tree already written for an earlier job costs nothing more.

**D2. tierstore writes each tree whole, as plain files, on the Mac's own disk, and answers the agent with that
directory.** It is built in step 1 (komira-ai/tierstore#430). It needs the tierstore owner's agreement.

- #430 is open: it passed automated review and is flagged for the owner.
- The agreement is the owner's merge of #430, and the line in C9 below.
- #431, the put ingest, is outside this design, so the agreement does not need it.

- What tierstore decided: C9 (komira-ai/tierstore `docs/design/virtual-build-fs.md`) reads "NFSv3 now, FSKit later if
  it measures better" for the macOS frontend, and "the writable directory of a run is the client's, not the core's".
- What D2 changes: for jobs on a Mac, a written local tree replaces the NFS frontend. The trees stay read-only and the
  writable directory stays the client's, so that half of C9 holds.
  - The frontend half is in question. As jobs are started today, disclaimed, the NFS frontend does not serve a job
    (section 1).
  - If D1 (ii)'s probe passes, it could. The local trees would then be the fallback, and C9's frontend would stand.
  - #430 does not edit C9's text. Recording the change there is the one piece of the agreement still missing.
- Options:
  - (i) tierstore writes local trees;
  - (ii) the agent fetches each tree through the cache's S3 front;
  - (iii) no provider trees on a Mac.
- Recommendation: (i).
  - (ii) would make jobscheduler a client of one store's interface and tree format, which job-rootfs.md keeps out of
    its code, wire, flags and capabilities. It would also do the provider's work a second time, at the same cost.
  - (iii) leaves the Macs without the trees clients build with.
- No protocol change: `Mount` already answers "an absolute path of a directory on the host" (`provider.proto`).

**D3. The space of `/usr/local/tierstore/job-trees`, for a later step.** Step 1 (#430) counts nothing and evicts
nothing.

- The directory is on the Mac's data volume, the volume of the homes. It sits beside the cache directory, not in it,
  and is named apart from the cache's own `trees/`.
- Options:
  - (i) one budget, the cache's `local_cache.budget` (200 GiB in the Mac's file), with eviction;
  - (ii) a second budget of its own;
  - (iii) keep step 1's removal by hand.
- Recommendation: (i). Both hold bytes of the same trees, and a second number is a second thing to size on Macs whose
  disks are small. (iii) fills a disk unattended.
- Counting: each written tree counts at its logical size. That overstates the space and never understates it.
- Use of space:
  - there is one written tree per tree key, shared by every job that names it;
  - a held tree is never evicted;
  - an unheld tree stays until space is needed;
  - when the budget is reached, unheld trees go first, oldest release first, then cache objects by the cache's own
    marks.
- At the cap: if the trees that jobs hold fill the budget alone, `Mount` answers `UNAVAILABLE` ("no space"). The
  start fails `input-unavailable` when the agent's `--mount-provider-wait` ends, 120 s by default, and the client runs
  it again.
  - The wait is accepted: a distinct answer would be a new provider code, for a configuration to fix (B20).

**D4. Whether to wait for the measurement of what a program that is not Apple's gets from the privacy control.**

- Options:
  - (i) decide without it; or
  - (ii) wait.
- Recommendation: (i) for the local trees. Under them no job reads a network volume, and no process of the agent does
  either (2.1), so the answer cannot change them. D1 (ii) is the one place the question matters, for its starter, and
  its probe measures exactly that.

**D5. Copy-on-write on a Mac, for a later step.** Recommended: one clone of the whole tree into the job's new home,
then one walk that gives every entry to the job's account. The source must be on the homes' volume.

- Options:
  - (i) this, candidate (b) of job-rootfs.md 5.3;
  - (ii) a union file system of our own, (a);
  - (iii) per-file links with copy on first write, (c).
- Recommendation: (i).
  - It is an ordinary tree, so every program sees ordinary semantics.
  - Nothing is mounted in the home, and nothing is installed.
  - The stand-in took 3.5 to 3.9 s for 46,362 files (section 11).
  - The walk is the existing `clone_writable` of `js-treestore`, extended to remove extended attributes (3.2).
  - A source on another volume would need a byte copy, a second path that no Mac needs, because each Mac has one data
    volume. Such a source is refused instead (section 7).
  - (ii) is the lazy file system of D1 (iii).
  - (iii) denies a program that opens an input for writing instead of giving it a copy (2.2).

## What changes for a user

Today a native job on a Mac (sandboxed under `sandbox-exec`, in `exec/` of its account's home) can have `READ_ONLY` and
`READ_WRITE` mounts of host paths, as links in `exec/` (job-rootfs.md 5.2). It is refused a mount whose source is a
provider (`input-not-allowed`, "a Mac is given no mount provider") and every `COPY_ON_WRITE` mount
(`rootfs-unsupported-here`).

- **Step 1:** a `READ_ONLY` mount may name a provider, as on Linux. It is a link to the provider's written tree (D2).
- **Step 1:** no job, and no process of the agent, reads a network volume. The agent refuses at start a mount root on
  one, and refuses a start whose source is on one (2.1).
- **A later step (D5):** a `COPY_ON_WRITE` mount at `/work` or below it, of a host path on the homes' volume or of a
  provider's tree, is a **private copy**.
  - It is made in the job's own new home before its first process, and owned by the job's account.
  - The job may change anything in it, and nothing it does reaches the source.
  - The copy goes when the account is deleted.

**Example, step 1.** A client that wants a tree from tierstore read-only in its working directory passes this file as
`--spec.mounts-file`. It uses the existing fields (`docs/job-surface.md`), with the job's other fields as for any native
Mac job:

```json
[
  {"provider": {"provider": "tierstore", "ref": "cas/l/<64 hex digits of the tree list>"}, "target": "/work/src"}
]
```

The mode is left out, so it is `MOUNT_MODE_READ_ONLY`. On a Mac the job finds the tree at `src` in its working
directory and cannot write it. The job requires `mount-provider.tierstore`, as on Linux, and the same file runs on a
Linux host.

**Example, a later step (D5), refused on a Mac in step 1** (`rootfs-unsupported-here`). A client that wants the tree as
its writable working directory, and a second tree read-only inside it, sends:

```json
[
  {"provider": {"provider": "tierstore", "ref": "cas/l/<64 hex digits of the tree list>"}, "target": "/work",
   "mode": "MOUNT_MODE_COPY_ON_WRITE"},
  {"provider": {"provider": "tierstore", "ref": "cas/l/<64 hex digits of the SDK's tree list>"}, "target": "/work/sdk"}
]
```

Once tierstore takes `COPY_ON_WRITE` (section 5, change 1), this runs on a Linux host. On a Mac after D5, the job starts
in a copy of the first tree, with the second as a read-only link at `sdk` inside the copy.

What does not change:

- Layers stay refused on a Mac (`rootfs-unsupported-here`; placement never sends them, job-rootfs.md 2.6).
- In step 1 the rules for mount targets do not change. In the later step they change in two ways only: a copy-on-write
  mount at `/work` or below it is taken, and a read-only or read-write mount may lie below a copy-on-write target
  (3.1).
- The Seatbelt view of the other mounts, and every Linux behaviour, stay the same.
- A job that names none of these runs as today.

Marks used below: *read* means read in code, a doc or a Mac; *measured* means measured on a Mac and cited; *trial* means
a pull request's run on a real Mac; *inferred* means reasoning.

## 1. What a Mac has today

- **macOS refused reads of a network volume to Apple's programs, as they were started in one measurement.**
  - *Reported*, not measured by this document's author: on a Mac Studio with macOS 27.0.1, on tierstore's loopback NFS
    mount, `ls`, `sh`, `shasum` and `python3` got `EPERM` for every user, from a shell and from a launchd job alike.
  - No request reached the file-system service. The privacy daemon (TCC) logged its Network Volumes service with
    "Platform binary prompting is Deny".
  - Its raw output is not reproduced here; this document's author did not run it and does not hold it. Which program
    started the readers, from which session, and whether disclaimed, are not recorded here.
  - A process holding the Network Volumes grant, and one started not disclaimed from such a process, were not tried.
    That is D1 (ii)'s probe.
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

## 2. The recommendation

### 2.1 Who reads a provider's tree, and the privacy control

| Process | What it reads | What it would need through the NFS mount |
|---|---|---|
| the provider (tierstore's cache, as `_tierstore`) | its own cache, on the local disk | nothing: it never reads its own mount |
| the agent (`js-agent`, root, a launchd daemon) | the source of a copy-on-write mount, to copy it (3.1) | a Network Volumes grant for its program, by a profile naming its signature |
| the run's supervisor (`js-agent __supervise-native`, root, one per run) | nothing of the tree: it starts the job in `exec/` | nothing |
| the job (its own account) | a `READ_ONLY` link's target | either a grant for every program it runs, which cannot be given (a program a build has just made has no stable signature, and a grant to `/bin/sh` would reach every job of the Mac); or one Network Volumes grant inherited from a signed starter that starts it not disclaimed: untested, D1 (ii), probe running |

So, as jobs are started today (disclaimed), no job can read through the mount. Whether a job started from a granted
starter can is D1 (ii)'s probe. Without (ii), keeping the mount would mean the agent copies every tree, the read-only
ones too. That copy would need a grant for the agent, a root daemon for the hard mount, and a full NFS read of every
tree, never a clone (`EXDEV`).

**Recommended for now, built in step 1, and the fallback if D1 (ii) fails (D2): no network volume on the path.**

- The provider writes each tree whole into a directory on the Mac's local disk, and answers `Mount` with it.
- The provider reads only its cache. The agent and the job read only local APFS files.
- The directory is beneath a root the operator lists with `--mount-root-ro`, on the homes' volume, so a copy of it is a
  clone (D5). For tierstore it is `/usr/local/tierstore/job-trees/<key>`.
- A `READ_ONLY` mount is the link of job-rootfs.md 5.2 to it. Its files are `_tierstore`'s, modes 0644 and 0755, so a
  job reads and cannot write them. Seatbelt grants the run that one directory and denies it the rest of the root
  (job-rootfs.md 5.4).
- No read of step 1 needs Full Disk Access (*trial*, with the check below).
  - Every reader reads a local directory that the privacy control does not protect: the provider's `job-trees`.
  - **The job's read (step 1).** The job's account reads `job-trees/<hex>`, which is the provider's, 0644 and 0755.
    *Trial*: in komira-ai/tierstore#430's trial on a Mac Studio, the unprivileged user `nobody` listed and read the
    tree and ran its script.
    - The trial installed no grant, to be confirmed by the checks below. #1004's profile names the root helper
      `js-mac-session` alone, by its path and signature, so even in force it could not cover tierstore or the programs
      `nobody` ran.
    - **The checks, run once on the trial's Mac, as root:**
      - **Profiles.** `profiles show -type configuration`. Every privacy payload
        (`com.apple.TCC.configuration-profile-policy`) must name only a specific program of ours, such as #1004's
        `js-mac-session` or hostkeeper's own program. None may name tierstore, `/bin/sh`, `/bin/ls`, a terminal or
        `sshd`, and none may use a wildcard or a broad identifier.
      - **Grants approved by hand.** These live in the system privacy database, not in a profile. Run
        `sqlite3 "/Library/Application Support/com.apple.TCC/TCC.db" "select client, auth_value from access where
        service = 'kTCCServiceSystemPolicyAllFiles' or service = 'kTCCServiceSystemPolicyNetworkVolumes'"`. It must list
        no row with `auth_value` 2 for tierstore, `sshd-keygen-wrapper` or `sshd`, a terminal, or the shell the trial
        ran from.
      - With both seen, the trial stands as evidence that a job's read of a local tree needs no grant, and nothing in
        this bullet is inferred.
  - **The read under Seatbelt (step 1).** komira-ai/jobscheduler#1006 has a macOS test under the real `sandbox-exec`
    profile: the run reads a tree through the read-only link, is refused a write there, and writes its put directory.
  - **The provider's own reads and writes.** *Trial*: the #430 trial ran tierstore as `_tierstore`, and it wrote the
    tree.
  - **No trial ran as root.** The local-tree trial ran as `_tierstore` and `nobody`. The put ingest's tests ran on a
    Mac as an ordinary user.
  - **The agent's copy, as root (a later step).** This is *inferred*, not tried.
    - Root reads files of any owner. The privacy control refuses root only in the places it protects.
    - The copy reads `job-trees/<hex>` and writes in a home the agent made, as it makes `exec/` today. Neither is
      such a place.
    - The probe that pins it: as root, from the signed agent's program without a grant, clone a tree of `job-trees`
      into a scratch home with `clone_writable` and walk it. Expect no `EPERM`, and no privacy daemon log line.
    - It is build item 6's first check.
  - A copy-on-write source that an operator's root puts under a protected folder (Desktop, Documents) would need a
    grant. komira-ai/jobscheduler#1004's profile (merged) names the root helper. After the one-program switch it names
    the agent's program, and once that profile is in force,
    that case needs nothing more.
  - Deleting an account's home and per-user folders does need the grant. That is komira-ai/jobscheduler#1004's grant,
    not this design's.

**The agent enforces the rule generically.**

- Every root of `--mount-root` and `--mount-root-ro` must be on a local volume (`statfs`, `MNT_LOCAL`), or the agent
  refuses to start, naming the flag.
- A source whose resolved directory is not on a local volume is refused `input-not-allowed` at the lookup.
- A configuration that no job could use therefore fails by name, once, instead of as `EPERM` inside each job.

### 2.2 Copy-on-write, a later step: candidate (b) recommended, the whole tree in one clone

A copy-on-write mount must give the job, at `exec/<rest>`, a tree that reads as the source. The job may create, change,
rename and delete anything in it. No change may reach the source. The tree is owned and writable by the job's account,
and goes with the run.

- **(b), recommended (D5, open for the owner).** One `clonefile(2)` of the source directory into the job's new home, then one walk over the copy
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

This flow is the later step's (D5). It assumes both rulings are built: one agent program, and the new account per job
of the later change that `docs/design/mac-account-per-job.md` designs (it merged with komira-ai/jobscheduler#1004).

Step 1 uses only these parts of it:

- the provider's answers (flow step 2);
- the lookup with the local-volume rule (flow step 3);
- the links (flow step 6).

Step 1's admission still refuses copy-on-write. Its start is today's: the helper lends a pool account and starts the
job. No account is made at assembly, and no copy is taken.

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
  job cancelled during its copy ends when the copy does: 3.5 to 3.9 s for 46,362 files, by the stand-in (section 11).

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

1. **Later (D5): accept `COPY_ON_WRITE` as it accepts `READ_ONLY`.**
   - The agent keeps the job's changes on every platform, so a provider of read-only trees serves the same tree.
   - This also fixes the same refusal on Linux.
   - No field changes.
2. **Step 1: on a Mac, answer `Mount` with a local directory that holds the whole tree** (D2; built in #430, except the
   space rules, which are D3 and later).
   - **Where:** `/usr/local/tierstore/job-trees/<key>`, one directory per tree key, shared by every source that names the
     key. The package's `postinstall` makes `job-trees` (`_tierstore`, 0755) beside the directories it makes today. Its
     parents are 0755. The name keeps it apart from the cache's own `trees/`.
   - **How it is written:** from the cache, on the same volume.
     - Step 1 (#430) copies every file from the cache through its span, with no platform-specific path.
     - Cloning a file that the cache holds as a whole object (`clonefile`) is a later speed-up.
     - Either way the file ends `_tierstore`'s, 0644 or 0755, with no extended attribute. A tree list names files, not
       inodes, so the written tree has no hard links.
     - The tree is written into `job-trees/.partial-<key>` and renamed to `job-trees/<key>` when whole, so no reader
       ever sees part of a tree.
   - **Answers and concurrency:**
     - `Prepare` answers at once and works in the background, as the protocol says.
     - There is one writer per key. A second `Prepare` of a key being written joins the write in progress.
     - `Mount` answers `UNAVAILABLE` until the rename, within tierstore's 25 s (the protocol allows 30), and the agent
       asks again until its `--mount-provider-wait` ends.
   - **Sharing and space** (the sharing is step 1's; the eviction and the budget are D3's, later):
     - A source holds its key's tree from `Prepare` to `Release` or `Sweep`, and never longer than the placement's life
       (`trees.ttl_max`, 24 hours), as a placement is held today.
     - A held tree is never evicted.
     - An unheld one stays until space is needed, oldest release first.
     - The written trees count at their logical size against `local_cache.budget`, which is a change to the cache's
       accounting: the directory is outside `local_cache.path` (D3).
   - **At the cap** (later, D3): when the trees held by sources fill the budget alone, `Mount` answers `UNAVAILABLE` ("no space").
     The agent's wait ends `input-unavailable`.
   - **Never** a directory of a network file system, which the agent refuses (2.1).

Rollout order:

- **Step 1:**
  1. tierstore's local trees (komira-ai/tierstore#430);
  2. the agent's read-only provider mount (komira-ai/jobscheduler#1006);
  3. each Mac's flags and tierstore file.
- **Later step (D5):**
  1. tierstore's change 1;
  2. the one-program switch, then accounts per job;
  3. the agent's copy-on-write item;
  4. its flags.

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

No new code. Every case has a named refusal; none gives a different answer silently. "Step" says when the case first
arises: in step 1, or only in the later copy-on-write step (D5).

| Case | Step | Learnt | Result |
|---|---|---|---|
| A mount root on a network volume | 1 | agent start | the agent does not start; the error names the flag and the volume |
| A homes' volume that ignores ownership | later | agent start | the agent does not start; the error names the volume |
| komira-ai/jobscheduler#1004's Full Disk Access not in force | 1 | admission | `no-full-disk-access`, that change's refusal of every native job, unchanged here |
| A provider the agent was not given | 1 | admission | `input-not-allowed`, as on Linux |
| A `READ_WRITE` mount of a provider | 1 | admission | `input-not-allowed` (komira-ai/jobscheduler#1006) |
| A `COPY_ON_WRITE` mount | 1 | admission | `rootfs-unsupported-here`, as today |
| A `READ_ONLY`/`READ_WRITE` mount at `/work` or outside it; a mount below another | 1 | admission | `mount-target-unsupported-here`, as today |
| A copy-on-write target outside `/work`; a mount below a link; a copy-on-write mount below another | later | admission | `mount-target-unsupported-here` |
| Layers | 1 | admission | `rootfs-unsupported-here`, unchanged |
| More mounts than `MAX_LISTED` | 1 | admission | `too-many-inputs-here`, unchanged |
| The provider refuses the ref or the mode | 1 | preparing | `provider-refused`, as on Linux |
| The provider's tree is not whole within `--mount-provider-wait` | 1 | preparing | `input-unavailable`, as on Linux |
| The trees cap with every tree held (D3) | later | preparing | `input-unavailable` once the wait ends |
| A source missing, not a directory, or beneath no root | 1 | preparing | `input-unavailable` / `input-not-allowed`, as job-rootfs.md section 8 |
| A source on a volume that is not local | 1 | preparing | `input-not-allowed`, naming the path and its file system |
| A copy-on-write source on a local volume other than the homes' | later | preparing | `input-not-allowed`, naming the path and both volumes |
| The account cannot be made at assembly | later | preparing | the start fails with the runtime's failure, naming the step |
| A read of the source fails during the copy, or the homes' volume is full | later | preparing | `input-unavailable`, naming the entry and the error |
| A hard link, FIFO, socket or device in a copy-on-write source, or nesting deeper than 64 | later | preparing | `invalid-mount`, naming the entry |
| The job's reads through a `READ_ONLY` link fail while it runs | 1 | the job | the job's own result, then the after-exit check (job-rootfs.md section 8) |

## 8. What a submitter sees on a Mac, and how it differs from Linux

The spec is the same on both platforms.

**In step 1:**

- **The start waits for the whole tree.** A provider's tree is written whole before the job's first process (D1).
  - By the stand-in of section 11, that is about 6 s for a warm tree of 46,362 files, the first time the Mac meets the
    tree.
  - Later jobs that name the same tree find it there and wait for nothing.
  - #430's real write is not yet timed.
- **The tree is read-only and the provider's.** Its directories are 0755, and its files 0755 with an execute bit or
  0644 without. setuid, setgid and sticky bits are dropped (komira-ai/tierstore#430). The job can read and run its
  files and cannot write them.
- **A copy-on-write mount is refused** (`rootfs-unsupported-here`). A job that must change its inputs copies them
  itself into its working directory.
- **Sources** must be on a local volume.

**In the later step (D5), additionally:**

- **A copy-on-write mount adds 3.5 to 3.9 s** for the same tree, by the stand-in. Making and deleting the job's account
  cost more on top; they are measured in the account-per-job design, not here.
- **A cancel during the copy waits for the copy** (3.4).
- **Modes in the copy.** Directories are 0700, and files 0600, or 0700 when the owner could execute them. setuid and
  setgid are dropped, and every entry is the job's account's. On Linux a writable tree keeps its modes, owned as its
  provider presents it (job-rootfs.md LR5).
- **Hard links, FIFOs, sockets, devices** and nesting deeper than 64 in a copy-on-write source are refused. A
  provider's written tree has none of them. On Linux an overlay keeps hard links one inode until written.
- **Renaming a directory** of the source succeeds. On Linux it is `EXDEV`.
- **Extended attributes, ACLs and file flags** are not in the copy.
- **A copy-on-write source** must be on the homes' volume.

**For the Linux behaviour, in either step:** a client that needs laziness, a writable tree, kept modes, hard links or
overlay semantics asks for `exec=container`. Placement then sends the job only to a host that runs containers, never to
a Mac's native agent. When no such host reports the provider's key, the submit is refused `no-capable-host`, as for
any constraint no host meets.

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

**Step 1's tests are in its own pull requests.** komira-ai/jobscheduler#1006 has the every-platform module
`native_fs` with its Linux tests, and the macOS tests of the flags, the hello and the read-only link under the real
`sandbox-exec` profile. komira-ai/tierstore#430 has the tests of the written tree, the shared and concurrent write,
a crash mid-write, the modes and the refusals. komira-ai/tierstore#431 has the put ingest's tests. The lists below give
each test's step: "1" means step 1 needs it and a step-1 pull request carries it or an equal; "later" means the
copy-on-write step (D5) brings it.

Every test below uses the repository's scratch helpers for its directories (T12). None touches the host's real homes,
`/usr/local/tierstore` or a service's directory, and the package test runs `postinstall` against a scratch target.

**Owner: jobscheduler, shared module (`js-agent`; every pull request, covered).** These run over the injected port:

| Test | Step | Asserts |
|---|---|---|
| `copy_on_write_target_outside_work_refused` | later | `mount-target-unsupported-here`, exact text, and `/work` itself taken for this mode only |
| `mount_below_a_link_and_copy_below_copy_still_refused` | later | `mount-target-unsupported-here`, exact text |
| `agent_refuses_a_mount_root_on_a_network_volume` | 1 | the start check for `--mount-root` and `--mount-root-ro`, exact message, and a local root taken |
| `agent_refuses_a_homes_volume_that_ignores_ownership` | later | the start check, exact message |
| `source_on_a_network_volume_is_input_not_allowed` | 1 | a host path, and a provider's directory, below a local root but on a network file system, exact detail |
| `copy_on_write_source_off_the_homes_volume_is_input_not_allowed` | later | exact detail, naming both volumes |
| `start_checks_run_before_the_account_is_made` | later | a start refused for its environment, working directory, Xcode or bundle makes no account and writes no ledger line |
| `journal_then_ledger_then_account_then_copy` | later | the order of step 4, and every entry of the copy is the account's |
| `run_without_copy_on_write_gets_its_account_at_the_start` | later | no `assemble_fs` account; `create_and_start` makes it through the same function |
| `first_ask_failure_is_given_again` | later | a copy that fails at the first ask fails every later ask of the same run |
| `copy_failure_is_named` | later | a refused entry gives `invalid-mount` naming it; `EIO`, `EACCES` and `ENOSPC` give `input-unavailable` with the exact detail |
| `run_never_started_has_its_account_and_copy_deleted` | later | `discard_fs` after a failed copy and after a cancel hands the deletion to the pool; a failed deletion holds the account |
| `discard_after_remove_does_nothing` | later | `discard_fs` of a run whose account `remove` deleted |
| `agent_start_keeps_an_adopted_runs_copy_and_deletes_the_rest` | later | an adopted run keeps its account and copy; a lease with an unjournalled live supervisor has it ended before its account goes; a lease the server holds is kept; an account whose run never started, and one cut off half-way, are deleted |
| `copy_at_work_is_the_working_directory` | later | the clone is `exec/` itself; a staged input or link whose name the copy holds fails the start |
| `link_below_a_copy_on_write_target_lands_in_the_copy` | later | the link is made after the copy, inside it |
| `provider_tree_not_whole_within_the_wait_is_input_unavailable` | 1 | `Mount` answering `UNAVAILABLE` past `--mount-provider-wait` |

**Owner: jobscheduler, placement (`js-lease-server`; every pull request), later:**
`copy_on_write_provider_job_goes_only_to_a_host_reporting_the_key`, in `agent_api_job_cases.rs`.

**Owner: jobscheduler, macOS (`js-agent`, `js-treestore`; the beta stage now, every pull request once the Macs are on
the farm).** These are the macOS-only parts, behind `cfg(target_os = "macos")`:

- `js-agent`, step 1, as komira-ai/jobscheduler#1006 names them:
  - `platform::macos::tests::a_native_hello_reports_each_provider_it_was_given`;
  - `native::mounts::what_a_mac_cannot_give_is_refused_with_its_own_code`, which admits a read-only provider mount and
    refuses a read-write one;
  - `native::mounts::a_missing_put_directory_is_made_at_the_start_and_granted_written`;
  - `native::mac::a_job_reads_a_providers_tree_and_writes_its_put_directory_under_the_real_profile`.
- `js-agent`, step 1, on Linux, also in #1006:
  - `native_fs::tests::a_providers_tree_is_admitted_read_only_only`;
  - `native_fs::tests::a_mounts_directory_must_be_on_a_local_volume`;
  - `native_fs::tests::the_agents_mount_roots_must_be_on_local_volumes`;
  - the put directory's tests.
- `js-agent`, later:
  - `copy_on_write_is_planned_not_refused`;
  - `copy_is_not_in_the_read_only_list`, with the profile text asserted whole;
  - `no_exec_copy_is_denied_exec`.
- `js-treestore`, later, `clone_writable` with the real `clonefile`:
  - `clone_writable_removes_extended_attributes`, with a real quarantine attribute;
  - `clone_writable_keeps_link_text_and_modification_times`;
  - the existing tests of its modes, owner, flags, ACL and refusals of hard links, devices, pipes and depth, which stay
    as they are.
- Later, the semantics test, as a `sandbox-exec` behaviour test: a program that opens an input read-write and changes it,
  truncates and appends, renames a file and a directory, deletes both, changes a mode and `mmap`s a file for writing.
  The source is unchanged after.

**Owner: tierstore (its repository, tierstore's maintainers):**

- Step 1, as komira-ai/tierstore#430 names them, among its others:
  - `a_tree_is_hydrated_from_the_cache_into_plain_files_equal_to_its_list`;
  - `concurrent_prepares_of_one_key_write_once_and_mount_waits_for_the_whole_tree`;
  - `a_finished_tree_is_renamed_whole_unless_one_is_there`;
  - `written_files_are_0755_with_an_execute_bit_and_0644_without`;
  - the package test that the `postinstall` makes `job-trees` with its owner and mode, against a scratch target.
- Later:
  - `prepare_accepts_copy_on_write_as_read_only`;
  - `prepare_still_refuses_read_write`, with the reason asserted by exact equality;
  - `whole_objects_are_cloned_and_spans_copied` (#430 copies every file; the clone is a later speed-up);
  - `a_held_tree_is_never_evicted_and_unheld_trees_go_oldest_release_first`;
  - `written_trees_count_at_logical_size_in_the_budget`;
  - `mount_at_the_cap_with_every_tree_held_is_unavailable_no_space`.

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

Step 1 is items 3 and 5 alone, and is being built:

- item 3 is komira-ai/jobscheduler#1006, on the agent as it is today, before the one-program switch;
- item 5, without its `COPY_ON_WRITE` change and its budget, is komira-ai/tierstore#430.

The outputs, which are not this design's, are komira-ai/tierstore#431, merged. The later step takes the rest.

One subject per pull request, in this order. Item 4 comes after the one-program switch (the one-program design, section
7) and after accounts per job (the account-per-job design, step 2), whose account it makes at assembly.

1. **The shared module, later** (`js-agent`): the admission rules of `native/mounts.rs` `check`, moved into it
   unchanged; the port; and the shared-module tests. No behaviour change.
   - #1006 does not move them. It adds the every-platform `native_fs` module for the provider-mode, local-volume and
     put-directory rules, which this item extends.
2. **`clone_writable` removes extended attributes** (`js-treestore`), with its macOS tests.
3. **Providers on a Mac** (`js-agent`):
   - the three flags on macOS, and `getpwnam_r` for a user named by name;
   - `Jobs::providers` from the flags, and the hello key moved to the shared contract;
   - the local-volume rule;
   - `plan_of` taking a provider's directory;
   - a missing put directory (a `READ_WRITE` host path) made at the start below its writable root;
   - the every-platform module `native_fs`.

   This item is komira-ai/jobscheduler#1006, open, built as the PR describes.

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

## 11. Measured with a stand-in (Python, unprivileged), the real trials, and the bound past which this design is wrong

**The stand-in.** *Measured* on two idle Macs (macOS 27.0.1 and 26.4), as an unprivileged user, by a Python stand-in,
not by tierstore or the agent. The tree was synthetic but of a large build's shape: 46,362 files in 12,394 directories, 463 MB of data, one
file in ten executable. The source was in the page cache. Three runs on each Mac:

| Step | 27.0.1 | 26.4 |
|---|---|---|
| the provider's write, stood in for by a byte copy of every file (read, write, mode, times) | 5.5 to 6.0 s | 5.3 to 6.6 s |
| the same with `cp -R` | 8.0 to 8.6 s | 7.1 to 8.5 s |
| one `clonefile` of the whole tree | 0.5 s | 0.8 to 0.9 s |
| the walk of 3.2 over the clone (`lchown` to the same user, mode, flags, attribute check; 58,755 entries, the root excluded) | 3.0 to 3.1 s | 2.8 to 3.0 s |
| one `clonefile` per file, for comparison | 6.7 s | 7.5 to 7.6 s |
| removing a cloned tree | 3.1 to 3.2 s | 4.2 to 5.4 s |

**The real trials.** They checked behaviour, not time; no real timing exists yet.

- komira-ai/tierstore#430, on a Mac Studio: the darwin build ran as `_tierstore` and answered two sources of one tree
  the same `job-trees/<hex>`. The tree held what its list holds, with the modes of section 8. The unprivileged `nobody`
  read and ran it and could not write in it, and no partial directory was left.
- komira-ai/tierstore#431, merged, outside this design: its tests ran on a Mac with the real FSEvents watcher, and none
  failed.
  A by-hand trial on a Mac Studio, reported with that work but not in #431's text, stored and read back a pack that an
  unprivileged user committed. Its proof against a storage node waits for a write key.
- komira-ai/jobscheduler#1006, on an idle Mac Studio: under the real `sandbox-exec` profile, a run read a tree through
  the read-only link, was refused a write there, and wrote its put directory. No timing was taken.
- The time of #430's real write, and of the later step's copy as root to another account, are build item 6. Neither
  could be timed here: the measuring user had no root.

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
NFS mount. Under the local trees no job and no process of the agent reads that mount, so the answer cannot change
them. For D1 (ii) it is that probe's question.
