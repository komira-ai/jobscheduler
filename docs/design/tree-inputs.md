# Tree inputs: a job's inputs read from tierstore's filesystem

Status: **design for the owner's review; nothing here is built.** Written against `main` of this repository at
`0c104a1d`, and against tierstore's design `docs/design/virtual-build-fs.md` as merged in komira-ai/tierstore#274
(called "the tierstore design" below, its sections cited as "tierstore 6.7"). That design asks for this one in its
section 6.7 and lists it as item 9 of its phase 1 plan (tierstore 12).

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
},
```

```rust
/// What the agent does when a tree input cannot be attached because `tierstore fs` is down,
/// does not answer in time, or refuses for a reason other than the tree itself.
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
}

message JobSpec {
  ...
  // The trees of inputs the job reads. Empty: none, and the job may run on any host. With
  // trees, `command` is the whole program, an absolute path in the image, and the image's
  // own entrypoint is not used.
  repeated TreeInput trees = 41;
}

message JobResult {
  ...
  // The agent's code for the input failure, when the reason is INPUT_FAILED. Absent otherwise.
  optional string input_failure = 12;
}

enum ResultReason {
  ...
  // An input tree could not be read while the job ran, or reading it stalled the job past its
  // wall limit: the failure is the host's, not the job's, and a retry may succeed. The job's
  // state is FAILED even when its process exited 0; the exit code is kept.
  RESULT_REASON_INPUT_FAILED = 5;
}

enum RefusalCode {
  ...
  // A tree input breaks a rule of TreeInput, two trees are the working root, or a job with
  // trees has an empty or relative command.
  REFUSAL_CODE_INVALID_TREE_INPUT = 5;
}
```

`invalid-tree-input` is the submit's refusal; the agent's own codes (`tree-invalid` and the others of section 5) are a
different layer and never reach a submit. `trees` is part of "the same job" for an idempotent resubmit, as `files` is.
The server's `end_of` (`crates/js-lease-server/src/per_job.rs:301`) maps a result that carries `input_failure` to a new
`JobEnd` case that becomes state `FAILED` with reason `INPUT_FAILED`, whatever the outcome; a result without it maps as
today.

### 2.5 The server's start: a `run` body

A container start carries no inputs today; a `run` body does. A job with trees is started with a `run` body built from
the job:

- `run.program` is the job's `command`, which must therefore be an absolute path in the image (the submit rule of
  2.4); `run.args` are the job's `args`; `run.env` is the job's `env`; `run.cwd` is empty (the exec root);
  `run.network` is the job's level; `run.inputs` are the job's trees as `InputRef::Tree`; `run.outputs` is empty.
- The start's own `command` and `env` are empty: `JobStartSpec::validate` refuses a `run` beside a non-empty `command`
  (`crates/js-agent-wire/src/per_job.rs:221-225`), and the job's environment is given once, in `run.env`, which is the
  one the new `crun` path reads (3.2). Today `start_of` puts `command` followed by `args` in the start's `command`
  (`job_start.rs:156`) and the job's `env` in the start's `env` (`job_start.rs:160`); a job without trees keeps that,
  exactly.

So the same `command` means something different with and without trees: without, it is appended to the image's
entrypoint as the server combines them today; with, it is the whole program. The `job.proto` comment of 2.4 says so.

Why not a new `inputs` field on the container start: inputs already have one home on the wire, the `run` body, with its
validation, its limits and its previous-release tests; a second list would be a second way to say the same thing.

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
  that failed (`Prepared::Failed`, then `settle` with an error), an attach that failed or passed its bound, a stop, a
  cancel or a lease end while preparing (`end_unstarted`, `lifecycle/machine/job.rs:326-335`). Each of these asks for a
  detach of every tree the job attached.
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

| Failure | When the agent learns it | Result | With `job_stages` |
|---|---|---|---|
| The control socket does not exist, refuses, or does not answer the attach within 120 s | attach | start fails `tree-unavailable`, retriable | the job starts without the tree; `JS_TREES_UNATTACHED` names its `dest` |
| `tierstore fs` refuses the attach for a reason that is not the tree's (`upstream <status>`, `no space`) | attach | start fails `tree-unavailable` | as above |
| `tierstore fs` refuses the tree itself (`unsupported entry name`, `unsupported entry kind`, `unsupported manifest version`, `tree too large`, a manifest whose bytes do not hash to its digest) | attach | start fails `tree-invalid`, not retriable | the same: no fallback |
| The attach answers a path that is not `<mount>/trees/<digest>` | attach | start fails `tree-unavailable` and a local finding | the same: no fallback |
| A fill fails while the job runs (`EIO` on an open), or `tierstore fs` aborts | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-read-failed`, the exit code kept | n/a |
| The job passed its wall limit and the detach reports `stall_ms` over a tenth of the limit | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-stalled` | n/a |
| The detach at exit is not answered within 5 s, and the job did not succeed | detach at exit | `FAILED`, reason `INPUT_FAILED`, `input_failure: tree-unavailable` | n/a |
| The detach at exit is not answered within 5 s, and the job succeeded | detach at exit | succeeded; the detach is asked again in `remove` | n/a |
| A job's write to its upper directory runs out of space | the job | the job's own result (`ENOSPC`, as with a tree in scratch) | n/a |
| The node reboots | the next start | runs lost as today; the new mount has no attachments | n/a |

`tree-invalid` has no fallback: the manifest is the submitter's, and a submitter that wrote a wrong one would stage the
same wrong tree itself or a different one; failing by name shows the defect where it was made. The other refusals are
the host's state, which another host or a later try does not share.

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
  item 9). This design does not build it now (TI9): the deployment that puts the cache on the scratch volume sets
  `--scratch-low-water-gib` to at least the cache's budget, at the source of the configuration and once (B20). That
  over-reserves by the bytes the cache holds, which are already out of the free space, and is safe. If a pool that books
  disk ever runs on such a node, the reserve is a small change beside the image reserve of `disk.rs`, with tierstore's
  own report of its used bytes as the input, since a walk of a cache of millions of files on every reread (every 30 s,
  `disk.rs:36`) is not acceptable.

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
  `cwd-in-tree`) are added to `refusal.rs`, which is add-only and snapshot-tested (`refusal_codes_snapshot`).
- **The ledger** of a `crun` run does not change (3.6).
- **tierstore's control protocol** is a local interface between two processes of one node. Tierstore 10 says its local
  sockets are versioned by their first line; the control socket's line is asked for in section 12. The agent refuses a
  version it does not know at the start-time probe, so a mismatch shows as a missing capability and a warning, not as
  failed jobs.

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
with and without an answer, and for the integrity allowance one case inside it and one on each side of it.

## 9. Build items

One subject per pull request, in this order; each names its tests from section 8. Items 1 to 4 land before tierstore's
control socket exists (they use the fake); item 6 needs item 5 of tierstore's phase 1 plan (the core and
`tierstore fs` on Linux) on a node.

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
