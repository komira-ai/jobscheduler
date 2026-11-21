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

- a `READ_ONLY` or `READ_WRITE` mount may name a provider, as on Linux. It is the same link to the directory the
  provider answers, and its reads stay lazy;
- a `COPY_ON_WRITE` mount at `/work` or below it, of a host path or of a provider's tree, is a **private copy** of the
  tree. The agent makes it before the job's first process, in the job's own home, and the job's user owns it. The job
  may change anything in it. Nothing it does reaches the source. The copy goes with the home.

What does not change: layers stay refused on a Mac (`rootfs-unsupported-here`, and placement never sends them,
job-rootfs.md 2.6). The rules for mount targets change in two ways only: a copy-on-write mount at `/work` or below it
is taken, and a read-only or read-write mount may lie below a copy-on-write target (3.1). The Seatbelt view of the other mounts and every
Linux behaviour stay the same. A job that names none of these runs as today.

Marks used below: *read* means read in code, a doc or a Mac; *measured* means measured on a Mac and cited; *inferred*
means reasoning.

## 1. What a Mac has today

- **tierstore on a Mac serves read-only trees only.**
  - Its provider accepts `MOUNT_MODE_READ_ONLY` and `UNSPECIFIED`. It refuses every other mode `FAILED_PRECONDITION`
    with "tierstore makes read-only trees" (*read*: komira-ai/tierstore `crates/tierstore/src/server/provider/sources.rs`).
  - A tree is a directory `<mountpoint>/mount-provider/<agent>/<run>/<index>` of its NFSv3 loopback mount, the agent
    and the run in hex (`provider/names.rs`).
  - The mount is `vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard` (*read*:
    `crates/tierstore/src/server/treefs/nfs/mount.rs`). It is hard: a read waits for the server for good.
  - Every procedure that would change the tree answers `NFS3ERR_ROFS` (`nfs/fs.rs`). NFSv3 has no procedure for
    extended attributes, so the tree has none.
  - Its owner decided that a job's writable directory is the client's, not the store's (tierstore
    `docs/design/virtual-build-fs.md`, decision C9). tierstore will build no writable view on macOS.
- **The OS has no union mount to build on.**
  - `MNT_UNION` gives lookup fallthrough only: no merged listing, no copy-up, no delete of a lower file.
  - A second NFS server of ours with tierstore below it measured 1.6 times slower on a cold `stat` and 25 times with
    eight threads, 1.8 times for `clang` and 1.5 times for `cargo` (*measured*: virtual-build-fs.md section 8, macos
    E18c). `MNT_UNION` is in its section 4.2.
  - The Macs have no third-party FSKit module, no FUSE-T and no macFUSE, and 0 system extensions (*read*:
    `systemextensionsctl list`, `pluginkit -m -p com.apple.fskit.fsmodule` and `/Library/Filesystems` on a macOS 26.4
    and a 27.0.1 host).
- **Clones do not cross into a lazy tree.**
  - `clonefile(2)` from the NFS mount to APFS is `EXDEV`, and `cp -c` silently copies the bytes instead (*measured*:
    macos E18).
  - Within one APFS volume a clone takes 8 to 12 µs per file: 1.5 s for 190,000 files (*measured*: macos E3,
    virtual-build-fs.md section 1).
  - `/Users`, `/private/var`, `/usr/local` and `/tmp` are one APFS volume on these hosts (*read*: `stat -f %d`).
- **The native runtime already has the hooks a copy needs.**
  - `Runtime::resolve_fs` and `Runtime::assemble_fs` run off the lifecycle's worker and answer `Pending` until they are
    done (`crates/js-agent/src/lifecycle/runtime.rs`).
  - The native runtime implements the first. It answers `Ready` at once, after `canonicalize` calls made on the worker
    (`native/mod.rs`, `mounts::resolve`), and it has the default of the second.
  - The lifecycle waits for an assembly under way rather than discard it (runtime.rs, `discard_fs`;
    `lifecycle/machine/prepare.rs`, `ending`). `assemble_fs` is given no stop.
  - `discard_fs` is called at the cleanup of every run that reached the preparing stage, one that ran included
    (`lifecycle/machine/cleanup.rs`).
  - The pool user and its home are made only in `create_and_start` (`helper.user_create`, then `launch`). The uid
    that will own the copy is therefore not known while the start is preparing.
- **The provider client is not tied to Linux.**
  - `crate::provider`, `lifecycle/mount_providers.rs` and the lifecycle's `providing` stage have no `cfg` gate.
  - Only the three flags are Linux-only (`platform/mod.rs`, `LINUX_ONLY`; declared in `platform/linux.rs`).
  - So is the hello key `mount-provider.<name>`, reported from `contract/container.rs`.
  - On a Mac, `Jobs::providers` answers none (`platform/macos.rs`).
  - The lifecycle sends a mount's own mode in `Prepare` (`providing.rs`), so a `COPY_ON_WRITE` mount of a tierstore
    tree is refused `provider-refused` today on Linux as well.

## 2. The decision

**Candidate (b) of job-rootfs.md 5.3, made per file: a private copy, built by the agent off the worker.**

- A `READ_ONLY` or `READ_WRITE` mount stays the lazy link of job-rootfs.md 5.2, whatever its source.
- A `COPY_ON_WRITE` mount is copied whole into the run's home before the job runs. Each regular file is cloned when the
  source is on the volume of the homes, and its bytes are copied otherwise. A provider's tree is always on another
  file system (NFS), so its files are always copied.

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
   - (b) gives it up for copy-on-write mounts only: the copy reads every file once.
   - `READ_ONLY` mounts keep it, and they are what tierstore's owner designed the Mac around (C9: the client makes its
     own writable directory).
   - A client that wants a lazy writable tree on a Mac mounts the tree `READ_ONLY` and makes its writable directory
     itself, as virtual-build-fs.md describes. That is (c), done by the client, which knows which files it writes.
3. **Cost.**
   - A host-path tree on the homes' volume costs a clone per file (1.5 s for 190,000 files, measured).
   - A provider's tree costs a full read through the NFS mount. That is the one cost not yet measured on a real
     `tierstore fs` mount (section 11, build item 5). It changes when the design is worth using for a large tree, not the mechanism.
4. **Teardown.**
   - Nothing is mounted in the home, so `user-delete`, which refuses to cross a mount (`docs/design/mac-lease-teardown.md`),
     and `kill-uid` stay as they are.
   - No process of the job ever blocks in the provider's mount because of the copy. Only the agent's copy threads read
     it, and 3.4 bounds them.
   - (a) needs an unmount before `user-delete`, and a way to end processes blocked mid-read in a hard mount. Those stay
     in state `U` until a forced unmount (*measured*: macos E16).
5. **Fleet.**
   - Nothing is installed or approved for (b): `clonefile`, `read`/`write` and `lchown` are the OS's own.
   - (a) needs a signed FSKit module enabled on every Mac, or FUSE-T, whose licence is not permissive.

Rejected besides:

- **A provider-made writable tree** (the provider answers `COPY_ON_WRITE` with a per-run directory it fills): it
  contradicts tierstore's decision C9. A tree file is a span of a cached pack, not a whole file, so it could not be
  cloned out of the cache. The protocol also carries no uid to give the tree to. If section 11's measurement shows the
  copy too slow for the trees clients send, this is the next step. It is an additive field of `PrepareRequest`, and it
  reopens C9 with tierstore's owner.
- **Hard links into the source**: a write through one changes the source's inode.
- **Sending `READ_ONLY` to the provider for a copy-on-write mount**: the agent would tell a provider something other
  than what the job asked, on one platform only. Section 5 changes the provider instead.

## 3. The mechanism

### 3.1 Where each step runs

1. **Admission** (`native/mounts.rs`, `check`, no I/O). A `COPY_ON_WRITE` mount is no longer refused for its mode. Its
   target must be `/work` or strictly below it (`mount-target-unsupported-here` otherwise). Nesting is refused
   `mount-target-unsupported-here`, as now, with one change: a `READ_ONLY` or `READ_WRITE` mount may lie below a
   `COPY_ON_WRITE` target, because the copy is a real directory of the home and the link is made inside it after the
   copy (step 4). A mount below a link, and a copy-on-write mount below another copy-on-write mount, stay refused. A
   `READ_ONLY` or `READ_WRITE` mount at `/work` itself stays refused. Every mount counts toward `MAX_LISTED`, as now
   (`fs.mounts.len()`).
2. **Lookup** (`resolve_fs`). The host path, or the directory the provider answered, is canonicalised and must be a
   directory beneath a root of the agent. A copy-on-write mount needs only a read-only root (`--mount-root-ro`), as on
   Linux. A provider's source is resolved at the ask whose `fs` carries the provider's directory. The first ask carries
   only the host paths (`lifecycle/job_fs.rs`, `JobFs::without_providers`). The lookups move off the worker onto the
   pool of step 3, and the ask answers `Pending` until they end. A lookup unanswered for 60 s fails
   `input-unavailable`, as on Linux (job-rootfs.md 4.7): a `canonicalize` through a hung hard mount must not hold the
   worker, and with it every other job of the host.
3. **Copy** (`assemble_fs`, new for the native runtime). The first ask starts one copy per copy-on-write mount on a
   pool of 4 threads of the agent's own (`js-copies`), and the ask answers `Pending` until every copy has ended. The
   lifecycle ignores the first ask's answer and asks again (`prepare.rs`), so a failure is kept by run id and given at
   every later ask. The state of a run's copies is kept by run id until `discard_fs`.
   - Each copy builds the tree in `<native-homes>/.js-agent-copies/<run>/<index>`. Its parent is made by the agent:
     root's, mode 0700, never through a link.
   - The parent is in the directory of the homes, so step 4's rename is one metadata operation. The agent refuses to
     start when `.js-agent-copies` and `--native-homes` have different `st_dev` (a volume mounted at that name).
     No code lists the homes directory, so the extra entry meets no sweep. No run can read it: its user is not root,
     and the parent is 0700.
4. **Placing** (`launch`, after `exec/` and the staged inputs are made, before `link_all`):
   - Each staged tree is renamed to `exec/<rest>`. A copy at `/work` itself moves each top-level entry of the staged
     tree into `exec/`.
   - Each entry is then given to the run's user with the existing hand-over (`HandOver`, `lchown`, never through a
     link).
   - A name that is there already fails the start, as `link_all` does.
   - The links of the other mounts are made after the copies, so a link below a copy-on-write target lands in the
     copy.

### 3.2 What the copy makes

The walk is one function over an injected file-system port. It lives in a module of `js-agent` that is built on every
platform, so its rules are tested on Linux, where the pull request measures coverage. Only the system calls are behind
the port's macOS side: `clonefile` and the hand-over. The macOS side clones a file with the `Cloner` of `js-treestore`
(`mac/seal.rs`, `clone_file`; `js-agent` depends on the crate already) rather than add a second binding. It does not
use `Sealer::clone_writable`, which clones a whole tree, keeps flags, and refuses links and hard links.

| Source entry | Copy |
|---|---|
| directory | a directory, mode 0700 |
| regular file | a clone (`clonefile`, no follow) when `statfs` of the source root and of the staging parent name one volume; else a byte copy. Mode 0700 if any execute bit was set, else 0600. setuid and setgid are dropped |
| symbolic link | a link with the same text, never followed |
| hard links (one inode, two names) | two independent files |
| FIFO, socket, device | refused `invalid-mount`, naming the entry |
| two names that differ only in case | refused `invalid-mount`, naming both. The homes' volume ignores case, and the source (NFS, or a case-sensitive volume) may not |
| modification time | kept, for files and directories, so tools that compare times (make, ninja, cargo) see the source's order |
| extended attributes, ACLs, file flags | not copied. tierstore serves none, and a tree a job may change keeps none of its source's quarantine or provenance marks |

The modes follow the ones the bundle store's writable clones already use (`js-treestore`, `clone_writable`). The
staged tree is root's until step 4 gives it away.

### 3.3 The Seatbelt view

- The copy is in the run's home, which the profile lets the run write already. It is not in the run's read-only list,
  which keeps the files, the staged inputs and the links.
- No read grant of the source is emitted, because the run never reads the source. The source's root stays a read deny
  of every run (job-rootfs.md 5.4).
- A `no_exec` copy-on-write mount is a `MountGrant` of the copy's path, read-only, with `no_exec`. It emits the
  `(deny process-exec* ...)` and `(deny file-map-executable ...)` of a `no_exec` link. It also emits that grant's read
  and ancestor-metadata allows of a path in the run's own home, which the run may read already, so they change
  nothing. No new policy item is needed.

### 3.4 How long a copy may take

- A copy that makes no progress for 60 s fails the start `input-unavailable`. Progress is one entry made, counted on
  the lifecycle's `now_ms`, which `assemble_fs` is given. The figure matches the 60 s bound of a host path's lookup
  (job-rootfs.md section 8).
- There is no bound on the total: the server's start timeout bounds the whole preparing stage from placement (900 s by
  default, `crates/js-lease-server/src/job_expiry.rs`). The bound is a constant, not a flag (B20).
- A copy thread blocked mid-read in a hard NFS mount cannot be interrupted (macos E16). The start fails by the bound.
  The thread is left. When its read returns, it sees its copy given up and removes its staged tree.
- While all 4 threads are blocked, later lookups and copies make no progress and fail the same way. The worker is not
  held, so the host's jobs without such mounts run.
- **A stop waits for the copy.** The lifecycle waits for an assembly under way, and `assemble_fs` is given no stop. A
  job cancelled while its copy runs therefore ends when the copy ends, fails, or makes no progress for 60 s. The cost
  is the copy's time, which build item 5 measures. If it proves long, the next step is a stop the lifecycle gives the
  runtime's assembly, a change to `Runtime` that this design does not take now (B20).
- A provider whose mount hangs is the provider's to fix: a forced unmount ends the reads (macos E16). The agent keeps
  no gate or counter for it.

### 3.5 Cleanup

- A start that ends before step 4 (refused, cancelled, failed) has its staged trees removed by `discard_fs`, on the
  copy pool, once the assembly has answered (3.4). A copy given up by the bound removes its own tree when its thread
  returns. `discard_fs` also removes the run's empty staging directory after step 4 has moved the trees out.
- After step 4 the copy is in the home and goes with it at `user-delete`, as the links do. That includes a copy that
  failed half-way through the hand-over, which is root's in part.
- An agent that starts removes everything under `<native-homes>/.js-agent-copies`, on the copy pool. No start is
  adopted before its copy is placed.
- A provider's source is released as on Linux: after `kill-uid` and before `user-delete`. The copy no longer needs
  the source once it is placed. Releasing it earlier is an optimisation this design does not take.

## 4. Flags

No new flag. On a Mac:

| Flag | Change |
|---|---|
| `--mount-provider NAME=SOCKET` | taken on macOS, with the Linux meaning (`crates/js-agent/src/lib.rs`); leaves `LINUX_ONLY` |
| `--mount-provider-user NAME=USER` | taken on macOS. A user named by name is looked up with `getpwnam_r` (Directory Services), as `js-mac-session` looks up its users: a Mac's service accounts are not in `/etc/passwd`, and tierstore's `_tierstore` gets the highest free id from 399 down to 300, so its uid differs from Mac to Mac |
| `--mount-provider-wait SECS` | taken on macOS, unchanged (1 to 900, 120 by default) |
| `--mount-root-ro NAME=PATH` | unchanged. The operator lists the provider's mount as a read-only root, as on Linux, and it is a read deny of every run (job-rootfs.md 5.4) |
| `--native-homes PATH` | unchanged. It also holds the agent's `.js-agent-copies` |

The socket check needs nothing new. tierstore's socket on a Mac is `_tierstore`'s, mode 0600, in a directory of
`_tierstore` with mode 0700 (*read*: komira-ai/tierstore `packaging/macos/README.md`), which the check takes
(`crates/js-agent/src/provider/socket.rs`).

## 5. What the provider side must do

These are requirements of the provider interface, stated for any provider; tierstore is the one there is.

1. **Accept `COPY_ON_WRITE` as it accepts `READ_ONLY`.** The agent keeps the job's changes on every platform: an
   overlay upper on Linux, the copy on a Mac. A provider that makes read-only trees therefore serves a copy-on-write
   mount with the same tree. This fixes the same refusal on Linux. It changes no field: the value exists, and the
   proto's comment ("its changes are its own and never reach the tree") already says it. Build item 4.
2. **Serve reads by root and by the run users.**
   - The agent's copy reads the tree as root, and a job reads a `READ_ONLY` or `READ_WRITE` link as its pool user.
   - tierstore's NFS frontend serves only the uids of `fs.nfs.allowed_uids` (its mount user and its owner by default)
     and refuses the rest `NFS3ERR_ACCES`.
   - A Mac's configuration lists uid 0 and the pool's uids.
   - Until it does, a copy fails `input-unavailable` and a job's read through a link fails `EACCES`.
3. **Have the tree's directory there.**
   - The macOS package of tierstore installs and starts the cache alone (`packaging/macos-pkg/tierstore.plist`). The
     cache answers `Mount` with a directory of `tierstore fs`'s mount (`/private/var/tierstore/fs`).
   - A Mac therefore also runs `tierstore fs`, root's LaunchDaemon (`packaging/hostkeeper/units/tierstore-fs.plist`),
     before a provider's tree can be opened.
   - Until it does, the agent's lookup finds no directory and fails the start `input-unavailable`.

Rollout order: the provider side (1 to 3), then the agent's build items, then each Mac's flags. An agent of the
previous release still refuses each of these mounts by name at start (`rootfs-unsupported-here`,
`input-not-allowed`). Placement does not filter it out, as today.

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
| A provider the agent was not given | admission | `input-not-allowed`, as on Linux |
| A copy-on-write target outside `/work`; a `READ_ONLY`/`READ_WRITE` mount at `/work`; a mount below a link; a copy-on-write mount below another | admission | `mount-target-unsupported-here` |
| Layers | admission | `rootfs-unsupported-here`, unchanged |
| More profile entries than `MAX_LISTED` | admission | `too-many-inputs-here`, unchanged |
| The source, or the provider's directory, missing, not a directory, or beneath no root | preparing | `input-unavailable` / `input-not-allowed`, as job-rootfs.md section 8 |
| The provider refuses the ref or the mode (a provider before section 5's change 1, for a copy-on-write mount) | preparing | `provider-refused`, as on Linux |
| A read of the source fails during the copy (`EIO`, `ESTALE`, `EACCES` from the provider's uid fence), or the homes' volume is full | preparing | `input-unavailable`, naming the entry and the error |
| A copy makes no progress for 60 s | preparing | `input-unavailable` |
| A FIFO, socket or device in the source; two names that differ only in case | preparing | `invalid-mount`, naming the entries |
| The rename into `exec/` fails (another volume under the homes) | creating | the start fails with the runtime's failure, naming the path: the host's configuration |
| The job's reads through a `READ_ONLY` link fail while it runs | the job | the job's own result, then the after-exit check (job-rootfs.md section 8) |

## 8. Differences from Linux a client may meet

The spec is the same on both platforms. A client that relies on any of these runs only in a container:

- **Laziness.** On a Mac a copy-on-write mount reads the whole tree before the job's first process.
- **Hard links** in the source are separate files in the copy. On Linux they stay one inode until written.
- **Renaming a directory** of the source succeeds on a Mac. On Linux it is `EXDEV`.
- **Case.** Two names that differ only in case are refused on a Mac.
- **Ownership.** The copy is the run's user's on a Mac. On Linux a writable tree is presented by its provider owned by
  the job's container identity (job-rootfs.md LR5).
- **Extended attributes** are not copied.

## 9. Tests

Each is a Rust behaviour test. None asserts a time: progress and the bound run on an injected clock, and the copy pool
is driven step by step.

**Shared module (Linux pull request check, covered):**

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
| `copy_read_error_is_input_unavailable_naming_the_entry` | `EIO`, `ESTALE` and `EACCES` each give the exact detail |
| `copy_without_progress_for_60_s_fails_and_at_59_s_waits` | the bound, one step inside and one past it |
| `copy_discarded_removes_its_staged_tree` | `discard_fs` after a failed copy and after a placed one, and a copy whose read returns after the bound failed the start |
| `first_ask_failure_is_given_again` | a copy that fails at the first ask fails every later ask of the same run |
| `lookup_without_answer_for_60_s_fails_and_does_not_hold_the_worker` | a blocked `canonicalize` answers `Pending`, then `input-unavailable` past the bound |
| `agent_refuses_a_copies_directory_on_another_device` | the start check, exact message |
| `staged_copies_removed_when_the_agent_starts` | leftovers of a previous process are removed, others' files are not |
| `copy_at_work_fills_the_working_directory` | each top-level entry is in `exec/`, and a name already there fails |
| `link_below_a_copy_on_write_target_lands_in_the_copy` | the link is made after the copy, inside it |
| `mount_below_a_link_and_copy_below_copy_still_refused` | `mount-target-unsupported-here`, exact text |

**Platform and placement (Linux pull request check):** `macos_takes_the_provider_flags`, which holds `LINUX_ONLY` and
the macOS flag list to clap; `native_hello_reports_the_provider_key`; `native_start_naming_an_unconfigured_provider_is_input_not_allowed`;
and, in `agent_api_job_cases.rs`, `copy_on_write_provider_job_goes_only_to_a_host_reporting_the_key`.

**macOS (the beta stage's tests on Apple silicon, never a pull request job, B9-linux):**
`copy_on_write_is_planned_not_refused`; `copy_is_not_in_the_read_only_list`, with the profile text asserted whole;
`no_exec_copy_is_denied_exec`; `same_volume_copy_is_a_clone`, which shares blocks (`df` unchanged); and job-rootfs.md
5.3 test 1 as a `sandbox-exec` behaviour test, run against a host-path tree.

**tierstore (its repository):** `prepare_accepts_copy_on_write_as_read_only` and `prepare_still_refuses_read_write`,
the latter with the reason asserted by exact equality.

**Why this list covers the failure and boundary cases.** It takes each row of section 7. It takes each entry kind of
the table in 3.2. It takes each side of the progress bound. It takes each place a copy ends: placed, cancelled before,
cancelled during, failed, and found by a new agent. It takes each nesting of the new mount below another. It takes the
one compatibility edge: a previous-release agent's refusal, which is unchanged and already tested.

## 10. Build items

One subject per pull request, in this order:

1. **The copy module** (`js-agent`, shared): the walk of 3.2 over its port, the progress bound, cancel, and the
   shared-module tests. No behaviour change.
2. **Providers on a Mac** (`js-agent`): the three flags on macOS, `getpwnam_r` for a user named by name,
   `Jobs::providers` from the flags, the hello key moved to the shared contract, the lookups on the pool with their
   bound, and `plan_of` taking a provider's directory. The "No provider reaches a Mac" text of `native/mounts.rs` and
   the "Linux build only" of the three flags in `js-agent`'s rustdoc flag table go. It gives lazy `READ_ONLY` and
   `READ_WRITE` provider mounts.
3. **Copy-on-write on a Mac** (`js-agent`, `js-mac-pool`): `check`, `assemble_fs` driving the pool, the staging
   parent and its start check, placing and hand-over in `launch`, `discard_fs`, the sweep at start, and the `no_exec`
   grant of 3.3. The module docs of `native/mounts.rs` lose "its mechanism is not chosen".
   job-rootfs.md 5.1's row changes with it, and `docs/job-surface.md` gains the differences of section 8 (T19).
4. **tierstore** (komira-ai/tierstore, before 3 is used): `Prepare` accepts `COPY_ON_WRITE`, with its docs; and the
   macOS package documents `tierstore fs` and `fs.nfs.allowed_uids` for a Mac with js-agent (section 5).
5. **A measurement on one Mac** (a runbook under `docs/runbooks/`, not a test of the pull request set): copy time of a
   provider's tree of 46,362 files through a real `tierstore fs` mount, cold and warm, with 1 and 4 threads; and the
   hand-over's time for the same tree. Run by the owner on an idle Mac.

## 11. What is still measured, and what would change the decision

- **The copy time through the provider's mount** (build item 5) decides whether a copy-on-write mount of a large
  provider tree is worth sending to a Mac. It does not change the mechanism.
  - If the copy takes longer than a job's own run would gain from a Mac, clients send `READ_ONLY` and make their own
    writable directory.
  - If clients need a lazy writable tree anyway, the provider-made tree of section 2 is the next design. It is an
    additive field, and it reopens tierstore's C9.
- **Whether a hard NFS mount answers root's reads** through `allowed_uids` as it answers the mount user's (*inferred*
  from `nfs/fs.rs`, `authorize`: the fence compares the `AUTH_SYS` uid only).
- **The hand-over's cost** on the worker for a large tree. If it is large, the hand-over moves into the copy, which
  then needs the run's uid before the user is made (a helper change).
