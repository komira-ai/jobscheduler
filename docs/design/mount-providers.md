# Mount providers: a job's tree prepared by a node-local provider from the moment the job is assigned

Status: **design for the owner's review; nothing here is built, and no code lands until the owner has answered.** It
adds a public interface (a new source of a layer and of a mount in the job spec, a capability, an agent flag, and a
local protocol a provider serves), and it amends [job-rootfs.md](job-rootfs.md) (decisions LR6, LR12, LR18, LR22 and
section 7.4) and D66, as section 9 lists. Its first provider is tierstore's (komira-ai/tierstore), for kbf's action
inputs (komira-ai/kbf); each of those repositories designs its own half against the contract of section 3.

**What changes for a user.** Today a job that wants a tree a storage system makes names a host directory, and the
storage system has to have made it before the job's host looks it up: a client that wants the files local before the
job runs has to learn the host from a read of the job and call the storage system on that host itself (job-rootfs.md
7.3), and the job's sandbox is readied while nothing fetches. After this design a job may name a layer or a mount as
`{provider, ref}`: a provider name and bytes only that provider reads. The agent of the host the job is placed on
hands the ref to its local provider **as soon as it has the job**, so the provider fetches while the agent pulls the
image and readies the container; the agent then asks the provider for the directory, mounts it as it mounts a host
path, and tells the provider when the job has ended. jobscheduler reads nothing of the ref and knows no storage system.

**What does not change.** A job that names no provider runs exactly as today, byte for byte on the wire. Host paths,
images, mount modes and options, the mount roots, the overlays, the after-exit check, the start timeout and the wall
limit keep their meaning; the wall limit still starts when the container is made, so a provider's work before it is
not counted against the job.

Contents: 1 the moment and the shape; 2 the spec and the server; 3 the provider contract; 4 the agent on Linux; 5 the
agent on macOS; 6 failures and bounds; 7 compatibility; 8 the first provider and its client; 9 what this amends;
10 build items and tests; 11 decisions; 12 open questions.

## 1. The moment and the shape

The owner's direction is that a job's tree is made **on the host the job is placed on, when the job is assigned**, and
never ahead on every host; and that jobscheduler does not learn any storage system's API. The moment the agent learns
of a job is the start in the heartbeat reply after `PLACED` (job-rootfs.md 7.1), admitted by `admit` and given a slot
(`crates/js-agent/src/lifecycle/machine/mod.rs`, `admit_and_start`). That is "assignment" in this document: the
earliest moment code on that host can act, and the first a client could not reach without a watch (LR12).

The shape is the one Kubernetes' CSI node plugin and Docker's volume plugins have, cut to what a job needs: a provider
is a process on the host that the operator installs and names; the agent calls it at three moments of a job and
passes an opaque reference; the provider does the storage work and returns a directory. A source is named in every call
by `(agent, run, index)`: the agent's instance, the run, and the source's place in the job, so no call needs an answer
of an earlier one to name what it is about. Three calls, all idempotent:

| Call | When | The provider | Returns |
|---|---|---|---|
| `Prepare(source, ref, mode)` | at assignment, once per provider source of the job | checks the ref, begins its work in the background (tierstore: creates the tree and begins fetching every file of it), and answers at once | nothing |
| `Mount(source)` | when the agent assembles the job's file system | answers when the tree can be opened (its files may still be filling: an open waits for its file) | a host directory |
| `Release(source)` | after the job's file system is taken apart, on every end path | gives up what the run held | nothing |

Plus `Sweep(agent, keep)` once the agent has started: release every source of this agent except those of the runs it
adopted.

## 2. The spec and the server

**Spec** (`crates/js-proto/proto/jobscheduler/v1/job.proto`), additive:

```proto
// A tree a node-local provider makes for the job (docs/design/mount-providers.md).
message ProviderSource {
  // The provider's name, as the hosts' operators name it: 1 to 32 bytes of [a-z0-9-],
  // starting with a letter.
  string provider = 1;
  // What the provider is asked for; read by the provider alone. At most 64 KiB.
  bytes ref = 2;
}

message Layer { oneof source { string host_path = 1; string image = 2; ProviderSource provider = 3; } }
message Mount { oneof source { string host_path = 1; /* tmpfs = 2, image = 3 designed */ ProviderSource provider = 4; } ... }
```

A layer is the job's root tree (kbf's input tree is one); a mount is a tree at a path. Both take the same source. All
of a job's refs together are at most 256 KiB (chosen here: a job spec is kept in the service's state, and a tree too
large for that is passed as a reference to an object the provider reads; section 8). The agent wire's start carries the
same source with the same limits (`crates/js-agent-wire/src/limits.rs` gains them).

**The ref is not secret.** It is part of the spec: kept in the service's state, returned by every read of the job
(`GetJob`, `ListJobs`; proto3 JSON shows it base64), and it may appear in findings. A ref names things the provider
reads with its own credentials; it never carries one.

**Server.** `js-lease-server` checks the shape (`job_api/rootfs.rs`): the name's grammar and the ref's size, by name,
as `INVALID_ARGUMENT` on the field, as every other shape fault is. It reads nothing else of the ref. Each provider a
job names becomes a required capability of the job, `mount-provider.<name>` = `true`, inserted where
`layered-rootfs` is today (`job_api/validate.rs`). So the existing checks do the rest: at submit, `can_ever_serve`
refuses a job no pool can ever serve `NO_CAPABLE_HOST` (the name is not one any pool's hosts or catalogs have); at
placement, only a host that reported the capability is chosen; and a start is never sent to an agent that did not
report it, so an agent that cannot decode the new source never receives one.

**Capability.** The agent reports `mount-provider.<name>` = `true` in its hello for each provider its flags name
(section 4); the key is at most 47 bytes, under the 64 a key may have. `js-caps` gains a prefix rule for the key (its
key lists are fixed names today) and counts it as detected, so the clash check of `--capability` (`js-agent/src/host.rs`)
refuses it there and a host cannot claim a provider it was not given. A host reports at most 64 capabilities in all,
which bounds its providers. A pool's static `[capabilities]` and a provider catalog name it as they name any other.

## 3. The provider contract

A provider serves the protocol `jobscheduler.provider.v1.MountProvider`, gRPC over a Unix socket, defined in
`js-proto` as jobscheduler's own contract (as the CSI spec is Kubernetes'). The socket is the provider's; the operator
names its path to the agent. The socket must be owned by root or by the agent's user and writable by no one else; the
agent checks its owner and mode before it connects, as it checks its flags file, and the provider may check the peer's
user. No key travels. The agent trusts the provider as part of the host, but not the text of the path it returns
(rule 3). Every request names its source as `(agent, run, index)`: `agent` is the agent's instance id (its state
root's), so several agents on one host never touch each other's sources, and `run` is sent as `js.<run id>`, so a
provider shared with other clients keeps jobscheduler's apart.

```proto
service MountProvider {
  rpc Prepare(PrepareRequest) returns (PrepareResponse);
  rpc Mount(MountRequest) returns (MountResponse);
  rpc Release(ReleaseRequest) returns (ReleaseResponse);
  rpc Sweep(SweepRequest) returns (SweepResponse);
}
message Source { string agent = 1; string run = 2; uint32 index = 3; }   // run: "js.<run id>"
message PrepareRequest { Source source = 1; bytes ref = 2; MountMode mode = 3; string host = 4; }
message PrepareResponse {}
message MountRequest { Source source = 1; }
message MountResponse { string path = 1; }            // an absolute host path, a directory
message ReleaseRequest { Source source = 1; }
message ReleaseResponse {}
message SweepRequest { string agent = 1; repeated string keep = 2; }   // keep: run ids, "js.<run id>"
message SweepResponse { uint32 released = 1; }
```

Rules a provider keeps:

1. **Idempotent.** `Prepare` of a source already prepared with the same ref is success (a start resent after an agent
   restart is the same run); with another ref it is refused `FAILED_PRECONDITION`. `Release` of a source it does not
   know is success, and so is a `Release` that arrives while that source's `Prepare` is still being answered: the
   provider ends what the `Prepare` began.
2. **`Prepare` answers within 10 s** (chosen here) and does its work in the background. A ref it cannot use is
   refused `INVALID_ARGUMENT` or `FAILED_PRECONDITION` with a reason of at most 256 bytes; any other failure is
   `UNAVAILABLE`.
3. **`Mount` answers when the tree can be opened**, or within 30 s (chosen here) with `UNAVAILABLE` "not ready", and the
   agent asks again; a provider that knows the tree will never be ready refuses `FAILED_PRECONDITION`. The path it
   returns lies below a directory the operator lists to the agent as a mount root (section 4), so the agent opens it
   by the same resolver as any host path and never trusts its text.
4. **Opens of a file still filling wait for it**; a read error while the job runs is the provider's to surface as an
   I/O error, which the after-exit check reads as `input-unreadable` (job-rootfs.md section 8).
5. **`Release` answers within 5 s** (chosen here). What the provider keeps after it (a cache) is its own.
6. **The provider has its own garbage collection** (a lifetime per source): the agent's state of a run can vanish
   with a reboot, and `Sweep` is the agent's way to catch up, not the only one.

## 4. The agent on Linux

**Configuration** (the host group's flags file, B14, S2): `--mount-provider NAME=SOCKET`, repeatable, as
`--mount-root NAME=PATH` is, and `--mount-provider-wait SECS` (default 120, 1 to 900): how long a start may wait on its
providers, from its admission. The provider's mount directory is listed as an ordinary `--mount-root-ro` (or
`--mount-root` for a provider that gives writable trees). The agent checks the name's grammar and that the socket's
directory exists at start; it never waits for a provider to start itself, so a provider restarting does not stop the
agent.

**The order of a start** (`lifecycle/machine/prepare.rs`), with the new steps in bold:

1. `admit` (no I/O) and `take_slot`, as today.
2. **A provider record** `<state>/providers/<run>.json` (the provider and index of each source) is written and synced
   before the first `Prepare`, so a restart can release what may exist. A source is named by `(agent, run, index)`, so
   the record needs no answer of the provider's.
3. **`Prepare` for each provider source**, on a provider thread of the agent's (one runtime for all providers, as the
   image `Prefetcher` has its own), never on the lifecycle worker and never on the `js-paths` pool. The image pull is
   asked in the same step.
4. While the image pulls, **`Mount` for each source** (a new phase, `Providing`, before `Resolving`); each answer's
   path becomes the source's host path in the job's `JobFs`. A `Prepare` or `Mount` that fails `UNAVAILABLE` is asked
   again until `--mount-provider-wait` from admission has passed. A stop while providing does not wait for a call in
   flight: the start ends, and its sources are released as on every end path.
5. From here the start is today's: the host-path lookups (60 s each, beneath the mount roots), the assembly (clone,
   overlay), the container, and the wall limit from its creation.
6. **`Release` for each source** at `Stage::Scratch`, beside `discard_fs` (`lifecycle/machine/cleanup.rs`): after the
   container is removed and the job's clones and overlays are unmounted, on every end path (an exit, a failed start, a
   stop while preparing, a failed image). The after-exit check of a failed job's host paths runs before the container
   is removed, so it reads the tree before it is released. A `Release` that fails is retried each step for 60 s, then
   left to the provider's garbage collection with a finding; it never taints the host and never holds the slot. The
   record is removed once every source is released.
7. **Once the agent has started**, after adoption: `Sweep(agent, keep = the adopted runs)` on each provider, on the
   provider thread, asked again every 10 s until answered; then `Release` of each record's sources whose run was not
   adopted. A start that names a provider whose `Sweep` is unanswered waits in `Providing` like any start whose provider
   does not answer, and fails `INPUT_UNAVAILABLE` at `--mount-provider-wait`. The ledger sidecar is not changed (its
   decode refuses unknown fields; B8).

A VM start and a whole-host job are refused a provider source as they are refused any layer or mount today. So the one
requeue there is (a whole-host job whose host failed its prepare, job-rootfs.md 7.2 rule 1) never moves a job that holds
a source; a host that is lost releases its sources at its agent's next start (`Sweep`) or by the provider's collection.

## 5. The agent on macOS (the native runtime)

The native runtime has no `prepare` today and refuses layers (`rootfs-unsupported-here`); job-rootfs.md section 5
designs mounts as links. A provider source on macOS is therefore a **mount below `/work`**, `READ_ONLY` or
`READ_WRITE` (a layer, a mount at `/work` itself or outside it, and `COPY_ON_WRITE` are refused by name, as they are
for a host path):

- `Prepare` and `Mount` run at assignment as on Linux, on the native runtime's own `prepare` (job-rootfs.md 5.1, build
  item 7), while the agent creates the run's pool user.
- The mount is a link `exec/<rest>` to the provider's path, made by the agent before the job's first process, listed
  read-only in the run's profile, with a read grant (and, read-write, a write grant) of the path's subtree and a
  metadata grant of its ancestors (job-rootfs.md 5.2, 5.4). The provider's directory is outside every read deny of
  the mount roots or is granted after it.
- `Release` follows `kill-uid` and the output collection and comes before `user-delete`, and the lease journal records
  the sources before `Prepare`, so the next agent start releases what a crash left (as it repeats a user deletion).
- `COPY_ON_WRITE` stays undecided on macOS (LR10): a provider that gives each run a writable tree of its own meets
  `READ_WRITE` instead.

## 6. Failures and bounds

| What | Seen as | Whose |
|---|---|---|
| `Prepare` refused (`INVALID_ARGUMENT`, `FAILED_PRECONDITION`): the provider cannot use the ref | start failure `AGENT_REFUSED`, detail `provider-refused`, with the provider's reason | the client's |
| `Prepare` or `Mount` unreachable, `UNAVAILABLE`, or over its call bound; the socket missing | start failure `INPUT_UNAVAILABLE`, detail naming the provider | the host's; another host may succeed |
| `Mount` not ready within the provider wait (`--mount-provider-wait`, 120 s chosen here) | `INPUT_UNAVAILABLE`, "the provider did not mount ... within" | the host's |
| `Mount` refused `FAILED_PRECONDITION` (the tree will never be ready) | `AGENT_REFUSED`, `provider-refused` | the client's |
| the returned path is not beneath a mount root, or not a directory | `AGENT_REFUSED`, `input-not-allowed` / `INPUT_UNAVAILABLE`, as for a host path | the operator's / the host's |
| a read fails while the job runs | the after-exit check's `input-unreadable` (`INPUT_FAILED`), as today | the host's |
| `Release` fails | a finding; retried 60 s, then the provider's own collection | nobody's job |

The provider's part of a start is bounded as a whole by `--mount-provider-wait` from admission (every `Prepare` and
`Mount` call inside it); the host-path lookups (60 s each), the image pull and the assembly that follow keep their own
bounds, and the pool's start timeout (900 s from placement) ends the start whatever the sum. New detail strings are
additive; no new public start failure code is needed.

## 7. Compatibility (B8)

A server of an earlier release refuses a job with a provider source by name ("no source this server knows"), as
job-rootfs.md 2.4 rule 1 promises. A new server never sends one to an agent that did not report the capability, which
every earlier agent does not. A new agent with an earlier server reports a capability that server ignores. The
protocol of section 3 is new; it grows by additions, and a provider ignores fields it does not know. Tests named in
build item 1 run an earlier client's spec against the new server and the reverse (the `Job.spec` check of
job-rootfs.md section 9).

## 8. The first provider and its client

- **tierstore** (komira-ai/tierstore) ships the provider in its own package, serving the socket: `Prepare` creates the
  tree on the node from the ref and begins a parallel fetch of all of it, one fetch per blob; `Mount` returns its path
  under tierstore's mount once the tree is placed; `Release` drops the placement. Its design is tierstore's; this
  document asks only the contract of section 3.
- **kbf** (komira-ai/kbf) would build the ref (its input root's digest and the layout tierstore needs to find each
  file), name the tree as the job's one layer, `{provider: "tierstore", ref}`, and no longer call a node itself; its
  `docs/design/trees-for-action-inputs.md` (a client-side create after a read shows the host) is to be revised to this.
- **The host group's configuration package** would add `--mount-provider tierstore=<socket>` and the provider's mount
  root.

## 9. What this amends

- **D66** ("no client or storage system is named") stays true of jobscheduler: a provider is named by the operator,
  and jobscheduler reads nothing of the ref. A new decision **D67** records the provider source and its contract.
- **LR22 and job-rootfs.md 2.1** refuse volumes the runtime keeps and network or device file systems because they
  "would make jobscheduler a storage client or the owner of state between jobs". A provider source is neither: the
  agent makes four local calls (`Sweep` with the three) and keeps nothing past a run's end. LR22 and the table of 2.1
  are amended to say so.
- **LR12, 7.2 rule 4 and 7.3**: the client-side hook (a client prepares on the host at the first read showing
  `host_id`) is replaced, for a provider source, by the agent's `Prepare` at assignment; the lifecycle guarantees are
  unchanged and no watch is added.
- **7.4** "the agent never asks a provider for anything" becomes "only for a provider source".
- **LR6** (allow-lists are checked by the agent, not advertised): unchanged for paths, images and ports; a provider's
  name is advertised because placement must find a host that has it.
- **LR18** (named options, no free option strings): the ref is not an option of the mount; jobscheduler never reads it,
  and its size is bounded.

## 10. Build items and tests

1. The spec, the shape checks, the capability prefix in `js-caps`, the agent wire's limits, and a provider example
   in `docs/job-surface.md` (server); tests: `a_provider_source_is_refused_by_an_earlier_server_by_name`,
   `a_job_naming_a_provider_no_pool_has_is_refused_no_capable_host`, `a_ref_over_its_limit_is_refused_naming_the_field`.
2. The protocol in `js-proto`, a fake provider in `js-testkit`, and the agent's client and flag; tests:
   `prepare_is_asked_at_assignment_before_the_image_is_ready` (the fake records `Prepare` before the container is made),
   `a_mount_answered_late_still_starts_the_job`, `a_provider_that_never_mounts_fails_the_start_input_unavailable`,
   `a_refused_ref_fails_the_start_provider_refused`, `release_follows_every_end_path_once`,
   `a_stop_while_preparing_releases_the_source`, `a_failed_release_never_taints_the_host`,
   `a_restart_releases_the_sources_of_runs_it_did_not_adopt`, `a_sweep_leaves_another_agents_sources_alone`.
3. The native runtime's `prepare` and the provider link on macOS (after job-rootfs.md build item 7).
4. tierstore's provider and kbf's client, in their repositories.

## 11. Decisions

Each is decided here; the owner may overturn any.

- **MP1. A provider source is a new member of `Layer.source` (3) and `Mount.source` (4),** not a new kind of job: a
  job's tree is still a layer or a mount, with every option it has.
- **MP2. The pool's knowledge of a provider is a capability,** so submit, placement and the start's send each check
  it with the code that checks `layered-rootfs` today.
- **MP3. The contract is a protocol jobscheduler owns, served by the provider on a Unix socket,** not a plugin binary
  the agent runs: a call is cheap, the provider keeps its own state, and a hung provider holds a thread of the agent's
  provider runtime, never the lifecycle worker.
- **MP4. `Prepare` begins at assignment and answers at once; `Mount` waits.** The overlap with the image pull and the
  sandbox's set-up is the point of the design.
- **MP5. A failed `Release` never taints the host and never holds the slot;** the provider's own collection is the
  backstop.
- **MP6. The agent writes what it may have to release before it asks for it,** on both platforms, and names a source
  by `(agent, run, index)`, so no release depends on an answer that may have been lost.

## 12. Open questions

- **Q1. A Unix socket served by the provider, or a binary the agent runs per call?** Recommendation: the socket (MP3).
- **Q2. The ref's limit: 64 KiB per source, 256 KiB per job?** Recommendation: yes; a tree that does not fit is passed
  as a reference to an object the provider reads, which keeps large requests out of the service's state.
- **Q3. The provider wait: 120 s, a flag of the agent?** Recommendation: yes, below the 900 s start timeout and above
  a cold create's expected time (to be measured with the first provider).
- **Q4. macOS in the first build, or after Linux?** Recommendation: after: the native runtime needs its `prepare`
  first, and kbf sends no macOS action to the job service yet.
- **Q5. Should a host stop reporting a provider that does not answer?** As designed, a host whose provider is down keeps
  its capability, and its starts that name it fail `INPUT_UNAVAILABLE` after the provider wait, which a client retries
  elsewhere. Recommendation: keep the capability static (a configuration fact) for the first build, and add a finding
  for a provider that does not answer; a capability that comes and goes needs the heartbeat to carry capabilities,
  which it does not today.
