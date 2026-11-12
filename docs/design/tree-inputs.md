# Tree inputs: a job's inputs read from tierstore's filesystem

**Superseded by [job-rootfs.md](job-rootfs.md) (D66).** Jobscheduler names no node-local cache: a job's inputs are
layers, mounts and host ports that any provider can serve, and the wire this document added is retired as job-rootfs.md
section 6 says. Kept as the record of the earlier design; nothing below is to be built.

Status before it was superseded: **design for the owner's review.** Only build item 1 (section 9) was built, as
komira-ai/jobscheduler#842. Written against `main` of this repository at
`0c104a1d`, and against tierstore's design `docs/design/virtual-build-fs.md` as merged in komira-ai/tierstore#274
(called "the tierstore design" below, its sections cited as "tierstore 6.7"). That design asks for this one in its
section 6.7 and lists it as item 9 of its phase 1 plan (tierstore 12).

**Amended** to close five gaps found when kbf on jobscheduler and tierstore was looked at end to end. The amendments
read tierstore's `main` at `37ffa7d7` for its control socket, cache and pins, which its operations guide documents as
built so far; the macOS sections (4) were not re-read, although tierstore has since chosen its macOS frontend (C9), and
build item 5 revisits them. Five changes, each marked where it is made, and each placed in a build item of section 9:
**A1** an attach that prefetches the tree inside the preparing stage (3.3); **A2** a tree is attached only within the
submitting client's namespace (2.6); **A3** the agent ends the fastest-growing job when the scratch volume falls below
its low-water mark (6.1); **A4** a tree's size is declared, reserved at attach and refused there when the host has no
room, never failed mid-job for want of pins or table room (2.7); **A5** a job may name hosts it must not be placed on
(2.8). Every new public field, value and refusal is listed in 7.1.

**What changes for a user.** Today a job that needs a tree of inputs (a build action's input root) fetches it itself:
kbf's task tool downloads every blob over HTTPS and lays the tree out in the container's scratch before the action runs
(`kbf-on-js-agent.md`, S4). After this design, the job names the tree by its digest when it is submitted; the host's
agent asks the node's `tierstore fs` to attach it, and the job finds it at the path it asked for, read lazily: only the
files the action opens are fetched, nothing is copied, and the action may write anywhere in a writable tree without the
write reaching the tree. What does not change: a job without tree inputs runs as today, on every host; a host without
`tierstore fs` reports nothing new and gets no tree job; the per-job disk position of `jobscheduler-design.md` (5.7:
no disk is booked per job on the farm, the node's tierstore is its cache) stands.

Contents: 1 what tierstore offers; 2 the wire; 3 the agent on Linux; 4 the agent on macOS; 5 failures; 6 disk; 7
compatibility; 8 tests; 9 build items; 10 decisions; 11 open questions; 12 what this asks of tierstore.

## 1. What tierstore offers, and what this design takes from it

From the tierstore design, read at the merged commit:

- **One mount per node**, made at boot before the job agent starts. Linux: `/run/tierstore/fs`, a FUSE mount with
  `allow_other` and `default_permissions` (tierstore 6.3). macOS: `/private/var/tierstore/fs`, served by a frontend not
  yet chosen (NFS on loopback or FSKit, tierstore C9; tierstore 8 measures NFS). The mount's root and `trees/` are mode
  0711 and cannot be listed.
- **Attach and detach** on a control socket (`fs.control_socket`, mode 0660, group `fs.client_group`; tierstore 6.5):
  `attach <digest> for <run> [view <upper dir>]` answers, once the tree's manifests are read and flattened, with the
  path to give the job; `detach <run>` ends every attachment of the run and answers with what they recorded (tierstore
  7.4: failed fills, an abort, `stall_ms`); `detach-all-except <runs>` re-asserts a client's live runs after its own
  restart. Attachments survive a restart or an update of `tierstore fs`.
- **A tree is reachable only while attached**: `trees/<digest>` resolves for new lookups between the attach and the last
  detach; an inode already referenced (an overlay's lower root, an open file) stays answerable for the mount's life
  (tierstore 6.5, I1). So on Linux a detach never pulls a tree from under a process that already holds it; it stops new
  lookups by name.
- **Tree contents** report one owner per node (`fs.owner`, tierstore D3), modes 0644 and 0755, one fixed mtime (D10).
- **Writes never reach the tree on Linux** (tierstore C4 = A): the runtime mounts its own overlay with the attached tree
  as the lower directory. On macOS there is no overlay; the core's writable views exist only if a macOS client needs
  them (tierstore 6.6, 8).
- **Failures** a job sees (tierstore 7.4): `EIO` on an open whose fill failed; errors on lookups while `tierstore fs` is
  down past its bound; a manifest that is invalid is refused at attach, by name, before the job starts.

This design takes all of that as given, and differs from tierstore 6.7 in three places, each argued where it is made:
the attach runs in the job's preparing stage rather than at the run-directory step (3.3, TI5); the detach is sent when
the job exits as well as in `remove` (3.5, TI6); and the disk offer does not subtract the cache's unused budget for now
(section 6, TI9). It corrects one assumption of tierstore 6.8 about macOS users (4.4).

## 2. The wire

### 2.1 Where a tree input travels today, and the gap

Read in code:

- The agent's wire has an input type, `InputRef` (`crates/js-agent-wire/src/native.rs:81`), internally tagged by
  `type`, with two variants: `Stage { stage_id, dest, placement }` and `Mount { root_name, rel, dest }`. `Placement`
  (`native.rs:59`) is `ReadOnly` (the default) or `Writable`. Inputs live only in the `native` body
  (`NativeSpec.inputs`) and the `run` body (`RunSpec.inputs`, `crates/js-agent-wire/src/run.rs:56`).
- No agent runs `Stage`: the macOS runtime refuses it ("this agent keeps no stage trees: only mount inputs are staged",
  `crates/js-agent/src/native/mod.rs:866`), and a container runtime refuses every `run` body before its inputs are
  looked at (`refusal::RUN_UNSUPPORTED` in `admit_body`, `crates/js-agent/src/lifecycle/machine/mod.rs:521-537`; a
  `run` becomes a body only on a runtime whose `runs_image_runs()` is true, `lifecycle/runtime.rs`, which `crun`'s is
  not). `stage_id` names a tree of jobscheduler's own tree store (`js-treestore`), which no input reads.
- The server never sends a `run` body: `start_of` builds every `JobStartSpec` with `run: None`
  (`crates/js-lease-server/src/job_start.rs:157`). The public `JobSpec`
  (`crates/js-proto/proto/jobscheduler/v1/job.proto`) has no input field at all.
- `InputRef` has no catch-all variant. An agent that meets an unknown `type` fails the decode of that one start, which
  then reads as `Command::Unknown` while the commands beside it arrive (`docs/design/agent-wire.md`, section 1;
  `commands_or_unknown`, `crates/js-agent-wire/src/heartbeat.rs`). An old agent therefore never runs a tree job with the
  tree missing; it ignores the start, and the job ends at the pool's start timeout. The capability below is what keeps a
  tree start away from such an agent.

So a tree input needs four links, of which only the first was named in the tierstore design: the agent wire, the
public submit, the server's placement and start, and the `crun` runtime's acceptance of a `run` body.

### 2.2 The agent wire: `InputRef::Tree`

A third variant, additive:

```rust
/// A tree that the node's `tierstore fs` serves, named by the digest of its root tree manifest.
Tree {
    /// `sha256:` and 64 lower case hex digits: the SHA-256 of the root tree manifest's bytes.
    digest: String,
    /// Where it appears: relative, below the exec root, as a mount's `rel`; empty for the exec
    /// root itself, which only a `Writable` tree may be.
    #[serde(default)]
    dest: String,
    /// Whether the job may change it. Absent means read-only.
    #[serde(default)]
    placement: Placement,
    /// What the agent does when the tree cannot be attached for a reason that is not the tree's
    /// own (section 5). Absent means the start fails.
    #[serde(default)]
    unavailable: TreeUnavailable,
    /// A1: fill every blob of the tree before the job starts, in the preparing stage (3.3).
    /// Absent means false: a blob is filled when the job first opens it.
    #[serde(default)]
    prefetch: bool,
    /// A2: the tree namespace of the client that submitted the job, set by the server from
    /// the client's configuration, never from the request (2.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    /// A4: the most the tree holds, which the attach reserves on the node (2.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<TreeSize>,
},
```

```rust
/// A4: what a tree needs of the node: the bytes of its distinct blobs and its entries below
/// the root, counted as tierstore counts them for `tree too large`.
pub struct TreeSize {
    pub blob_bytes: u64,
    pub entries: u64,
}
```

The three fields are optional on the wire, so the variant stays additive to the one of build item 1
(komira-ai/jobscheduler#842) whether or not that has merged. The server always sets `namespace` and `size` (2.6,
2.7); a `crun` agent refuses at admission a tree that lacks either, `invalid-job` (3.2), so its attach line always
carries both words (3.3). `namespace` follows the client-name rule
(1 to 40 bytes of letters, digits, `.`, `_`, `-`), checked by `check_input`.

```rust
/// What the agent does when a tree input cannot be attached because `tierstore fs` is down,
/// does not answer an attach in time, or refuses it for want of room or upstream (amended:
/// never for a refused namespace or a failed or slow prefetch; section 5 lists the cases).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeUnavailable {
    /// The start fails with `tree-unavailable`. The default.
    #[default]
    Fail,
    /// The job starts without the tree: its `dest` is an empty directory (the exec root, for an
    /// empty `dest`), and the agent names it in `JS_TREES_UNATTACHED` so the job stages the
    /// inputs itself.
    JobStages,
}
```

Rules, checked by `check_input` (`run.rs:205`) with the existing ones:

- `digest` is `sha256:` and 64 lower case hex digits, as `stage_id` is (`stage_digest`); the agent passes the 64 hex
  digits to tierstore, whose digests are bare.
- `dest` empty is allowed for `Tree` only, and only with `Writable`: `check_input` calls `rel_path` with empty allowed
  in that one case, and `dest()` (`run.rs:197`) gains a `Tree` arm. A non-empty `dest` follows `rel_path` as every
  input's does. `disjoint` needs no change: it already treats an empty `dest` as unrelated to every other (an input
  below the exec root is not "below" an empty `dest` in its test) and refuses two empty ones as `Duplicate`, so at most
  one input is the exec root.
- `unavailable` exists on `Tree` only.
- The bound of 256 inputs (`limits::MAX_INPUTS`) stays.

`TreeUnavailable` gets no catch-all variant either, for the same reason `Placement` has none: an agent that reads an
unknown value must not guess what the job wanted. `JS_TREES_UNATTACHED` holds the unattached inputs' `dest`s joined by
`,` (a `dest` cannot contain `,`; the exec root is written `.`, which no `dest` can be), and is set only when at least
one tree was not attached. It is the agent telling the job what happened, not configuration of the agent, so S2 does
not apply to it. The `JS_` prefix is reserved against the job's own environment (`js_types::env::env_name_reserved`),
so a submitter cannot forge it.

**The result.** The agent's `JobResult` (`crates/js-agent-wire/src/per_job.rs:274`) gains one optional field,
`input_failure: Option<String>`, skipped when absent: the code of section 5 (`tree-read-failed`, `tree-stalled`,
`tree-unavailable`) when a tree input failed while the job ran. `detail` keeps its meaning (a refusal code when the
outcome is not a clean exit), so a reader of `detail` is not surprised by a new use of it.

Amended (A1, A2, A4, A5): `input_failure` is also set on a `failed_to_start` result whose start failed for a tree
(every row of section 5 marked "start fails"), with that row's code; the new codes are `tree-prefetch-failed`,
`tree-prefetch-slow` and `tree-not-permitted`. So every tree failure, before or during the run, reaches the client as
`INPUT_FAILED` with its code beside the job's `host_id`. The code says whose the failure is: `tree-invalid` and
`tree-not-permitted` are the submission's, and no retry helps; every other code is the host's state, and a retry that
avoids the host (2.8) may succeed. A `failed_to_start` result without it maps as today.

A3 adds one `Outcome` (`crates/js-agent-wire/src/heartbeat.rs:48-69`), `ScratchLowWater` (`scratch_low_water`): the
agent ended the job because the scratch volume fell below its low-water mark and this job's writes grew fastest (6.1).
Its exit code is unconstrained, as `killed`'s is. Such a result never carries `input_failure`, even when the exit-time
detach reports failed fills or is not answered: a full volume is what made those fills fail, and the job was ended for
filling it.

Why a new variant and not `Stage` (tierstore 6.7 says the same): `stage_id` names a tree of `js-treestore`, a different
namespace with different digests (`js-treestore` names a tree by its diff id), and `Stage`'s `dest` may not be empty.
Re-meaning `Stage` would break B8. `Stage` stays as it is, refused by every runtime.

### 2.3 The capability `tierstore-trees`

The agent reports `tierstore-trees=true` in its hello when it is configured for tierstore and the control socket
answered its start-time probe (3.1). The key is a constant of `js-caps` (`crates/js-caps/src/host.rs`), and goes into:

- `DETECTED_KEYS` (`host.rs`, an array whose length grows from 10 to 11), so an operator cannot set it with
  `--capability` (`crates/js-agent/src/host.rs` refuses a detected key);
- `EXACT_KEYS` (`crates/js-caps/src/matching.rs:101`). Without it the placement below cannot work: a request key that is
  not a platform key is refused as `RequestError::UnknownKey` by `js_caps::Request::parse`, which `needs_of` ends in
  (`crates/js-sched-placer/src/map.rs`), and `NodeCaps::from_report` (`crates/js-caps/src/report.rs`) keeps only the
  keys it recognises, so a host's value would be dropped before matching. `run-files` and `scratch-quota` are neither,
  which is why they are no precedent for placement.

The server sends a start with a tree input only to a host whose hello lists the capability, in two places:

- **Placement.** A job with a tree input requires `tierstore-trees=true` of its host. `job_request`
  (`crates/js-sched-placer/src/jobs.rs`) inserts it into the job's capabilities after the client's own constraints, so
  the client does not have to spell it, and a client's `tierstore-trees=false` is overwritten.
- **The start.** `start_of` refuses to build a tree start for a host that does not list the capability
  (`Unsendable::TreesNotReported`, new), as it refuses a host-isolation start to a host without `host-jobs`. `start_of`
  reads a host's capabilities only through `BeatContext`, so `BeatContext` gains a `trees: bool`, filled in
  `agent_api.rs` the way `host_jobs` is. This is the backstop if a hello changed between placement and start; the job
  then ends at the start timeout, as other unsendable starts do.

The doc comments of `run-files` and `scratch-quota` say the server sends those fields only to hosts that report them
(`crates/js-caps/src/host.rs:45-55`), but `start_of` checks neither today. The tree check is written in `start_of` and
tested there, not left to a comment.

A4 adds two detected keys beside it, reported only with `tierstore-trees=true`, from the probe's first line (3.1):
`tierstore-pin-gib`, the node's `local_cache.pin_limit` in whole GiB rounded down, and `tierstore-tree-entries`, the
node's bound on flattened entries (section 12). Both are countable keys (`Consumable`, `matching.rs:58-66`): a host
matches when it has at least the amount. `DETECTED_KEYS` grows from 11 to 13. 2.7 says how the server asks for them.

### 2.4 The public submit

`job.proto` gains, all additive:

```proto
// One tree of inputs the node's tierstore serves, named by digest.
message TreeInput {
  // sha256: and 64 lower case hex digits: the digest of the tree's root manifest.
  string digest = 1;
  // Where the tree appears below the job's working root. Empty: the working root itself,
  // which only a writable tree may be.
  string dest = 2;
  // The job may change the tree; its changes are private and never reach the tree.
  bool writable = 3;
  // The tree may be left out when the host cannot attach it: the job then stages it itself.
  bool job_stages_if_unavailable = 4;
  // A1. Fill the whole tree before the job starts; the time is not charged to its wall limit.
  bool prefetch = 5;
  // A4. Required: the bytes of the tree's distinct blobs, and its entries below the root. The
  // host must hold at least this much; a tree larger than declared fails `tree-invalid`.
  optional uint64 blob_bytes = 6;
  optional uint64 entries = 7;
}

message JobSpec {
  ...
  // The trees of inputs the job reads. Empty: none, and the job may run on any host. With
  // trees, `command` is the whole program, an absolute path in the image, and the image's
  // own entrypoint is not used.
  repeated TreeInput trees = 41;
  // A5. Hosts (a Job's host_id) the job is never placed on. At most 16. Empty: any host.
  repeated string avoid_hosts = 52;
}

message JobResult {
  ...
  // The agent's code for the input failure, when the reason is INPUT_FAILED. Absent otherwise.
  optional string input_failure = 12;
}

enum ResultReason {
  ...
  // An input tree could not be attached, prefetched or read for the job, or reading it stalled
  // the job past its wall limit. input_failure says whose the failure is: tree-invalid and
  // tree-not-permitted are the submission's, and no retry helps; every other code is the
  // host's, and a retry that avoids the host may succeed. The job's state is FAILED even when
  // its process exited 0; the exit code is kept.
  RESULT_REASON_INPUT_FAILED = 5;
  // A3. The host's scratch volume fell below its low-water mark and this job's writes grew
  // fastest, so the host ended it. Not a quota. The job's state is FAILED; no exit code.
  RESULT_REASON_SCRATCH_LOW_WATER = 6;
}

enum RefusalCode {
  ...
  // A tree input breaks a rule of TreeInput, two trees are the working root, or a job with
  // trees has an empty or relative command.
  REFUSAL_CODE_INVALID_TREE_INPUT = 5;
  // A2. The submitting client has no tree namespace, so it may not submit trees.
  REFUSAL_CODE_TREES_NOT_PERMITTED = 6;
  // A5. avoid_hosts has more than 16 entries, a repeated one, or one that is not a host id.
  REFUSAL_CODE_INVALID_AVOID_HOSTS = 7;
}
```

The amended rules of `TreeInput` (A1, A4): `blob_bytes` and `entries` are both present, or the submit refuses
`INVALID_TREE_INPUT`; zero is allowed for an empty tree. `prefetch` has no rule.

`invalid-tree-input` is the submit's refusal; the agent's own codes (`tree-invalid` and the others of section 5) are a
different layer and never reach a submit. `trees` is part of "the same job" for an idempotent resubmit, as `files` is.
The server's `end_of` (`crates/js-lease-server/src/per_job.rs:301`) maps a result that carries `input_failure` to a new
`JobEnd` case that becomes state `FAILED` with reason `INPUT_FAILED`, whatever the outcome (amended: `failed_to_start`
included), except `scratch_low_water`; a result without it maps as today. A3: an outcome `scratch_low_water` maps to a
second new `JobEnd` case, state `FAILED` with reason `SCRATCH_LOW_WATER` and no exit code, and is checked before
`input_failure`, so a result that carries both (which the agent never sends, 2.2) still reads as the job's.

### 2.5 The server's start: a `run` body

A container start carries no inputs today; a `run` body does. A job with trees is started with a `run` body built from
the job:

- `run.program` is the job's `command`, which must therefore be an absolute path in the image (the submit rule of
  2.4); `run.args` are the job's `args`; `run.env` is the job's `env`; `run.cwd` is empty (the exec root);
  `run.network` is the job's level; `run.inputs` are the job's trees as `InputRef::Tree`, each with its `prefetch`, the
  client's `namespace` (2.6) and its `size` (2.7); `run.outputs` is empty.
- The start's own `command` and `env` are empty: `JobStartSpec::validate` refuses a `run` beside a non-empty `command`
  (`crates/js-agent-wire/src/per_job.rs:221-225`), and the job's environment is given once, in `run.env`, which is the
  one the new `crun` path reads (3.2). Today `start_of` puts `command` followed by `args` in the start's `command`
  (`job_start.rs:156`) and the job's `env` in the start's `env` (`job_start.rs:160`); a job without trees keeps that,
  exactly.

So the same `command` means something different with and without trees: without, it is appended to the image's
entrypoint as the server combines them today; with, it is the whole program. The `job.proto` comment of 2.4 says so.

Why not a new `inputs` field on the container start: inputs already have one home on the wire, the `run` body, with its
validation, its limits and its previous-release tests; a second list would be a second way to say the same thing.

### 2.6 Who may name a tree (A2)

`TreeInput.digest` is the client's, and today it is the only credential: tierstore's attach takes a digest and a run and
nothing else, gated only by the control socket's group (tierstore 6.5); a manifest's blob `source` may name any object
key outside tierstore's own prefixes (`_journal/`, `_tiering/`, `_entries/`, which a fill refuses); so a client that
knows a digest, or writes a manifest whose sources point at another's objects, reads them. Neither side can close this
alone. The server knows the client (the bearer token names it, `job_api/auth.rs`, and the job records it, `Job.client`)
but never reads the store; tierstore knows where every byte comes from but not who asked. So the check is split, and
**tierstore enforces it**:

- **The server** gives each client that may use trees a tree namespace, a configuration key of the `[[client]]` table
  (`tree_namespace = "<name>"`, `crates/js-lease-server/src/file/clients.rs`); a client given by `--client-token`
  alone has none. A submit with trees from a client without one is refused `TREES_NOT_PERMITTED`. `start_of` writes
  the namespace of the job's recorded client into every tree of the `run` body, from the configuration at that moment;
  if the client lost it since, the start is `Unsendable::TreesNotPermitted` and the job ends at the start timeout. The
  request never carries a namespace, so a client cannot name another's.
- **The agent** passes it on: `attach <64 hex> for js.<run id> namespace <name>`. It checks nothing about it, since it
  holds no client configuration.
- **tierstore** maps each namespace to the object-key prefixes it may read, in its own configuration, as its scoped
  keys already list `read_prefixes`; it resolves the root manifest, every child manifest and every blob `source` of an
  attach only under those prefixes, and refuses the attach otherwise (`not in namespace`), or when it names none while
  namespaces are configured (`namespace required`). The check is per attach, not per tree: a tree another namespace
  attached is checked again for this one. Section 12 asks for it; how keys are laid out per namespace (a
  content-addressed `cas/l/` shared by all cannot be one namespace's) is tierstore's and the writing client's to
  decide.

The agent maps both refusals to `tree-not-permitted`: a start failure, carried as `input_failure`, with no fallback,
not retriable (section 5). If the split were wrong, what a client gains is read access to another namespace's build
inputs on any node where it can run a job; tierstore's design calls tree contents build inputs, not secrets (tierstore
6.8), which is why a namespace, not a per-digest grant, is the unit.

### 2.7 A tree's size: the placement need, and the attach that refuses (A4)

tierstore today never refuses an attach for pin or table room (it can refuse one `no space` when a manifest's own fill
finds the cache past its `critical` share, a row of section 5): a pin past `local_cache.pin_limit` is refused and the
blob kept by recency only, so it may be evicted and filled again at its next open ("a refused pin costs speed, never a
job", tierstore D9), and a fill past the cache's `critical` share answers `no space`, which is `EIO` on the open.
Flattened manifests are held in memory with no bound but the per-tree one (about 110 B per entry, tierstore 6.3). So
many concurrent distinct trees end as an `EIO` in the middle of some job, or as a `tierstore fs` out of memory. This
design moves that failure to the attach, before the job starts:

- **The client declares** each tree's `blob_bytes` and `entries` (2.4). It wrote the manifests, so it has both.
- **The attach reserves them**: `attach … reserve <blob_bytes> <entries>`. tierstore refuses `tree larger than
  declared` when the tree exceeds either (`tree-invalid`, the submitter's mistake); otherwise it pins every blob of the
  tree at once, filled or not, against `pin_limit`, and counts the entries against a node bound on flattened entries,
  refusing `no pin room` or `no table room` when either would pass. A blob another attachment already pinned costs
  nothing more, and a tree already flattened for another run counts its entries once, so a toolchain shared by many
  jobs is reserved once. A fill of a reserved blob evicts unpinned entries rather than answering `no space` for the
  cache's budget or its `critical` share. The reservation ends at the tree's last detach. Section 12 asks for this.
- **The agent maps** `no pin room` and `no table room` to `tree-unavailable` at attach: retriable, carried as
  `input_failure`, and the job stages the tree itself when it asked to (section 5).
- **The placement need.** The server adds to the job's capabilities `tierstore-pin-gib` at least the sum of its trees'
  `blob_bytes` in GiB rounded up, and `tierstore-tree-entries` at least the sum of their `entries` (2.3), beside
  `tierstore-trees=true`, overwriting a client's own. So a job is placed only on a host whose limits could hold its
  trees at all. The need is matched, not booked: booking each job's bytes would count a shared toolchain once per job
  that reads it, and refuse room the node has. Two jobs that each fit but not together meet the attach refusal above.

What remains a mid-job failure is what no attach can foresee: a blob missing upstream, a fetch that fails, or a
scratch volume shared with the cache that jobs' own writes have filled, where a fill cannot be written whatever is
reserved (`tree-read-failed` in each case). The last is the case of 6.1, whose guard ends the job that fills it.

### 2.8 Not on that host again (A5)

After an `INPUT_FAILED` result (or any other), a client may resubmit the job under a new id with
`JobSpec.avoid_hosts` naming the host the failed run had (`Job.host_id`, field 10, which every job carries once
placed). The placer never places the job on a listed host: `walk_jobs`'s `fits`
(`crates/js-sched-placer/src/jobs.rs:386-394`) gains one conjunct, and the whole-host walk drops the listed hosts from
its candidates the same way. Rules: at most 16 entries, each a host id (`js_types::HostId`), none repeated, else
`INVALID_AVOID_HOSTS`. A host that no longer exists is allowed. The submit's capable-host check does not read the list,
since a pool may still launch a new host; a job that every present host is listed against waits, and ends by the
queue's wait bound as any unplaceable job does. The field is not about trees: any failure that is the host's may use
it, and a client does not set it after `tree-invalid` or `tree-not-permitted`, which no host cures. It takes part in
"the same job" (`JobSpec::same_job_as` destructures every field), which costs nothing, since a
retry after a terminal failure needs a new id anyway.

## 3. The agent on Linux (`crun`)

### 3.1 Configuration and the start-time probe

Two new flags, set in the agent's flags file (B14: configuration is the file plus a service start):

| Flag | Meaning |
|---|---|
| `--tierstore-fs <path>` | The mount of `tierstore fs`. Absolute, canonical, with no `,` or `:` (it becomes part of the overlay's options). Absent: no tree inputs, and the capability is not reported. |
| `--tierstore-control <path>` | The control socket. Required with `--tierstore-fs`, refused without it. |

At start, after its own checks, the agent probes once: it connects to the control socket and reads the first line
`tierstore fs` answers, which names the protocol's version and `fs.owner` (section 12). On an answer it reports
`tierstore-trees=true`. On none (no socket, refused, no answer within 5 s, a version it does not know) it starts
anyway, does not report the capability, and adds `tierstore-trees: <why>` to the offer's warnings, which the agent
prints as `js-agent: warning: …` as it prints the GPU toolkit probe's. The hello is built once per start
(`crates/js-agent/src/host.rs`, `offer`), so a host whose `tierstore fs` comes up after the agent reports the
capability at the agent's next start; the unit orders the agent after `tierstore fs` (3.7) so that this is rare, and
this design adds no re-probe (TI7).

The probe also compares `fs.owner` with the host uid the job's action runs as: the job user's first subordinate id plus
`ACTION_ID` (1000, `crates/js-contain/src/container.rs:12`; the sum is `Oci::container_ids`,
`crates/js-contain/src/oci/lease.rs`). A mismatch is a deployment mistake that would make every copy-up fail inside the
container (overlayfs cannot copy up a file whose owner is not mapped in the container's user namespace), so the
capability is not reported and the warning names both ids. The check runs once, at start, as B20 asks.

Amended (A1, A2, A4): the first line also names the node's `pin_limit` and its bound on flattened entries, which become
`tierstore-pin-gib` and `tierstore-tree-entries` (2.3), and its version must be one that has `prefetched`, `namespace`
and `reserve` (section 12). A version without them is a version the agent does not know: no capability, and the
warning says so.

The agent is root on a `crun` host (`agent-privilege.md`), so it may use the control socket whatever its group.

### 3.2 Admission

`crun` today accepts only container bodies and refuses every `run` (`admit_body`, `machine/mod.rs:521-537`). It gains
one kind of `run`: an image run with at least one input, every input a `Tree`, and no outputs. It is admitted through a
new runtime hook beside `runs_image_runs` (which stays false on `crun`, and whose macOS path needs a bundle store), so
nothing else about a `run` on `crun` changes:

| A `crun` start with | Answer |
|---|---|
| a `run` with only `Tree` inputs, no outputs, on an agent that reports `tierstore-trees` | admitted |
| a `run` with no inputs | `run-unsupported`, as today |
| a `run` with a `Stage` or a `Mount` input | `run-unsupported`, as today |
| a `run` with an output | `outputs-unsupported-here` (new) |
| a `run` with a `Writable` tree at a non-empty `dest` | `tree-placement-unsupported` (new); 3.4 says why |
| a `run` with a `ReadOnly` tree at an empty `dest` | refused by the wire's own check (2.2) |
| a `Tree` input on an agent that does not report `tierstore-trees` (no `--tierstore-fs`, a failed probe, an owner mismatch) | `tree-unsupported-here` (new) |
| amended (A2, A4): a `Tree` without `namespace` or without `size` | `invalid-job`: only a server that does not set them sends one (2.2) |

The `run`'s program, args, env and network replace the container's command, args, env and network exactly as the
start's own fields would; nothing else of the container changes.

### 3.3 Attach, in the preparing stage

Tierstore 6.7 places the attach "at the step where it makes the run directories (before `create_and_start`)". This
design moves it earlier, into the job's preparing stage (TI5), because of how long an attach may take: it is answered
when every manifest of the tree is read and flattened (tierstore 6.3, 6.5), each manifest may be fetched from upstream,
one fetch is bounded by `fs.fetch_timeout` (60 s by default), and tierstore states no bound for the whole attach. The
run-directory step runs on the lifecycle worker, which must not wait on such a call, as it must not wait for an image
pull (`crates/js-agent/src/lifecycle/prefetch.rs`, lines 1-10).

So the attach runs where the image pull runs. A **tree thread** of the agent (one, beside the image prefetcher) makes
the job's attaches, one connection per attach, and the job leaves the preparing stage when its image is ready **and**
every tree is attached. `Prepared { Pending, Ready, Failed }` (`lifecycle/runtime.rs`) is keyed by image today
(`Runtime::prepare`); the job's waiting state gains the trees' state beside the image's, and the tree thread's answers
are read the way the prefetcher's are. The agent bounds each attach at 120 s, a number chosen here (TI5), not
tierstore's; past it the attach counts as unavailable (section 5), and section 12 asks tierstore to state a bound of
its own.

The attach line is, in full, `attach <64 hex> for js.<run id> namespace <name> reserve <blob_bytes> <entries>`
(2.6, 2.7), with ` prefetch all` appended for a tree that asked for it (A1, below).

**Prefetch, inside the preparing stage (A1).** Without it, a file's bytes are fetched whole on its first open, inside
the job (tierstore 6.4: no block-wise fetch yet), so a cold 150 to 190 MB toolchain binary is a wait charged to the
action's wall time, and the attach bound above covers manifests only. With `prefetch` the agent appends `prefetch all`
to the attach, which tierstore already parses: its answer still comes once the manifests are flattened, and a thread of
tierstore then fills every blob the cache lacks and pins it (tierstore's operations guide, control socket). That answer
does not say when the fill has ended, so the agent then sends `prefetched <64 hex> for js.<run id>` on the same
connection, a verb asked of tierstore in section 12, which answers once the tree's prefetch has ended, with the blobs it
could not have, in the shape of `detach`'s `failures`. tierstore does not prefetch a tree twice: when another run's
attach already started the tree's prefetch, this run's `prefetched` waits for that one, gets the same `failures`, and
finds the blobs pinned for its own run as well; the prefetch goes on while any run holds the tree. The job leaves the
preparing stage only when its image is ready, every tree is attached, and every prefetching tree has answered
`prefetched`:

- empty `failures`: the tree is ready, and every blob is in the cache and pinned for the run (with the reservation of
  2.7, a pin cannot be refused, so no blob is evicted before the job opens it);
- any failure: the start fails `tree-prefetch-failed`, carried as `input_failure` (section 5);
- no answer within `TREE_PREFETCH_LIMIT`, 10 minutes from the attach's answer, a constant chosen here: the start fails
  `tree-prefetch-slow`, likewise. The pool's start timeout (15 minutes by default, counted from placement,
  `job_expiry.rs`) also covers the image wait and the attaches, so whichever passes first ends the job: when those
  took more than about 5 minutes, or the pool's start timeout is shorter, the job ends `start-timeout`, as it would for
  a slow image, and the client does not learn that the prefetch was slow.

Each waiting `prefetched` holds its own connection, on a waiter thread per prefetching job (at most the agent's job
slots), so one slow tree never holds back another job's attach on the tree thread. The time is not charged to the
job: the wall limit starts in `settle`, after the preparing stage; the lease end, when the start has one, still caps
the two together, as it does an image wait. `prefetch` is all or nothing. tierstore refuses `prefetch some`, so
prefetching only the large files is not offered; it can be added later as another optional field when tierstore
offers it (TI18). How long a cold prefetch and a cold first open take is measured in build item 6.

The wall limit is not running in the preparing stage (the deadline is set in `settle`, `machine/mod.rs:937`), so a slow
attach is never charged to the job; the server's start timeout bounds it from outside.

`attach <64 hex> for js.<run id>` sends the run id with a `js.` prefix (section 12 says why), and the run id is
`[A-Za-z0-9._-]` (`check::ident`), so it needs no quoting. The agent uses the path the attach answers with, after
checking that it is `<--tierstore-fs>/trees/<64 hex>`, spelled exactly, so a mistaken answer cannot point the overlay
elsewhere.

### 3.4 The container: the overlay's lower directory and read-only binds

In `make_resources` (`crates/js-agent/src/crun/mod.rs:641`), with the trees attached:

- **A `Writable` tree at the exec root** becomes the overlay's `lowerdir` in place of today's empty `<lease>/root`
  (`input_root: lease.root`, `crates/js-agent/src/crun/spec.rs:96` and `:165`). The mount options stay exactly as today
  (`userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off`, `crates/js-contain/src/oci/spec.rs:47-48`).
  Tierstore measured a rootless `crun` overlay with those options over its read-only FUSE mount (tierstore 6.3, 6.6,
  Measured: linux E4): on the 7.0 kernel with the FUSE mount's `max_stack_depth` 1, and on 6.8, where the mount
  declares no stack depth. The `/work` the job sees is then the tree, writable, with its writes in `upper`; this is the
  overlay `agent-egress.md` already describes, with a non-empty lower. With no writable tree, `input_root` stays the
  empty `<lease>/root`.
- **A `ReadOnly` tree at `dest`** becomes a read-only bind at `/work/<dest>` through `ContainerSpec.shared`
  (`crates/js-contain/src/container.rs:144-145`): `rbind,ro,nosuid,nodev`, rendered after the overlay
  (`oci/spec.rs:223-230`). The agent uses `shared` for the first time; today it always passes it empty
  (`crun/spec.rs:106` and `:172`). `shared` expects a directory at the mount point; `make_resources` makes `upper` and
  gives it to the container's ids before `crun` starts (`crun/mod.rs:660-668`), and the agent, as root, now also makes
  `<dest>` and its parents in `upper`, owned as `upper` is. So the point exists whatever the lower holds there: a file
  of the lower at that path is hidden by the directory in `upper`, and the bind covers it. The doc comment of
  `SharedMount.path` ("the input root has an empty directory there", `container.rs:194-195`) changes with it.
- **A `Writable` tree at a non-empty `dest`** is refused (3.2). It would need a second overlay mounted at `/work/<dest>`,
  with its own `upper` and `work`, inside the container; nothing asks for it yet (B20), and it can be added later
  without a wire change, since the wire already allows it.
- **`JS_TREES_UNATTACHED`**, when a tree was not attached and the job asked to stage it itself (section 5), is appended
  to the container's environment after `job_env` (`crun/mod.rs:628`), the way the GPU variables are, so it is never
  refused by the reserved-name check `check_run_env` applies to the job's own environment.

What the job can reach (tierstore 6.8): for a `Writable` tree, the FUSE mount is not in the container's mount namespace
(tierstore measured 0 FUSE lines in the container's `mountinfo`; the lower's host path shows in the overlay's options,
which grants nothing). A `ReadOnly` tree is a bind of the tree's directory, so one `fuse.tierstore` mount at
`/work/<dest>` is visible in the container's `mountinfo`; it exposes only that tree, read-only. In both cases no path in
the container leads to another tree.

What a job sees, compared with a tree laid out in scratch (tierstore 6.6, Measured: linux E4): reads, `mmap`, `dlopen`
and `exec` work; a write to an input copies it up into `upper`; a delete is a whiteout; renaming an input
**directory** fails `EXDEV` (GNU `mv` falls back to copying; a tool calling `rename(2)` directly sees the error), and
renaming an input file works.

What tierstore's spikes have not shown yet, and build item 6 runs: that the mount survives a restart of its unit as seen
from the agent, and that an attach after a failed lookup of the same digest succeeds (both open in tierstore 12, SP1);
and E4 used a mapping of uid 1000 to host 1000, not this agent's subordinate range.

### 3.5 Detach, on every way a run ends

Tierstore 6.7 places the detach in `remove`, after outputs are read and before the scratch delete. This design keeps
that detach and adds one at exit (TI6), because the job's result is built and queued (`Event::ResultReady`) when
`supervise` sees the exit, before the `Removing` stage (`crates/js-agent/src/lifecycle/machine/cleanup.rs:64-96`), and
the result needs what the detach records (section 5).

Every detach is made by the tree thread, never by the lifecycle worker; the worker hands it the run and reads its
answer on a later pass, as it reads the attaches'. Detach does no fetch, and the agent bounds it at 5 s. The calls:

- **At exit.** When `supervise` sees the container exit, it asks for `detach js.<run id>` and keeps the job in its
  current stage until the answer comes (or the 5 s pass); then it builds the result with `input_failure` set from the
  answer (section 5). The job's processes are gone, and the overlay keeps its lower answerable regardless (I1).
- **In `remove`, `remove_leftover` and `tear_down`.** `CrunRuntime::remove` (`crun/mod.rs:830-882`) asks for a detach
  after `crun delete` and before the lease directory is removed, when the exit-time detach got no answer; so do the
  branch for an adopted or leftover run (`remove_leftover`, `crun/mod.rs:834-839`) and the teardown of a run whose
  making failed (`tear_down`, `crun/adopt.rs:527`, through `ledger.discard`).
- **At the ends of the preparing stage.** A job that ends before it has a container never reaches `remove`: an image
  that failed (`Prepared::Failed`, then `settle` with an error), an attach or a prefetch (A1) that failed or passed
  its bound, a stop, a cancel or a lease end while preparing (`end_unstarted`, `lifecycle/machine/job.rs:326-335`).
  Each of these asks for a detach of every tree the job attached.
- **At restart**, `detach-all-except` (3.6) is the backstop for a detach that was asked for and never answered.

`not attached` is a success: detach is idempotent.

**Who removes the run directory.** The overlay's `work/work` is created mode 000 and the whiteouts are character
devices 0/0 owned by the container's mapped root; the action user cannot remove them, so tierstore requires that the
runtime remove the run directory as the overlay's mounter or as root (tierstore 6.6, D18). The agent removes the lease
directory as root, through `js_contain::remove::remove_tree` (`crates/js-contain/src/remove.rs:63`, called from
`CrunRuntime::remove`); root's permission override covers `work/work` and the whiteouts, so nothing changes in that
function. What is new is that whiteouts now appear (with an empty lower there was nothing to white out), so the removal
test of section 8 creates them. Nothing of tierstore is mounted under the run directory on the host (the overlay and
the binds are in the container's mount namespace, gone with it), so the walk never meets a FUSE mount.

### 3.6 Agent restart

The agent adopts its live runs at start (`Lifecycle::adopt`, `crates/js-agent/src/lifecycle/machine/adopt.rs:67-171`)
and then removes every scratch entry of a run it did not keep. Right after the adoption, before that sweep, it asks the
tree thread to send `detach-all-except js. <run ids>` with the ids of the runs it kept (its jobs and its stuck runs, the
set the sweep keeps, `adopt.rs:137-139`); section 12 says why the prefix is named. Runs it evicted are not in the list
and are detached with it. No ledger change is needed: the run ids are all tierstore needs, and an adopted run detaches
at its exit as in 3.5. Until the call is answered the agent admits no tree job, and the tree thread repeats the call
every 30 s.

### 3.7 The integrity check, and the units

The agent's integrity check compares the mounts of its own namespace after a job with a baseline taken at start, and
counts only a mount that is **new** (`crates/js-agent/src/integrity/mod.rs`: a mount is its root, mount point, file
system type and source, without the kernel's numbers; one that disappeared is not a finding). The tierstore design
relies on the baseline: the node-wide mount is made before the agent starts, so it is in it (tierstore 6.7), and no
per-job mount is made on the host (the overlay and the binds are in the container's namespace, which the check does
not read).

Two cases the baseline alone does not cover, both new here:

- **`tierstore fs` mounts afresh while a job runs.** When its unit gives up restarting, systemd drops its descriptor
  store, the kernel aborts the connection, and the next start mounts afresh (tierstore 7.2). A new mount with the same
  root, point, type and source is the same element of the set and is not new; but if it appears while the old one is
  gone, and a job ends in between, it is. (That a re-made mount reads as the same element is inferred from the check's
  comparison, not measured against a real `tierstore fs`; build item 6 runs it.) So `hosts_own_mount`
  (`integrity/mod.rs:531`, the host's own mounts, today the network namespaces and `/run/user/<uid>`) gains one entry:
  `point` equal to the configured `--tierstore-fs` and `fstype` equal to `fuse.tierstore`. The function takes the
  configured path, as a field of the check. A FUSE mount anywhere else, or another type at that point, is still a
  finding. A job cannot make this mount: its container has no `/dev/fuse` and no capabilities.
- **The type and source must be stable** across instances `a` and `b` and across releases, or each hand-over would be
  a new mount. Section 12 asks tierstore to fix them as `fuse.tierstore` and `tierstore`.

The `crun` agent's unit (`packaging/js-agent/js-agent-crun.service` and its copy under `packaging/js-agent/hostkeeper/`)
gains `tierstore-fs@a.service tierstore-fs@b.service` in its `After=`: an ordering, not a dependency, so a node without
tierstore runs the agent as before. `Wants=` is not extended: it would start both alternating instances. The unit's
header comment, which explains its ordering, says so, and the pin in `crates/xtask/tests/js_agent_farm_unit.rs` (which
asserts `After=` whole) and the hostkeeper package test under `crates/xtask/tests/` change with it.

## 4. The agent on macOS (`sandbox-exec`)

Nothing here is built before tierstore's macOS frontend exists (tierstore C9 is open). The section says what the agent
will do then, so that the wire and server work of sections 2 and 3 do not have to change for it. Until then a macOS
agent reports no `tierstore-trees` and receives no tree job.

### 4.1 The mount

At boot root mounts the frontend at `/private/var/tierstore/fs`, before the agent starts. If C9 chooses NFS, the mount
is made with `nosuid,nodev` given explicitly: tierstore measured that a loopback NFS mount made by root has neither by
default, which would let a setuid file in a tree take effect (tierstore 8, Measured: macos E14). The profile's floor
denies `file-mount` to every job (`crates/js-mac-pool/src/seatbelt/profile.rs:50`), so the agent's jobs never mount.

launchd has no ordering between daemons, and the agent's plist
(`packaging/js-agent-macos/hostkeeper/units/js-agent.plist`) starts the agent on its token's path only. A Mac whose
mount comes up after the agent therefore stays without the capability until the agent's next start (open question Q3).

### 4.2 The Seatbelt profile: the tree granted, the mount's root never

The agent builds profiles with the open read model (`ReadModel::Open`, `crates/js-agent/src/native/settings.rs:174`):
reads are allowed except under the listed read denies. The builder's allow-list model (`ReadModel::Allow`) exists, but
the agent does not use it, because it builds no grants (the `NativeConfig` doc, `native/mod.rs`). Under either model the
tree must be readable and the rest of the mount must not:

- The mount's root is a **read deny** of every run. The agent adds the canonicalised `--tierstore-fs` to
  `NativeConfig.read_denies` itself, in the step that canonicalises the other denies (`native/settings.rs:139-151`);
  none of today's denies (the state and scratch roots, the helper's socket directory, `--native-deny-read`,
  `kcpassword`) covers it. `Profile::render` emits the read denies at `profile.rs:124-128`. So no run can read another
  run's tree by its digest, nor a view.
- Each tree of the run is granted by a **new policy item emitted after the read denies and before the floor**:
  `(allow file-read* (subpath (param "TR_i")))` for each tree, and
  `(allow file-read-metadata (literal (param "TM_0")) (literal (param "TM_1")))` for the mount's root and its `trees/`,
  so that `realpath` and `getcwd` can `lstat` the ancestors. Later rules win in Seatbelt, so the grant beats the deny of
  the mount's root and still loses to the floor. It is one item, emitted in the same place under both read models, so
  the agent does not have to move to the allow-list model to use it. The tierstore design names this as the
  alternative to the allow-list model (tierstore 6.7).
- Every path in the profile is `realpath`-canonical, because Seatbelt matches the resolved path. `AbsPath::parse`
  refuses `/var/…` and the other linked roots and asks for `/private/var/…` (`LINKED_ROOTS`, `seatbelt/path.rs:10`,
  checked at `:85`); the agent canonicalises `--tierstore-fs` once at start and builds each tree path as
  `<mount>/trees/<64 hex>` from it, so no per-run `realpath` is needed. A `subpath` rule matches the resolved path, so a
  symbolic link in a tree that points outside it leads to a denied path (tierstore Measured: macos E15, E18a).
- A tree path is never a write grant: the profile's head denies `file-write*` everywhere but the run's home.

The builder bounds each list at 64 entries (`MAX_LISTED`, `policy.rs:16`). The read-only list of a run holds its files
directory, its mount inputs and, with this design, its tree links (4.3); the agent checks that sum at admission and
refuses a start over it (`too-many-inputs-here`), and the tree grants are a second list with the same bound.

### 4.3 Where the tree appears

The run's working directory is `exec/` in the pool user's home. A `ReadOnly` tree at `dest` appears as a symbolic link
`exec/<dest>` → `/private/var/tierstore/fs/trees/<64 hex>`, made by the agent before the job's first process. The link
is in the run's read-only list (`IN_i`, `profile.rs:129-132`), which keeps the job from removing or replacing the link
itself; writes through it already land outside the run's home, where the profile's head denies them. A job that
resolves the link reads the tree lazily.

The agent's working-directory check follows a link only when it resolves inside `exec/` (`native/cwd.rs:44-80`), so a
`cwd` at or below a tree's `dest` would be refused at start as escaping the run directory. The agent refuses such a
start at admission instead, by name (`cwd-in-tree`), rather than widening the check.

A `Writable` tree on macOS has no overlay to stand on. Three answers exist, and this design does not choose among them
before C9 (open question Q1):

- (a) the agent copies the tree out of the mount into `exec/<dest>` with the copy the macOS runtime already makes of a
  `Mount` input (`crates/js-agent/src/native/files.rs:74-89`): no laziness, but today's semantics and code;
- (b) the agent builds directories and a symbolic link per file into the tree, and copies up the files the job will
  write, which must then be declared before the start (tierstore 8, option a: 2.0 to 2.1 s for 46,362 files, Measured:
  macos E18);
- (c) the core's writable views (tierstore 6.6), if tierstore builds them: the agent makes the view's upper directory in
  the run's home and attaches with `view`. A view over loopback TCP is a fence only for a run whose network is `Off`
  (tierstore 6.8), so (c) would be refused at admission for a run with more network, which would use (a). The run's
  working directory would be in the view, so the working-directory check above would have to accept the view's path.

### 4.4 Detach, restart and the pool user

The lifecycle builds the result when `poll` sees the run's main process exit, before `remove`, on macOS as on Linux
(`cleanup.rs:64-96`). So the exit-time detach of 3.5 is made there too. Unlike Linux, the run's user may still have
processes at that moment (`kill_uid` runs only in `remove`, `native/mod.rs:723-744`); after the detach, a stray process
that looks up a name of the tree fails, which is harmless since the job has ended and `remove` kills it next.

`remove` then kills the user's processes (`kill_uid`), collects declared outputs, and returns the pool user to the pool
(`user_delete`, a reset: `docs/design/mac-lease-teardown.md`). The agent sends its second detach **after `kill_uid` and
before `user_delete`**: after, so that no process of the run still reads the tree; before, so that the pool user,
which another lease will be lent next, holds no attachment. This corrects one assumption of tierstore 6.8, which says
jobscheduler creates a fresh user for each run: it lends a pool user per lease and resets it after, so the uid that a
view's server-side check compares with is reused. Under option (c) the order above is what keeps a view from outliving
its run's use of the uid; under (a) and (b) there is no per-uid check in the server, and the order only keeps the
attachments tidy.

After a restart the agent adopts the runs its journal holds a run record for (`native/adopt.rs`), and sends
`detach-all-except` with those run ids before it admits a tree job. A lease with no run record (a start that died
before the helper started its process) is ended by the rescue's `reclaim`, and its attachment is dropped by the same
call, since its id is not listed.

macOS has no integrity check (`integrity/mod.rs`, "macOS"), so 3.7 has no macOS counterpart.

## 5. Failures

Every failure below ends in one of three places: the start fails (no job ran), the job's result says the input failed
(the job ran, and the failure is the host's), or the job's own result stands. The rule that separates them is
tierstore's (7.4): a recorded failure of the filesystem makes the run an infrastructure failure, whatever the job's exit
status, so that a compiler's "cannot read file" caused by a fetch is never reported as the user's build error.

Amended: a "start fails" row below is reported as a `failed_to_start` result whose `input_failure` is the row's code
(2.2), so the client reads it as `FAILED`, reason `INPUT_FAILED`, beside the host it was placed on, and may resubmit
avoiding that host (2.8), except after `tree-invalid` or `tree-not-permitted`, which are the submission's. A start that
fails for any other reason is `FAILED_TO_START` as today.

| Failure | When the agent learns it | Result | With `job_stages` |
|---|---|---|---|
| The control socket does not exist, refuses, or does not answer the attach within 120 s | attach | start fails `tree-unavailable`, retriable | the job starts without the tree; `JS_TREES_UNATTACHED` names its `dest` |
| `tierstore fs` refuses the attach for a reason that is not the tree's (`upstream <status>`, `no space`; A4: `no pin room`, `no table room`) | attach | start fails `tree-unavailable` | as above |
| A2: `tierstore fs` refuses the namespace (`not in namespace`, `namespace required`) | attach | start fails `tree-not-permitted`, not retriable | the same: no fallback |
| A4: the tree is larger than its declared `blob_bytes` or `entries` (`tree larger than declared`) | attach | start fails `tree-invalid`, not retriable | the same: no fallback |
| A1: `prefetched` answers with failures | prefetch | start fails `tree-prefetch-failed`, retriable | the same: no fallback |
| A1: `prefetched` is not answered within `TREE_PREFETCH_LIMIT` (10 minutes) | prefetch | start fails `tree-prefetch-slow`, retriable | the same: no fallback |
| `tierstore fs` refuses the tree itself (`unsupported entry name`, `unsupported entry kind`, `unsupported manifest version`, `tree too large`, a manifest whose bytes do not hash to its digest) | attach | start fails `tree-invalid`, not retriable | the same: no fallback |
| The attach answers a path that is not `<mount>/trees/<digest>` | attach | start fails `tree-unavailable` and a local finding | the same: no fallback |
| A fill fails while the job runs (`EIO` on an open), or `tierstore fs` aborts | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-read-failed`, the exit code kept | n/a |
| The job passed its wall limit and the detach reports `stall_ms` over a tenth of the limit | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-stalled` | n/a |
| The detach at exit is not answered within 5 s, and the job did not succeed | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-unavailable` | n/a |
| The detach at exit is not answered within 5 s, and the job succeeded | detach at exit | succeeded; the detach is asked again in `remove` | n/a |
| A job's write to its upper directory runs out of space | the job | the job's own result (`ENOSPC`, as with a tree in scratch) | n/a |
| A3: the scratch volume falls below its low-water mark and this job's writes grew fastest | the scratch watch (6.1) | `FAILED`, reason `SCRATCH_LOW_WATER`, no exit code | n/a |
| The node reboots | the next start | runs lost as today; the new mount has no attachments | n/a |

`tree-invalid` has no fallback: the manifest is the submitter's, and a submitter that wrote a wrong one would stage the
same wrong tree itself or a different one; failing by name shows the defect where it was made. The other refusals are
the host's state, which another host or a later try does not share. `tree-not-permitted` has none either: staging the
tree itself would read the same objects the namespace keeps the client from (2.6). A failed or slow prefetch has none,
since the blobs it could not have are the ones the job would fetch from the same store; another host may have them
(2.8).

An unanswered detach after a failed job counts as the input's: the job failed while the filesystem it was reading did
not answer, and calling that a build error is the mistake the rule above exists to prevent. A job that succeeded read
what it needed.

`stall_ms` does not extend the job's deadline while it runs, as tierstore 7.4 suggests (TI11): the agent learns it only
at detach, and a poll of `tierstore fs` per running job would be machinery for a case tierstore's measurement plan
(tierstore 11) has not shown (B20). The timeout row keeps the submitter from reading such a timeout as its own.

On macOS, a server that stays down leaves the run's processes blocked in the kernel, where `SIGTERM` does not reach
them (tierstore 7.5, Measured: macos E16), and tierstore 7.5 asks the runtime to kill the run past `fs.recover_wait` and
fail it as an infrastructure failure. That row belongs with the frontend C9 chooses, and is designed in build item 5;
the agent's `kill_uid` already escalates to `SIGKILL` in rounds (`mac-lease-teardown.md`), and a process still blocked
mid-read after them is the forced unmount of tierstore 7.5, tierstore's to make.

## 6. Disk

There is no per-job disk quota on the farm: its pools book no disk, and its agents run without `--scratch-quota`
(`jobscheduler-design.md` 5.7, `pools-providers-network.md`, `agent-privilege.md` Duty 3). This design keeps that.

- **A tree's bytes** are in tierstore's cache, under the cache's own budget, not in the run's scratch. They book
  nothing.
- **A job's writes**, copy-ups included, are in the run's `upper` on the scratch volume, as every write of a container
  job is today. Copy-ups are new bytes only for inputs the job changes.
- **The node's disk offer** (`disk-gib`: the volume's free space less the unused budgets of the bundle and image stores
  and less the low-water mark; `crates/js-agent/src/host.rs:90-112`, `crates/js-agent/src/disk.rs`) is a number the
  scheduler compares only with jobs that ask for disk; on the farm none does. When tierstore's cache shares the scratch
  volume, the bytes it holds are already out of the volume's free space, and its unused budget would be promised twice.
  Tierstore 6.7 and 6.9 say the offer subtracts that unused budget, and its plan puts it in this work (tierstore 12,
  item 9). This design does not build it now (TI9), since on the farm no job asks for disk and the offer promises
  nothing. Amended: the deployment no longer sets `--scratch-low-water-gib` to the cache's budget, as this paragraph
  first said, because the mark is now also the level at which the agent ends a job (6.1): with a large budget and a
  near-empty cache, jobs would be ended while the volume still had that budget free. It sets the mark to the free space
  the cache needs to keep filling, a margin of the deployment's choosing, at the source and once (B20). If a pool that
  books disk ever runs on such a node, the reserve is a small change beside the image reserve of `disk.rs`, with
  tierstore's own report of its used bytes as the input, since a walk of a cache of millions of files on every reread
  (every 30 s, `disk.rs:36`) is not acceptable.

### 6.1 The low-water mark at run time (A3)

Without a quota, one runaway job's `upper` and copy-ups can fill the scratch volume; when the cache shares it, every
fill then fails (`io`, its directory cannot be written, or `no space` past the cache's `critical` share), the open is
`EIO`, and other jobs fail `tree-read-failed` for a job that is not theirs. The reservation of 2.7 does not help there:
it holds the cache's budget, not the volume's free space.
`--scratch-low-water-gib` today only lowers the disk offer (`host.rs:107-112`): nothing watches the volume while jobs
run, and nothing ends a job for it (`Cause` has `Stopped`, `TimedOut`, `Silenced` only,
`lifecycle/machine/job.rs:120-127`). This design keeps the farm without a quota and adds a node-level guard, not a
per-job limit:

- **When it is armed.** On a `crun` agent with both `--tierstore-fs` and `--scratch-low-water-gib`: the case of a cache
  that shares the scratch volume. No new flag, and no change on any other agent. Once armed it covers every running job
  of the agent, tree or not, since all of them write to the same volume.
- **The watch.** A scratch watch thread reads the volume's free space every 5 s (`LOW_WATER_POLL_MS`) with
  `facts::free_bytes` (`crates/js-agent/src/facts.rs:692`, a `df` through the agent's `CommandRunner`), called
  directly, not through the disk gauge (`ScratchDisk`), which keeps an answer for 30 s. While the free space is above
  the mark it does nothing more. The lifecycle worker never reads the disk.
- **Who is ended.** The first poll at or below the mark takes a baseline: the size of each candidate's writes (its
  `upper` and its run directory, with `disk::tree_bytes`, links not followed: the job's own bytes, not the cache's).
  A candidate is a job that is running and has no stop asked for, for any cause. The next poll, if the volume is still
  at or below the mark, measures again and ends the one candidate whose writes grew most between the two, the one
  started last among equals; a job present in only one of the two measurements (started or ended between them) is not
  compared. A job that grew by nothing is never ended: if none grew (the cache, or something outside the jobs, took the
  space) nothing is ended and the agent logs one warning per crossing. After an ending, no new baseline is taken until
  that job's run directory has been removed, so a second job is never chosen while the first one's bytes are still on
  the volume; then, if the volume is still at or below the mark, a new round begins.
- **How it ends.** The worker takes a fourth `Cause`, `ScratchLow`, and stops the job with the grace a wall-limit stop
  has. The result's outcome is `scratch_low_water` (2.2), which the server records as `FAILED`, reason
  `SCRATCH_LOW_WATER` (2.4). The client can tell it from its own failure and from a host's input failure; whether a
  retry may succeed is for the client to judge, since the job may be the runaway.

It is not a quota: no job has a number of its own, a job may write as much as the volume holds while the node has room,
and only a node that crosses its operator's mark ends one job, the fastest grower. The walks run only below the mark,
so they cost nothing on a healthy node. macOS has no overlay and no cache on the scratch volume yet; whether it needs
the watch is decided with its frontend in build item 5.

## 7. Compatibility (B8)

- **The agent wire.** `InputRef::Tree`, `TreeUnavailable` and `JobResult.input_failure` are additive. An agent of a
  previous release reads a start with a tree as `Command::Unknown` and ignores it (2.1); the server never sends one to
  it, since it does not report `tierstore-trees`. A server of a previous release ignores `input_failure`, an unknown
  field. The servers upgrade before the agents (`agent-wire.md`, section 1), so a server always knows the capability and
  the field before an agent sends them.
- **The public API.** `JobSpec.trees` (field 41), `TreeInput`, `JobResult.input_failure` (field 12),
  `RESULT_REASON_INPUT_FAILED` and `REFUSAL_CODE_INVALID_TREE_INPUT` are additive. A previous client reads the new
  reason and refusal code as `UNSPECIFIED`, as each enum's comment says it reads a value it does not know, and never
  sends `trees`. The other direction is the dangerous one: a new client against a previous server would have field 41
  dropped as unknown, and its job accepted and run without its trees. So servers upgrade before the clients that send
  trees, as before agents, and a client that sends trees checks that the job the submit answers with (`Job.spec`) lists
  them, and cancels it if not. The test of section 8 shows the drop.
- **The capability** is a new key; a server of a previous release does not know it and drops it from the host's report
  (`NodeCaps::from_report`, `crates/js-caps/src/report.rs`).
- **The agent's refusal codes** (`tree-unavailable`, `tree-invalid`, `tree-read-failed`, `tree-stalled`,
  `tree-placement-unsupported`, `tree-unsupported-here`, `outputs-unsupported-here`, `too-many-inputs-here`,
  `cwd-in-tree`, and with the amendments `tree-prefetch-failed`, `tree-prefetch-slow`, `tree-not-permitted`) are added
  to `refusal.rs`, which is add-only and snapshot-tested (`refusal_codes_snapshot`).
- **The ledger** of a `crun` run does not change (3.6).
- **tierstore's control protocol** is a local interface between two processes of one node. Tierstore 10 says its local
  sockets are versioned by their first line; the control socket's line is asked for in section 12. The agent refuses a
  version it does not know at the start-time probe, so a mismatch shows as a missing capability and a warning, not as
  failed jobs.
- **The amendments.** `Tree`'s `prefetch`, `namespace` and `size` are optional with defaults, so a start without them
  reads as before; a previous agent never meets them, since it never gets a tree start. `Outcome::ScratchLowWater`
  reaches a previous server as `unknown`, which `end_of` does not record, so servers upgrade before agents, as for every
  agent wire addition (`agent-wire.md`, section 1). The two new detected keys are dropped by a previous server, like
  `tierstore-trees`. In the public API, `TreeInput` fields 5 to 7, `JobSpec.avoid_hosts` (field 52),
  `RESULT_REASON_SCRATCH_LOW_WATER` and the two refusal codes are additive, and a previous client reads the new values
  as `UNSPECIFIED`. A previous server drops `avoid_hosts` and may place the job on a listed host, so a client that sends
  it checks `Job.spec`, as for `trees`. `blob_bytes` and `entries` are required only of a job with trees, which no
  released client sends. The client configuration key `tree_namespace` is new; a configuration without it reads as
  before.

### 7.1 Public interface changes

Every item is new; nothing is renamed, removed or re-meant, except that the comment of `RESULT_REASON_INPUT_FAILED`,
which no release has shipped, widens to failures before the run. Each is a **public interface change**.

| Item | Where | Amendment |
|---|---|---|
| `TreeInput.prefetch` (5), `TreeInput.blob_bytes` (6), `TreeInput.entries` (7) | `job.proto` | A1, A4 |
| `JobSpec.avoid_hosts` (52) | `job.proto` | A5 |
| `RESULT_REASON_SCRATCH_LOW_WATER` (6) | `job.proto` | A3 |
| `REFUSAL_CODE_TREES_NOT_PERMITTED` (6), `REFUSAL_CODE_INVALID_AVOID_HOSTS` (7) | `job.proto` | A2, A5 |
| `JobResult.input_failure` set on a start that failed for a tree | `job.proto` semantics, agent wire | A1, A2, A4 |
| `[[client]] tree_namespace` | the server's configuration file | A2 |
| `InputRef::Tree.prefetch`, `.namespace`, `.size`; `TreeSize` | agent wire | A1, A2, A4 |
| `Outcome::ScratchLowWater` (`scratch_low_water`) | agent wire | A3 |
| codes `tree-prefetch-failed`, `tree-prefetch-slow`, `tree-not-permitted` | agent wire, `refusal.rs` | A1, A2 |
| keys `tierstore-pin-gib`, `tierstore-tree-entries` | `js-caps` detected keys | A4 |
| the control socket's first line: `pin_limit`, the entry bound, a version with the words below | tierstore's control socket (3.1, section 12) | A1, A2, A4 |
| verbs `prefetched`; attach words `namespace`, `reserve`; refusals `not in namespace`, `namespace required`, `no pin room`, `no table room`, `tree larger than declared` | tierstore's control socket (section 12) | A1, A2, A4 |

## 8. Tests

Each pull request of section 9 carries its own; together they are this list. Every test is hermetic: the agent's tests
talk to a **fake `tierstore fs`**, an in-process listener on a Unix socket in the test's scratch directory that speaks
the control protocol and is told what to answer, and to an ordinary directory standing in for the mount. Every bound
(the probe's 5 s, the attach's 120 s, the detach's 5 s, the 30 s repeat of 3.6) is tested with the agent's injected
clock and the fake's explicit "answered" events, never a real wait. A FUSE mount never appears in jobscheduler's tests;
that the overlay works over tierstore's FUSE mount is tierstore's spike SP1 (Measured: linux E4) and is checked again,
with this agent's id mapping, by the run of build item 6.

**Wire** (`crates/js-agent-wire/tests/`):

- `golden.rs`: the literal JSON of a `Tree` input with every field, and the minimal one, with `digest` and a non-empty
  `dest` only (an empty `dest` needs `Writable`, so the minimal golden names a `dest` and leaves `placement` and
  `unavailable` to their defaults); a `JobResult` with `input_failure`.
- `one_job_previous_release.rs`: `a_tree_start_reads_as_unknown_to_the_previous_release`, the frozen previous
  `InputRef` decoding a start with a `Tree`, and the command list keeping its other commands; and
  `a_previous_release_start_reads_the_same_on_this_release`, the recorded starts with `Stage` and `Mount` decoding
  unchanged. With the previous server's decode of a result carrying `input_failure`, this is the B8 pair.
- `native_job.rs` (validation, by name and exact error): a digest of 63 and of 65 hex digits, an upper case digit, a
  missing `sha256:`; an empty `dest` with `ReadOnly`; two empty `dest`s; a `ReadOnly` tree at `a` beside a `Writable`
  one at the exec root (allowed); two non-empty `dest`s where one lies below the other (refused, as today); an unknown
  `unavailable` value reading as an unknown command; 256 inputs (allowed) and 257 (refused).
- amended (A1 to A4): the goldens of a `Tree` with `prefetch`, `namespace` and `size`, and of a `scratch_low_water`
  result and a `failed_to_start` result carrying `input_failure`; a `Tree` without the three fields reads with their
  defaults; a `namespace` of 40 bytes (allowed), 41 and 0 (refused) and one with a `/` (refused); `size` with
  `blob_bytes` 0 and `entries` 0 (allowed); a previous server's frozen `Outcome` reading `scratch_low_water` as
  `unknown` (the B8 pair of the new outcome).

**Server** (`crates/js-lease-server`, `crates/js-sched-placer`, `crates/js-caps`, `crates/js-proto`):

- `js-caps`: a hello's `tierstore-trees=true` survives `from_report`; a request naming it parses; an operator's
  `--capability tierstore-trees=true` is refused;
- the submit refuses each rule of 2.4 with `INVALID_TREE_INPUT`, and accepts the same job resubmitted with the same
  trees and refuses it with different ones (`job-id-conflict`);
- `job_request` of a job with trees requires `tierstore-trees=true`, overwriting a client's `false`; a job without trees
  requires nothing new; a tree job is placed on the reporting host of two and never on the other;
- `start_of` builds the `run` body of 2.5 field by field, with the start's `command` and `env` empty, and refuses a host
  whose `BeatContext` says no trees (`Unsendable::TreesNotReported`); a job without trees starts byte for byte as today;
- `end_of` maps a result with `input_failure` to state `FAILED`, reason `INPUT_FAILED`, the code and the exit code, for
  each outcome including `Succeeded`; a result without it maps as today;
- the proto golden; a previous client's decode of `INPUT_FAILED` and `INVALID_TREE_INPUT` as `UNSPECIFIED`; and a
  previous server's decode of a `JobSpec` with `trees` dropping them (the hazard of section 7, pinned).
- amended, A2: a client with `tree_namespace` submits trees; one without, and the `default` operator from
  `--token-file`, are refused `TREES_NOT_PERMITTED`; `start_of` writes the job's client's namespace into every tree,
  ignoring nothing from the request since the request has no such field; a client whose namespace was removed after
  submit is `Unsendable::TreesNotPermitted`;
- amended, A4: a tree without `blob_bytes`, or without `entries`, is refused `INVALID_TREE_INPUT`, and one with both 0
  is accepted; `job_request` of two trees of 1 GiB + 1 byte and 1 GiB asks `tierstore-pin-gib` at least 3 and
  `tierstore-tree-entries` at least their sum, overwriting a client's own; a host reporting one GiB less than that is
  not a candidate, one reporting exactly that is; `js-caps` parses both keys as countable and refuses either from an
  operator's `--capability`;
- amended, A5: `avoid_hosts` of 16 hosts (accepted), 17, a repeated host, and a value that is not a host id (each
  refused `INVALID_AVOID_HOSTS`); a job avoiding one of two capable hosts is placed on the other, and avoiding both
  stays queued; a whole-host job is likewise never placed on a listed host; a host id unknown to the server is accepted;
  the same job id resubmitted with the same `avoid_hosts` is the same job, and with another list `job-id-conflict`;
- amended: `end_of` maps a `failed_to_start` result with `input_failure` to `INPUT_FAILED` and without it as today, and
  `scratch_low_water` to `SCRATCH_LOW_WATER` with no exit code, also when it carries `input_failure`; the proto golden
  of the new fields and values, a previous client's decode of them as `UNSPECIFIED`, and a previous server dropping
  `avoid_hosts`.

**Agent, Linux** (`crates/js-agent/tests/agent/`, with the fake):

- the start-time probe: capability reported on an answer; not reported, with the exact warning, on no socket, on a
  refusal, on silence past 5 s, on an unknown version, and on an owner mismatch;
- admission: each row of the table of 3.2, by refusal code;
- attach: the job stays preparing while the fake holds the attach and starts when it answers; the lifecycle keeps
  serving another job meanwhile; the spec's `lowerdir` is the answered path; a `ReadOnly` tree is a `shared` bind at
  `/work/<dest>` with its mount point made in `upper`; an answered path outside `<mount>/trees/` is refused;
- each row of the failure table of section 5, including `job_stages`: the container's environment holds
  `JS_TREES_UNATTACHED` exactly `.` for the exec root and exactly `a,b/c` for two trees, and lacks it when every tree
  attached;
- detach: asked at exit before the result; asked again in `remove` when the first got no answer; asked from
  `remove_leftover`, from `tear_down` after a failed `make`, and from each end of the preparing stage (image failure,
  attach failure, attach past its bound, stop, cancel, lease end) after an attach had answered; `not attached` taken as
  success;
- restart: `detach-all-except` lists exactly the kept runs, is sent before the sweep, and is repeated while the fake is
  down, with no tree job admitted meanwhile;
- integrity: a `fuse.tierstore` mount at the configured point appearing after the baseline is not a finding; a FUSE
  mount at another point is `new-mount`; another type at that point is `new-mount`; with no `--tierstore-fs` the
  allowance does not exist.
- amended, A1: the attach line with `prefetch all`, then `prefetched`; the job stays preparing until `prefetched`
  answers and starts on an empty `failures`; fails `tree-prefetch-failed` on one failure; fails `tree-prefetch-slow` at
  `TREE_PREFETCH_LIMIT` on the injected clock and not one millisecond before; a held `prefetched` of one job does not
  delay another job's attach; a tree without `prefetch` sends no `prefetched`; the wall deadline is set after
  `prefetched` answers, not at the attach; a second job attaching a tree whose prefetch another job started waits for
  the same `prefetched` answer and its failures; after `tree-prefetch-failed` and after `tree-prefetch-slow` the agent
  detaches every tree the job attached;
- amended, A2 and A4: the attach line is exactly `attach <hex> for js.<run> namespace <name> reserve <bytes> <entries>`;
  `not in namespace` and `namespace required` give `tree-not-permitted` with no fallback even with `job_stages`;
  `tree larger than declared` gives `tree-invalid`; `no pin room` and `no table room` give `tree-unavailable` and the
  `job_stages` fallback; the probe reports both new keys from the first line, and no capability for a first line of a
  version without the new verbs; a `Tree` without `namespace`, and one without `size`, is refused `invalid-job` at
  admission and no attach is sent;
- amended, A3 (fake free-space reader, fake walker, injected clock): not armed without `--tierstore-fs` or without the
  mark; above the mark, no walk; free space exactly at the mark and below it both take a baseline; of three jobs the
  one that grew most between the two walks is stopped, outcome `scratch_low_water`; two that grew equally, the later
  started; a job started or ended between the walks is not compared; a job already being stopped is not a candidate;
  the volume recovering above the mark before the second read ends nothing; no job grown ends nothing and logs one
  warning per crossing; still below after one stop, no second job is chosen until the first's run directory is
  removed, and then a new round begins; a stopped job whose exit-time detach reports failed fills, or is not answered,
  still has outcome `scratch_low_water` and no `input_failure`.

**Removal** (a new case in `crates/js-contain/tests/crun_root`, run by the existing `crun-root` job of `ci.yml`; no new
job): a container over a plain directory as its lower deletes a lower file, removes a lower directory and writes a file
of its own, leaving whiteouts and `work/work` mode 000; `js_contain::remove::remove_tree` removes the lease directory
whole, and nothing of it remains.

**Agent, macOS** (`crates/js-mac-pool`, `crates/js-agent`, in the macOS job that already runs the workspace's tests):

- the profile text of a run with two trees, asserted whole: the deny of the mount's root among the read denies, the
  grants after every read deny and before the floor, under both read models;
- a tree path spelled `/var/…` refused; the read-only list at 64 entries (allowed) and 65 (refused), counted across
  files, mount inputs and tree links; a `cwd` below a tree's `dest` refused `cwd-in-tree`;
- a new behaviour test that runs `sandbox-exec` with the generated open-model profile, two directories standing in for
  two trees under a directory standing in for the mount: the run reads its granted tree, and is denied the mount's
  root, the other tree, and a write into its own tree; the same under the allow-list fixture of `seatbelt/selftest.rs`.
  The agent's own start-up self-test stays the `/usr/bin/true` probe;
- detach asked at exit, and again after `kill_uid` and before `user_delete`, in that order, asserted from the fake
  helper's and the fake tierstore's records.

Why this list is complete for the failure and boundary cases: it takes each row of the failure table of section 5, each
row of the admission table of 3.2, each validation rule of 2.2 and 2.4 at its boundary (digest length on both sides,
the input bound on both sides, one empty `dest` against two, the macOS list bound on both sides), both directions of
each compatibility edge of section 7 (agent and server, client and server), the five ways the probe can fail, every
place a detach is asked from (exit, `remove`, `remove_leftover`, `tear_down`, each end of the preparing stage, restart)
with and without an answer, and for the integrity allowance one case inside it and one on each side of it. The
amendments keep that shape: each new failure row, each new rule at its boundary (`avoid_hosts` at 16 and 17, a
namespace at 40 and 41 bytes, a size of 0, a pin need at one GiB rounding, the prefetch bound at and before it, the
low-water mark at and above it), each new compatibility edge in both directions, and for the scratch watch each branch
of its choice (armed or not, one or several growing, a tie, none growing, recovery).

## 9. Build items

One subject per pull request, in this order; each names its tests from section 8. Items 1 and 2 need nothing of
tierstore. Items 3 and 4 are written against the fake (amended: tierstore's control socket now exists, but not the
words section 12 asks for, which the fake speaks as asked); item 6 needs item 5 of tierstore's phase 1 plan (the core
and `tierstore fs` on Linux) on a node.

1. **js-agent-wire, js-caps**: `InputRef::Tree`, `TreeUnavailable`, `JobResult.input_failure`, the validation, the
   refusal codes, the `tierstore-trees` key in `DETECTED_KEYS` and `EXACT_KEYS`, the wire tests, and `agent-wire.md`
   (its list of types with no catch-all variant).
2. **js-proto, js-jobs, js-lease-server, js-sched-placer**: `JobSpec.trees`, `TreeInput`, `JobResult.input_failure`,
   `RESULT_REASON_INPUT_FAILED`, `REFUSAL_CODE_INVALID_TREE_INPUT`, the submit's checks, the placement need,
   `BeatContext.trees`, `start_of`'s `run` body and refusal, `end_of`; `job-surface.md`.
3. **js-agent (common to both platforms)**: the two flags, the tierstore client (a module of `js-agent`, its one user,
   with the fake beside its tests), the tree thread, the start-time probe, the capability, the integrity allowance, and
   the unit's `After=`.
4. **js-agent and js-contain (`crun`)**: admission of a tree `run`, the attach in the preparing stage, `lowerdir` and
   `shared`, the mount points in `upper`, `JS_TREES_UNATTACHED`, every detach of 3.5, `detach-all-except`, the failure
   mapping, the `SharedMount.path` comment, and the removal test.
5. **js-mac-pool, js-agent (macOS)**, after C9: the policy item and the mount-root deny of 4.2, the link and the
   `cwd-in-tree` refusal of 4.3, the chosen answer of Q1 and Q3, the detaches of 4.4, the down-past-the-bound row of
   section 5, the mount's boot step in the macOS package.
6. **An end-to-end run on one Linux node** with `tierstore fs` and a kbf task that reads a tree: the action runs, a
   write copies up, the run directory is removed, the integrity check passes, a restart of `tierstore fs`'s unit leaves
   the agent's capability and a later attach working. A runbook entry under `docs/runbooks/`, not a test of the PR set
   (B9).

The amendments land in the same items, whose boundaries do not move:

| Item | A1 prefetch | A2 namespace | A3 low-water | A4 size | A5 avoid hosts |
|---|---|---|---|---|---|
| 1 | `Tree.prefetch`; codes `tree-prefetch-failed`, `tree-prefetch-slow` | `Tree.namespace`, its rule; code `tree-not-permitted` | `Outcome::ScratchLowWater` | `Tree.size`, `TreeSize`; keys `tierstore-pin-gib`, `tierstore-tree-entries` | |
| 2 | `TreeInput.prefetch`; `input_failure` on a failed start in `end_of` | `tree_namespace`, `TREES_NOT_PERMITTED`, the namespace in `start_of`, `Unsendable::TreesNotPermitted` | `SCRATCH_LOW_WATER`, its `JobEnd` | `blob_bytes`, `entries`, their check, the two needs in `job_request` | `avoid_hosts`, `INVALID_AVOID_HOSTS`, both placement walks |
| 3 | the `prefetch all` word and `prefetched` in the client | the `namespace` word | | the `reserve` word; the first line's limits and the two keys in the probe | |
| 4 | the wait in the preparing stage, its bound, the waiter threads | the refusal mapping | the scratch watch, `Cause::ScratchLow` | the refusal mapping | |
| 5 | the same on macOS | the same | decided there (6.1) | the same | |
| 6 | the cold prefetch time and cold first-open latency, measured | a refused attach outside the namespace | | | |

If item 1 (komira-ai/jobscheduler#842) merges before this amendment, its A1 to A4 rows land as a second pull request of
item 1's crates, before item 2; the fields are optional, so either order keeps the wire additive. Items 3 to 6 need
tierstore's answers to section 12 in its control protocol: items 3 and 4 against the fake, item 6 against tierstore's
build.

The disk reserve of tierstore's item 9 is left out of item 4 on purpose (section 6, TI9). kbf's side, naming its trees
in the job and dropping its own download when the tree attached, is kbf's design; this one gives it `JobSpec.trees` and
`JS_TREES_UNATTACHED`.

## 10. Decisions

Each was not decided by the tierstore design, and is decided here; the owner may overturn any.

- **TI1. A new `InputRef` variant, not `Stage` re-meant.** `Stage` names `js-treestore`'s trees; re-meaning it breaks
  B8 (2.2).
- **TI2. Trees travel in a `run` body, and `crun` accepts that one kind of `run`.** Inputs already have one home on the
  wire; a second field would be a second way (2.5, 3.2).
- **TI3. The server adds the placement need itself**, and `start_of` checks the capability again (2.3). A client cannot
  forget it, and the start is guarded even if a hello changed.
- **TI4. A `Writable` tree only at the exec root, on `crun`, for now.** A second overlay inside the container is not
  asked for (3.4); the wire already allows it, so adding it needs no wire change.
- **TI5. The attach runs in the preparing stage, on a tree thread, bounded at 120 s by a constant** (3.3). The lifecycle
  worker must not wait on a socket, and the wall limit must not count the attach. This moves it earlier than tierstore
  6.7 places it.
- **TI6. Detach at exit, before the result, and again wherever a run ends** (3.5), because the result is built before
  removal and needs what the detach records, and because a run that ends before it has a container never reaches
  `remove`.
- **TI7. The capability is a start-time fact.** The hello is built once per start; a re-probe would be machinery for a
  late start that the unit's ordering already prevents on Linux (3.1, 3.7). macOS has no such ordering (Q3).
- **TI8. A narrow integrity allowance** for the one mount at the configured point and type, rather than none, because
  tierstore's own recovery re-mounts while jobs may run (3.7).
- **TI9. No disk reserve for tierstore's cache now**; the low-water mark covers it where the cache shares the volume
  (section 6). The farm books no disk.
- **TI10. `tree-invalid` has no fallback; `tree-unavailable` has one only when the job asked** (section 5).
- **TI11. `stall_ms` does not extend the deadline;** it turns a timeout into an input failure when it was over a tenth
  of the limit (section 5).
- **TI12. An unanswered detach after a failed job is an input failure** (section 5).
- **TI13. On macOS, one policy item after the read denies grants the trees under both read models** (4.2), rather than
  moving the agent to the allow-list model, which it does not use today.
- **TI14. On macOS, the second detach is between `kill_uid` and `user_delete`**, since pool users and their uids are
  reused (4.4).
- **TI15. The tierstore client is a module of `js-agent`**, not a crate: it has one user (R9).
- **TI16. Run ids go to tierstore with a `js.` prefix** (3.3, section 12), so that this agent's `detach-all-except`
  cannot touch another client's attachments on the same node.
- **TI17. A `cwd` inside a tree is refused on macOS** (4.3) rather than letting the working-directory check follow a
  link out of `exec/`.

Decided with the amendments:

- **TI18. Prefetch is whole-tree, waited for in preparing, bounded at 10 minutes by a constant** (3.3). tierstore offers
  only `prefetch all`; a per-job bound would be a knob without a caller (B20). 10 minutes leaves the agent room to
  name the failure before a default pool's start timeout when the image and the attaches are quick; otherwise the
  start timeout ends the job first, as for a slow image.
- **TI19. A failed or slow prefetch, and every tree failure before the run, reach the client as `INPUT_FAILED`** with
  their code (2.2, section 5), not as `FAILED_TO_START`, so that a client can tell a tree failure from its build's,
  and from the code whether it is the host's (retry elsewhere, 2.8) or the submission's (`tree-invalid`,
  `tree-not-permitted`: no retry helps).
- **TI20. tierstore enforces the namespace; the server assigns it from the client's configuration** (2.6). The server
  never reads the store and tierstore never sees the client, so each does the half it can; the agent only passes it.
- **TI21. A namespace per client, not a grant per digest** (2.6). Tree contents are build inputs, not secrets
  (tierstore 6.8); a per-digest grant would need the server to learn every digest a client wrote.
- **TI22. A tree's size is declared by the client, reserved by tierstore at attach, and matched, not booked, at
  placement** (2.7). Booking would count a shared toolchain once per job; the attach refusal covers what matching
  cannot.
- **TI23. The low-water watch arms only with both `--tierstore-fs` and `--scratch-low-water-gib`, covers every job of
  that agent, ends the fastest grower, one at a time, and never a job that did not grow** (6.1). It protects the
  cache's share of the volume; it is not a quota. The mark is now a level at which jobs end, so it is sized as a
  margin, not as the cache's budget (section 6).
- **TI24. `avoid_hosts` is a field of the job, generic, at most 16, ignored by the submit's capable-host check** (2.8).
  A retry needs a new id anyway; elastic pools may launch hosts the list does not name.

## 11. Open questions

- **Q1. A writable tree on macOS**: copy (a), a link per file (b), or tierstore's views (c) (4.3). It depends on C9 and
  on whether tierstore builds views for its own build executor (tierstore 6.7, 8). Recommendation: (a) first, since it
  is today's code with a new source, and (c) if tierstore builds views anyway.
- **Q2. Which user mounts the macOS frontend at boot.** This design says root, with `nosuid,nodev` given (4.1);
  tierstore 8 notes that a mount by an unprivileged user gets those by default and names the boot user as open. That
  root's `mount_nfs` honours both options on a loopback export is not measured; it is a step of build item 5.
- **Q3. A macOS agent that starts before the mount.** launchd cannot order the two (4.1). Either the plist gains a
  condition on a path that exists only while the mount is up, or the macOS agent re-probes; recommendation: the plist
  condition, since it is configuration and not runtime machinery (B20). Decided in build item 5.

## 12. What this asks of tierstore

For item 5 of tierstore's phase 1 plan (the core, the control socket and the unit), so that this design's client can be
written against it:

- a first line on the control socket that names the protocol's version and the `fs.owner` uid, answered to a client
  that connects (3.1);
- `detach-all-except` that acts only on the runs whose ids begin with a prefix the caller names
  (`detach-all-except js. <runs>`), or only on the attachments of the calling client, since the build executor is a
  second client on the same node (tierstore 6.7) and a node-wide `detach-all-except` from this agent's restart would
  detach the executor's live trees (3.6, TI16);
- a stated bound for a whole attach, so that `tree-unavailable` means the same on both sides (3.3);
- the mount's file system type `fuse.tierstore` and source `tierstore`, the same for both instances and every release
  (3.7);
- `detach` answering `not attached` for a run with nothing attached (tierstore 9 already says so) and answering without
  a fetch (3.5);
- the refusal reasons of tierstore 6.1 and 7.4 as stable words, since the agent sorts them into `tree-invalid` and
  `tree-unavailable` (section 5).

Asked with the amendments:

- **A1.** `prefetched <sha256> for <run>`, answered once the tree's `prefetch all` has ended, whichever run's attach
  started it, with the blobs it could not have in the shape of `detach`'s `failures`, every blob pinned for the asking
  run too, so that the agent can wait for the fill inside the preparing stage (3.3); this is the `prefetch` of
  tierstore's pull request 6, plus a completion answer it does not give today.
- **A2.** An attach word `namespace <name>`; tierstore's configuration mapping each namespace to the key prefixes it may
  read; the root manifest, every child manifest and every blob `source` resolved only under them; refusals `not in
  namespace` and `namespace required` (2.6). How objects are keyed per namespace is tierstore's and its writing clients'
  to decide.
- **A4.** An attach word `reserve <blob_bytes> <entries>`: refused `tree larger than declared` when the tree exceeds
  either; otherwise every blob of the tree pinned at once against `pin_limit`, and its entries counted against a node
  bound on flattened entries (a tree already flattened counted once), refused `no pin room` or `no table room` when
  either would pass; a reserved blob's fill evicting unpinned entries rather than answering `no space` for the cache's
  budget or `critical` share (a volume full for other reasons still fails it, 6.1); the reservation released at the last
  detach (2.7). For attaches that reserve, this replaces tierstore's D9 ("a refused pin costs speed, never a job") with
  a refusal before the job starts. The first line names `pin_limit` and the entry bound (3.1).
