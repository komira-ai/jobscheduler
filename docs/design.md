# jobscheduler: design

Status: design, with a first slice merged. Project: jobscheduler. First user: kbf. Second user: ucdk.
Audience: kbf and ucdk engineers, and the project owner who decides the open items in section 15.

This is the first document of the jobscheduler repository (komira-ai/jobscheduler). Paths such as `crates/...` and `docs/...` refer to the
kbf repository (komira-ai/kbf) at commit 45e420656, the commit this document was written against. Paths beginning `ucdk/` refer to the
ucdk repository (komira-ai/ucdk) at commit f28c240. Every statement about existing code names the file it was read from. A statement
marked (not found) means a search of the named place did not find the thing; it is a keyword result, not a proof of absence. Crates of
jobscheduler itself are named `js-*`; the map is in 10.2.

Code now exists on `main` of this repository: `js-types`, `js-provider`, `js-pool-static`, `js-provider-fake`, `js-conformance`, `js-lease`,
`js-lease-server`, `js-cli` and `js-coverage`. Section 5.9 says what each becomes; 10.2 marks which crates are merged. Everything else
describes future work. What exists in kbf today (the former section 4) is in [design/kbf-today.md](design/kbf-today.md); references of the
form 4.n point there.

## Standing rules cited in this document

These are rules the project already follows. The design cites them by tag.

| Tag | Rule |
|---|---|
| R1 | Scheduling rulings: the kbf server uses all CPUs; no per-category tables and no reservation admission; a lease is a node or half a node, plus telemetry recorded for later scheduling; an action seen before starts at its last peak plus a buffer; an unseen action climbs 4, 16, 32 GiB, then the whole host; an out-of-memory kill moves to the next rung above. |
| R2 | Tests never assert timing: no elapsed or deadline assertions, no real sleeps that race a timeout. Clocks are paused or injected; progress is counted as explicit events. |
| R3 | Formats change additively only: new protobuf fields, old binaries skip what they do not know; no new required fields, no raises, no finalize steps. |
| R4 | Configuration is flags, not environment variables. A configuration file is named by a flag; a secret is a path in it, never a value. Provider-standard credential variables (`AWS_*`) are the one exception. |
| R5 | kbf and jobscheduler are public repositories: no internal names, no people, no internal plans in code, docs, commits or PR text. |
| R6 | In kbf CI every crate holds 99% line and 99% branch coverage, or a recorded floor that may only rise (`coverage-floors`). In this repository's CI the rule is stricter: every crate has zero uncovered lines and branches, recorded in `coverage-baseline` (the floors file here); no count may rise against the base branch, and a new crate enters at zero. |
| R7 | Cloud spend, uploads and deploys happen only on an explicit go from the project owner for that action. |

## 1. Summary

jobscheduler is a generic scheduler for jobs and compute. A client leases capacity (a machine, or a share of one) for a bounded time,
and work runs on that capacity through an agent. The scheduler also places individual requests (what to run, what it needs) on leased
capacity, tracks the lease, accepts one result, and remembers how much the work actually used so the next request is sized better. kbf
already contains such a scheduler, tied to Bazel remote execution. This design extracts the generic part so that ucdk Jobs, and any later
client, can use the same machine, and so that kbf runs on it as one more client.

The decisions, in one place:

1. **One core; only the edge touches the world.** The scheduler core stays a pure, deterministic state machine (`crates/kbf-sched`,
   `crates/kbf-types/src/state.rs`). It calls no node and no cloud. Adapters at the edge turn their world into core events and core
   effects into calls; the only adapters that call a cloud are capacity providers (5.5).
2. **One kind of thing runs work: an agent on a machine.** `js-agent` is the generic half of the kbf daemon: it says hello with
   capacity, labels and GPUs, heartbeats, and runs and stops containers. Capacity providers do not run work; they get machines, and a
   machine runs `js-agent`. There is one call shape (5.2).
3. **Providers get machines; pools are declared in configuration.** A capacity provider (static, aws, later others) offers
   `acquire(shape)`, `release`, `list` and `observe`. The scheduler's configuration declares providers and pools (provider, shape, min,
   max, labels); a consumer picks pools by class and labels; a new provider is a new crate behind the trait (5.5, 5.7).
4. **Scaling a pool is the provider's job.** The scheduler asks a pool's provider for machines between the pool's min and max and gives
   them back when idle (5.8). This reverses the former non-goal of fleet autoscaling (D14).
5. **Two-level leasing.** The scheduler leases machines to a consumer for a TTL; the consumer places its work inside its leases and makes
   no scheduler call per action (9.1).
6. **Absence has three answers, not two.** A thing is present, absent (observed gone) or unknown (could not be observed). Only "absent"
   frees a booking. Unknown never does. The rule covers a lease on an agent and a machine at a capacity provider (5.6).
7. **Placement is queue, then fit, then rank, each behind a trait.** kbf's three lease kinds become *resource classes* expressed as
   data. kbf's oversubscription of action memory is one admission mode of a class, not an assumption of the core.
8. **Request history is a generic sizing service.** The estimator and memory ladder move behind a `SizingPolicy` and a generic key.
   kbf passes today's constants. Results are identical before and after.
9. **kbf works at every step.** The split is a ladder of small PRs, each a move or a trait introduction with kbf as the only
   implementer. Golden trace hashes (step 0) and golden persisted and wire bytes (step 1) stay byte-identical.
10. **The wire grows additively.** `kbf.worker.v1` is kept. A generic `jobscheduler.agent.v1` package is added, and is the only agent
    wire; it also carries registration. The daemon moves to it last, and only after the server advertises it (10.3).
11. **ucdk Jobs may use jobscheduler as a second realization.** ucdk 1.0 launches directly against the cloud; jobscheduler is an
    optional realization of Job, bound to one (app, environment) (11).
12. **Lease correctness is always on; quotas belong to the platform.** Every request passes the start-contract checks (a required wall
    time, a stated fence, reserved label keys). A held lease has a TTL its client renews, and the reaper frees a lease whose TTL has
    passed and a machine nothing holds (12.1). jobscheduler has no tenant caps, budgets or spend limits: Kubernetes and cloud-provider
    quotas already enforce them, and a pool's `max` is the only brake it adds. The usage record and the labels stay because sizing,
    cleanup and attribution need them (12).

### 1.1 What this amendment deletes and adds

Deleted:

- The delegated provider flavour (a cloud batch service the scheduler calls) and its former section 5.5; `Flavour` and
  `Flavour::Delegated` leave the merged `js-provider`.
- The delegated-only unknown rules: the delegated column of the outcome table, the delegated start and stop outcome rules, and "silence is
  Unknown" for a delegated provider. The three-answer rule narrows to two places, a lease on an agent and a machine at a capacity
  provider (5.6).
- Non-goal N2 as written (no fleet autoscaling), the quota-ceiling cap of the ladder (7.4), the "no standing cloud credential" rule of
  11.1, and decision D1 as an open question.
- The kbf history (former section 4), moved unchanged to `design/kbf-today.md`; sections 5.3, 7, 11 and steps 0 to 22 moved to companions.

Added: the capacity-provider interface and the `static` and `aws` kinds (5.5); `js-agent` as the one runner, with self-registration (5.4,
5.8) and `kbf-executor` beside it (9.2); two-level leasing and the `acquiring` state (9.1); config-driven providers and pools (5.7); pool
scaling between min and max (5.8); the mapping of the merged crates (5.9); the fake aws (13.1); steps 27 to 35, decisions D14 to D22 and
risks K8 to K15.

## 2. Goals and non-goals

Goals:

- G1. A scheduler core usable without any Bazel or REAPI type.
- G2. kbf's behaviour unchanged by the extraction, proven by golden trace hashes from its simulation suite (`docs/design/simulation.md`,
  invariants I1 to I16, L1 to L4) and golden persisted and wire bytes.
- G3. Two contracts, each with one conformance suite that an implementation and a test fake both satisfy: the agent contract (5.2) that
  the kbf daemon satisfies, and the capacity-provider contract (5.5).
- G4. Placement that handles exclusive work, named devices and oversubscription as data.
- G5. A request history that sizes repeat work and survives kills without hiding them.
- G6. A durable run record for every submitted job before the submit call returns.
- G7. Lease limits that make a runaway job bounded in time (a required wall time and a fence), without changing kbf farm work.
- G8. A pool's machine count is its provider's job: the scheduler scales it between the pool's min and max (5.8).
- G9. Providers and pools are configuration; a new provider is a new crate behind the trait (5.7).

Non-goals:

- N1. Preemption and fair-share between tenants. kbf has neither (`crates/kbf-sched/src/lib.rs`, the "Not yet" list;
  `docs/design/scheduler.md`). The design adds the seam (the `Queue` type) and no policy.
- N2. Fleet version management. It stays in `crates/kbf-coordinator`, which sets desired versions for host updaters and is not a capacity
  scaler (`crates/kbf-coordinator/src/lib.rs`). (The former N2 also excluded autoscaling; D14 reverses that.)
- N3. Re-adoption of running leases across an agent restart as a scheduler feature. kbf kills leases with the daemon today; re-adoption
  is an optional agent capability for later (the heartbeat rules of 5.4 hold for any unit an agent does re-adopt).
- N4. Quotas and cost limits (tenant caps, budgets, spend ceilings, a dollar-cost model). The platform enforces them: Kubernetes quotas,
  cloud-provider quotas and instance limits; they reach jobscheduler only as a refused `acquire`.
- N5. Replacing REAPI, the CAS or the action cache in kbf. These stay in kbf.

## 3. Terms

- **Job.** A unit of work a client wants run, described by a *definition* (payload, resource need, retry policy, timeout, labels). A kbf
  action is a job whose payload references CAS digests. A ucdk Job is a job whose payload is a container image and arguments.
- **Request.** One submission of a job, with a request id. It enters the queue and ends in one outcome. In kbf it is
  `kbf_sched::input::Request` (`crates/kbf-sched/src/input.rs`) and the scheduler calls it an operation.
- **Attempt.** One try at a request on one host. Retries and reruns create attempts. Exactly one attempt's result is accepted.
- **Lease.** Two levels (9.1). A *machine lease* is the scheduler's grant of a host, or a share of one, to a consumer for a TTL
  (`js-lease`). A *placement* is a consumer's grant of resources inside its lease to one attempt, identified by (term, sequence)
  (`LeaseId`, `crates/kbf-types/src/lease.rs`). Per R1, a machine lease covers a node or half a node. A lease is recorded durably before
  anything is told to start (commit before start, `Scheduler::lease_committed` in `crates/kbf-sched/src/scheduler.rs`).
- **Agent.** The process on a machine that runs work: `js-agent`. It dials out, says hello, heartbeats, and runs and stops containers
  (5.2, 5.4). The kbf daemon is the model.
- **Host.** A machine with an agent, belonging to one pool.
- **Capacity provider.** A driver that gets machines for a pool: it launches and terminates them, or lists ones an operator owns. Kinds:
  `static`, `aws`, later `gcp`, `k8s`, `vsphere`, `proxmox`, bare metal (5.5). It never runs work. Plain "provider" in this document
  means a capacity provider.
- **Shape.** What one host offers: a resource vector plus capabilities (and GPU facts). A pool has one shape.
- **Pool.** Declared in configuration: a provider, a shape, `min` and `max` hosts, and labels (5.7). The scheduler books hosts from pools.
- **Consumer.** A client that leases capacity and places work in it: kbf, the scheduler's own run service for ucdk, or a person using
  `js-cli`.
- **Resource vector.** Named non-negative integers (`cpu_millis`, `memory_bytes`, `gpus`, `vms`, and later others) plus, on a pool only,
  a lease-count cap. Today `Resources` (`crates/kbf-types/src/work.rs`) has five fields: the four dimensions and
  `leases: Option<u64>`, the most Action and VM leases a node holds at once (`None` is no cap; a request carries `None`; `fits` and the
  arithmetic leave it alone). The cap is not a booked dimension and stays a separate typed field (step 11).
- **Resource class.** Data that says how a kind of lease is admitted: which dimensions gate admission and which are counted in the booked
  total, whether it needs an empty pool, which runtime the agent uses. Replaces the closed `LeaseKind` enum
  (`crates/kbf-types/src/lease.rs`). A consumer selects pools by class and labels (6.2).
- **Capabilities.** Typed key/value facts a host reports (ISA level, features, device attributes) under a schema; a request states needs
  the same way (`crates/kbf-caps`).
- **Booking.** The amount of a pool's resource vector the scheduler counts as taken by a lease. A booking is not a limit; the limit is
  what the agent enforces (7.3).
- **Request history.** What the scheduler has learned about past attempts of similar work: peak memory, CPU parallelism, kills,
  failures. Keyed by a *sizing key* (class, variant, partition (the tenant), basis; 7.1).
- **Fence.** The rule by which a lease ends when its agent cannot be heard. *Self-fence*: the agent stops the work after a fixed time
  without contact. *Run-on*: the work may continue through a lost connection.
- **Tenant.** The party a request is attributed to (labels, the sizing partition). kbf has no tenant concept in the scheduler today (not found in
  `crates/kbf-sched/src`).
- **Queued.** The one pre-lease phase of a request: submitted and waiting for capacity or fit (8.2), including waiting for a host to
  boot. There is no separate phase for a request held back by a tenant cap or budget, because jobscheduler has none (N4).

## 4. What exists in kbf today

The account, with the file each statement was read from, is in [design/kbf-today.md](design/kbf-today.md), sections 4.1 to 4.6. What the
rest of this document relies on: the scheduler core is a pure state machine whose placement is queue, fit and rank, and whose leases live
while their worker is heard (4.1); a `Commit` effect is stored before `Start` is sent (4.2); the estimator and memory ladder are the
sizing service of section 7 (4.3); the daemon's wire, graded absence rule and self-fence are the model for the agent, and its leases die
with it, which for terminating instances is the normal case (4.4, D3); the simulation invariants are I1 to I16 and L1 to L4 (4.5); and
ucdk's launcher and Job properties UL1 to UL4 and UJ1 to UJ3, and its different `Provider` trait (D12), are in 4.6. The scheduler assumes
a fixed fleet: it avoids a worker earlier attempts ended on, and the largest live node bounds the ladder. Both now read a fleet that may
change under them (5.8).

## 5. Capacity providers and the agent

### 5.1 Roles
Three things take part, and each has one job.

- The **agent** (`js-agent`) runs on a machine. It dials out, says hello, heartbeats, and runs and stops containers. It is the only thing
  that runs work (5.2, 5.4).
- A **capacity provider** gets machines: it launches and terminates them, or lists the ones an operator owns. It never runs work (5.5).
- A **pool** is the scheduler's view of the machines of one provider that fit one shape: its hosts are the machines whose agents have said
  hello. The lease table books hosts from pools (5.9).

The core stays pure and holds no provider reference. An adapter on the edge feeds it inputs (`Capacity`, `Heartbeat`, `Report`, `Phase`)
from agent sessions and executes the `Start` effect; another drives the capacity providers from the pool scaler (5.8). The core has no
`Stop` effect today; stopping a lease the scheduler still holds is new (5.2, step 23).

### 5.2 The agent contract: one call shape
The agent is a stream, not a Rust trait. Its messages are the `jobscheduler.agent.v1` package (10.3):

- `Hello` (agent to scheduler): host id; pool name and provider identity (for a launched instance, its instance id); `agent_version`;
  capacity as a resource vector (`cpus`, `mem_gib`, `gpus`, lease-count cap); GPU facts; labels; capabilities; phase; the leases it holds.
- `Welcome` (scheduler to agent): epoch, heartbeat interval, and the pool the host was admitted to, or a refusal (5.8).
- `Heartbeat`: the leases the agent holds, including those whose result is not yet acknowledged.
- `Run` (scheduler or leasee to agent): start one container: image, arguments, limits, environment by reference, host mounts
  (read-only or read-write directories such as the shared input cache), the fence, labels, and the validity anchor below.
- `Stop` and `Cancel`: stop one lease; `Result` and `ResultAck`: one result, resent until acknowledged; `Usage` with a `basis`.

An agent runs only containers, with the limits the message states (`js-contain`, step 19), and holds no credential of the work (5.4). The
agent adapter must not invent answers, so its outcome table is the stream's:

| Call | Outcome |
|---|---|
| `start` | `Sent` or `NoStream`. The outcome is an event: `Started` (the lease is listed in a heartbeat), a `Result`, or `NeverStarted` (no Result and not listed past the validity bound). There is no Accepted or Refused(code). |
| `stop` | New in this design (step 23). Today `Cancel` only answers a heartbeat that lists a lease the scheduler no longer holds. Here `stop` is a retained intent, resent on every heartbeat that lists the lease. No read-back at call time. The outcome is an event: a Result, or the lease gone from the heartbeat. |
| `observe` | Reads the last heartbeat set and the stream identity; it is not a query. Listed is Present. A lease the heartbeat omits is Absent only once its `Start` is due (4.4): on the same session, `START_GRACE` after the `Start` went out; if the `Start` went to an earlier session of the same process, at once; if it went to another process, after `HANDOVER_GRACE`, or at once when that process is proven stopped. Before then the lease is pending, not Absent. The reason picks the rerun budget (4.4). Silence past the grace is the fence rule of 4.1, not Absent. |

The adapter's events are `Started | Result | Usage | PhaseChange | CapacityChange | NeverStarted | Lost`.

`StartSpec` carries: lease id; the payload reference (opaque to the core); the booking; a separate `limits` vector (what the agent must
enforce); `assignments` (named resources granted, such as a GPU index or a device id); `mounts`; `fence` (self-fence or run-on); `labels`;
a deadline for the attempt; and the validity anchor `heartbeat_seq` and `valid_for_ms` (the newest heartbeat the server had taken, and how
long after the daemon sent it the Start may still be acted on, measured on the daemon's clock alone; 0 means no bound). Today the daemon
receives booking only, derives limits itself, and receives one `device_id` (`worker.proto`); `limits`, `assignments`, `fence`, `labels`
and `mounts` are the additive fields of step 7.

**Start contract.** Before the first call for a request, the core refuses it unless: the wall-time limit is positive; the fence is stated
(requests resolve it at submit from the definition or from a default the class declares as data; the resolved value is in the run record,
so no default is silent; D6); labels are free of reserved keys (12.3); and retries are owned by the core and are never passed on. These
checks apply to every request of every class, kbf's included, and must change no kbf behaviour: the kbf adapter supplies a positive
wall-time limit and no reserved labels, and the `action` class declares kbf's fence. They are lease limits (12.1), not quota policy. A
provider may add refusals and may never relax these. Each rule has a conformance case (13.2). The merged `StartSpec::new` enforces the
wall-time and reserved-label rules; the fence and no-retries rules are not modelled yet (`Request` has no such field).

### 5.3 What each part generalises
The account of how capacity, capabilities, the start payload and result, the failure taxonomy and usage differ from what the kbf daemon
sends today is in [design/generalising.md](design/generalising.md). Two parts are needed here.

**Phases.** The generic phase set is the wire's. A host takes new leases only in `serving`; a launched host is `registering` from launch
until its first hello, and takes none.

| Phase | Meaning | New leases |
|---|---|---|
| registering | launched or listed, no hello yet (scheduler-side only) | no |
| serving | normal | yes |
| preparing, restoring | the node is switching sessions, tied to a whole-machine lease (`SESSION_SWITCH_GRACE`) | no |
| memory-pressure | only while the node holds a lease | no |
| draining | announced by the agent (SIGTERM): leases run to their end, nothing is requeued | no |
| quarantined | held out | no |

An unknown phase value is read as quarantined (fail closed), the one deliberate exception to "skip what you do not know", since skipping a
phase could place work on a node that is not serving. Agent-announced draining is distinct from operator cordon and drain (4.1).

**Failure taxonomy.** The classes of `crates/kbf-daemon/src/runtime.rs` are kept: Killed, Failed, Invalid, MissingInput, TimedOut,
OutOfMemory (own limit), BusyNode (node pressure), Lost (restarted or fenced). Spot or instance reclaim shows as the agent being Lost, or
as a capacity-provider `observe` of Absent with a reclaim reason; the work is rerun elsewhere and the platform-fault budget is spent (8.1),
as for BusyNode. The one mapping point to the wire stays `result_of` in `crates/kbf-daemon/src/lease.rs`.

### 5.4 The agent
The kbf daemon is the model. The agent dials out and holds one stream; `Hello` carries capacity, labels, GPUs and phase, `Welcome` carries
the epoch and heartbeat interval, `Heartbeat` lists the leases the agent holds, `Run`, `Stop` and `Cancel` come down, `Result` goes up and
is repeated until `ResultAck` (`crates/kbf-daemon/src/acks.rs`). Rules that carry over unchanged:

- A lease lives while the agent is heard and lists it. There is no renew call.
- A `Start` that arrives later than its validity window is refused by the agent, which then sends nothing; the scheduler gives the lease
  up as never started (`NeverStarted`).
- A `Cancel` for a lease the scheduler no longer holds is resent while the lease is listed. A lost Cancel is therefore not an error.
  (A stop of a lease the scheduler still holds is new; see `stop` in 5.2.)
- The heartbeat set includes leases whose result is not yet acknowledged. An agent that holds or re-adopts units lists all of them
  before its first heartbeat on a new stream; otherwise the scheduler may requeue the lease once its `Start` is due (4.4) and the work
  runs twice. The first heartbeat after re-adoption must be complete.
- The agent self-fences at 40 s of silence against 60 s of scheduler grace; the compile-time assertions in
  `crates/kbf-sched/src/fence.rs` keep the relation. This is also how a terminated or partitioned instance stops its work. A run-on lease
  is allowed only when the request says it is safe (D6).
- `ResultAck` values ACCEPTED, DUPLICATE, STALE, UNKNOWN_LEASE and MISMATCH are kept. One result is accepted per request, and only while
  the committed lease matches (`result_committed` in `crates/kbf-sched/src/scheduler.rs`).

What the agent trusts, and who trusts it:

- The agent obeys the peers it was told to obey: the scheduler on its control stream, and the leasee named in a lease (9.1) on the work
  session, authenticated by a token the scheduler minted for that lease. It obeys no other caller.
- The agent proves itself to the scheduler with the join credential of its pool (5.8). A credential admits a host to one pool only.
- The agent holds no storage credential, no registry credential and no REAPI type. Inputs reach a container as a mounted directory and
  outputs leave as a mounted directory. The component that fetches inputs, uploads outputs and reports to a frontend is the consumer's
  trusted executor (for kbf, `kbf-executor`, 9.2), which runs beside the agent on the same host or on the consumer's side of the session.
  The action container never holds a storage credential either.

### 5.5 Capacity providers
A capacity provider gets machines for a pool. It does not run work and never sees a request. A sketch, not a final signature; calls are
async at the edge:

```rust
trait CapacityProvider {
    fn id(&self) -> ProviderId;                              // stable value; the scheduler never branches on its kind
    async fn acquire(&self, a: AcquireSpec) -> AcquireAck;   // Accepted{instance} | Refused(code) | Unknown
    async fn release(&self, i: &InstanceId) -> ReleaseAck;   // terminate, then read back
    async fn list(&self, scope: Scope) -> Vec<Instance>;     // by pool or labels; for reaping and re-adoption
    async fn observe(&self, i: &InstanceId) -> Observed;     // Present | Absent | Unknown
    fn events(&self) -> Option<EventStream>;                 // optional: reclaim notices; a provider without them returns None
}
struct AcquireSpec { acquire_id: AcquireId, pool: PoolName, shape: Shape, labels: Labels }
struct Instance { id: InstanceId, acquire_id: Option<AcquireId>, pool: PoolName, labels: Labels, state: InstanceState }
```

Semantics, each with a conformance case (13.2):

- **Idempotent on the acquire id.** A second `acquire` with the same id answers `Accepted` with the same instance and launches nothing.
  The instance carries the acquire id as a tag.
- **`Accepted` means launched, not ready.** The host joins its pool when its agent says hello (5.8). Until then the instance is
  `registering` (5.3) and counts toward the pool's `max`.
- **`Refused(code)`** is a plain constant: `quota`, `capacity`, `shape`, `denied`. It is an answer. A cloud's quota error is reported
  here and is the platform's limit speaking; jobscheduler adds none (N4).
- **`Unknown`** is no answer (timeout, reset, throttle). It never launches a second instance and never frees one (5.6).
- **`release`** terminates, then reads the instance back, and reports what it read (`Gone`, `Terminating`, `Refused`, `Unknown`). It
  never claims more than was observed. Releasing an instance that is already gone is a release.
- **`list`** returns only instances carrying this scheduler's tags (12.3), narrowed by scope. It is the record after a restart: the
  scheduler re-adopts each listed instance that matches a pool and an acquire record, and the reaper releases the rest (12.1).
- **`observe`** is `Present`, `Absent` after an explicit not-found, or `Unknown`. Only `Absent` frees a booking.
- **Commit before acquire.** The lease table records an `acquire` intent (acquire id, pool, shape) durably before it calls
  `acquire`, as it records a lease `pending` before it calls `start` (4.1). A crash between the two leaves an intent that `list` resolves.
- **No call under the table lock.** `acquire` takes minutes. The table records the lease as `acquiring` and returns at once; the pool
  scaler (5.8) makes the call outside the lock and the result arrives as an event (D19).

| Call | Answers | Core action |
|---|---|---|
| `acquire` | `Accepted{instance}`; `Refused(code)`; `Unknown` | Accepted: record the instance, wait for hello. Refused: the pool cools down and the waiting leases stay queued. Unknown: keep the intent, `list` by acquire id before any retry. |
| `release` | `Gone`; `Terminating(state)`; `Refused`; `Unknown` | The host leaves the pool only after a read-back of Absent. Anything else is retried at the next tick. |
| `observe` | `Present(state)`; `Absent`; `Unknown` | Absent of an instance that held a lease is host-lost (5.6). Unknown changes nothing. |
| `list` | instances, or an error | An error is not an empty list: the reaper does nothing on an error. |

**Kinds.** The first two:

| Kind | `acquire` | `release` | `list` / `observe` | Identity of an instance |
|---|---|---|---|---|
| `static` | Claims a free host of the inventory that fits the shape: a host the operator lists, or one that registered itself (5.8). Launches nothing and answers at once. | Returns the host to the inventory. The machine is untouched. | The inventory; answered from memory or from heartbeats, never `Unknown` for a listed host. | the host id |
| `aws` | `RunInstances` from a launch template with the shape's instance type, in the pool's subnets, with the acquire id as the idempotency token and the tags of 12.3. | `TerminateInstances`, then `DescribeInstances` by id. | `DescribeInstances` filtered by tag; by id. A not-found or a terminated state is `Absent`; a throttle or an API error is `Unknown`. A spot reclaim is `Absent` with a reason. | the instance id and its tags |

Later kinds are named only, each a crate behind the trait: `gcp`, `k8s` (pods as machines, or nodes through a cluster autoscaler),
`vsphere`, `proxmox`, and bare metal (a static inventory with power control). None is designed here.

### 5.6 Failure and absence semantics
This is the most important part of the contract. A thing the scheduler holds can be in one of three observed states, and the core treats
them differently. It applies to two things: a *lease on an agent*, where the three are derived from the heartbeat stream (5.2), and an
*instance at a capacity provider*, where they are answers to `observe` (5.5).

| Observed | Meaning | Core action |
|---|---|---|
| Present | the lease is listed, or the provider confirms the instance, with a state | keep it; update state |
| Absent | the heartbeat omitted the lease with its `Start` due (5.2), or the provider answered that the instance does not exist | the lease or host is over: if no result was accepted, give up and rerun against the right budget |
| Unknown | no answer, a partial answer, or an error other than not-found | keep it; never free its resources; retry the observation; escalate to an alert after a configured count of unknown observations |

Rules:

- **Acquire outcome unknown.** A timed-out `acquire` leaves a durable intent. The provider looks the instance up by the acquire-id tag
  with `list` before any retry, and never launches a second instance for the same id. For an agent the intent is the lease record, and
  the validity bound ends it.
- **Release outcome.** `release` terminates, reads back, and claims no more than it read. An agent `stop` claims nothing at call time; its
  outcome is an event.
- **Agent silence is not absence.** Silence past the grace period triggers the fence rules of 4.1. An agent silent while its instance is
  `Present` at the provider is a partition and the agent self-fences. Silence while the instance is `Absent` means the host is gone.
- **A host leaves a pool only after `release` reads back Absent.** An instance gone underneath (reclaim, an operator terminating it) is
  `Absent` on `observe` or missing from `list`, and is treated as host-lost: its leases are given up and rerun (8.1).
- **Provider outage.** While a capacity provider is unreachable, its pool can neither grow nor shrink, and existing leases stay. While an
  agent's stream is down, no new leases are placed on its host and its leases keep their resources booked. This is the store-outage rule
  of `crates/kbf-sched/src/scheduler/outage.rs` applied per provider: expiry timers restart with a grace window on recovery.
- **Cordon and drain.** Operator `Cordon` and `Drain` apply to a host and are scheduler state, lost on a server restart. A wait caused
  only by a cordon is never refused as unservable, and a cordoned host is not released on idle. Agent-announced draining is a phase (5.3).
- **Poison and duplicates.** A lease lost `LOSS_REQUEUES` (3) times refuses the operation (4.1); the value becomes a per-class retry policy
  value (8.1). A second result for an accepted request is answered DUPLICATE and ignored; a result for a lease that is not the committed
  lease is STALE.

### 5.7 Configuration
The scheduler is configured by one file named by a flag (`--config`; R4), in the TOML style of `js-pool-static` (`deny_unknown_fields`,
unique names). Every value below is configuration; none is compiled in. A secret is a path to a file, never a value. The one environment
exception is the cloud's own credential chain (`AWS_*`, ambient role credentials), which R4 permits as a provider-standard variable.

```toml
[[shape]]                                   # what one host offers; pools and providers refer to it by name
name = "gpu-a100x8"
gpu_class = "nvidia-a100"
[shape.resources]
cpu_millis = 96000
memory_bytes = 1099511627776
gpus = 8
[shape.capabilities]
isa = "x86-64-v3"

[[provider]]                                # operator-listed machines (the merged --pool-config hosts)
name = "lab"
kind = "static"

[[host]]                                    # same fields as the merged file, plus the provider
provider = "lab"
id = "gpu-01"
shape = "gpu-a100x8"

[[provider]]                                # machines that register themselves (5.8)
name = "edge"
kind = "static"
register = true
join_token_file = "<path-to-token-file>"    # one secret per provider, held by the operator

[[provider]]                                # launches and terminates instances
name = "cloud"
kind = "aws"
enabled = false                             # a provider that bills an account starts disabled (12.4)
role_arn = "arn:aws:iam::<account-id>:role/<role-name>"
region = "<region>"
launch_template = "<launch-template-name>"  # the image, the agent bootstrap and the security group live here
subnets = ["<subnet-a>", "<subnet-b>"]
[provider.instance_types]                   # shape name to instance type
gpu-a100x8 = "<instance-type>"

[[pool]]                                    # provider + shape + min/max + labels
name = "gpu-lab"
provider = "lab"
shape = "gpu-a100x8"
class = "gpu-host"
labels = { tier = "gpu", site = "lab" }

[[pool]]
name = "gpu-burst"
provider = "cloud"
shape = "gpu-a100x8"
class = "gpu-host"
min = 0
max = 4
idle_timeout = "10m"                        # 5.8
boot_timeout = "15m"
labels = { tier = "gpu", site = "cloud" }
```

A consumer picks pools in its own configuration, not the scheduler's. For kbf, a sketch of the section that maps its action classes
to selectors (the file is kbf's; the scheduler never reads it):

```toml
[[scheduler.class]]
kbf_class = "whole_machine"                 # leases a whole gpu host from any pool labelled tier=gpu
class = "gpu-host"
labels = { tier = "gpu" }
```

Rules, enforced when the file loads and refused with the key and the line:

- Unknown keys are refused. Provider, shape and pool names are unique. A pool names an existing provider and shape. `min <= max`, and a
  static pool's `max` is at most the hosts its inventory can hold. A provider of a kind the binary lacks is refused.
- A provider with `enabled = false` loads, and its pools take no leases. A key that holds a secret (`token`, `secret`, `password`) takes a
  path; an inline value is refused.
- **The core names no provider kind.** A kind registers a constructor (`kind` string to a function that builds a `CapacityProvider` from its
  table). A new provider is a new crate `js-provider-<kind>` and one line in the binary's registry. A layering test checks that no crate
  below the binary names `aws` or any other kind (10.1).
- Adding a key is additive (R3). A scheduler reads an older file unchanged.

The merged `--pool-config` file keeps working: it is a configuration with one implicit `static` provider, one pool of class `gpu-host` and
the listed hosts, until a `--config` file is given (5.9).

### 5.8 Pools: scaling and registration
**Membership.** A host belongs to exactly one pool. It joins when its agent's hello is admitted (below) and leaves only by `release`
reading back Absent (5.6), or, for a static host, when the operator removes it.

**Scaling.** Scaling the machine count of a pool is its provider's job; the scheduler decides when to ask. The pool scaler runs in `tick`
(12.1) with an injected clock (13.5), so a seeded run scales deterministically. Per pool, each tick:

- *Acquire.* Let `demand` be the leases in `acquiring` (no free host fits) and `have` the hosts plus the instances in flight
  (`registering`). Acquire `max(min - have, demand - inflight)` instances, at most `acquire_batch`, and never beyond `max`; every acquire
  has a fresh acquire id. In-flight instances count toward `max`. A `Refused` starts a `cooldown` for the pool.
- *Boot timeout.* An instance still `registering` after `boot_timeout` is released and reaped; its leases, if any, stay `acquiring`.
- *Release.* A host with no lease, not cordoned and not draining, idle for `idle_timeout`, while the pool holds more than `min`, is
  released. A host holding a lease is never released. Ranking packs onto the fewest hosts (`FirstFit`, 6.5) so that idle hosts exist.
- *Static pools* do not launch. `min` and `max` then bound how many inventory hosts the pool claims; `idle_timeout` does not apply.

`max` is the only brake jobscheduler adds; there is no cost guardrail. A pool at `max` with a request no host can fit is unservable (8.2).
Cold start is paid by the queue unless the owner sets `min` above zero (D21).

**Registration.** A machine may join a static pool by saying hello, so that nothing needs listing in advance.

- *Identity.* The hello carries a host id (chosen by the operator or derived from the machine), the pool name, labels, the shape it
  offers, and `agent_version`.
- *Admission.* The scheduler admits a host only if: the pool exists and its provider has `register = true`; the hello carries the pool's
  join credential; the host id is not held by a live stream (a second stream for the same id replaces the first only after the first has
  been silent past the grace of 4.1); the reported shape satisfies the pool's declared shape (`Shape::satisfies`); and the pool holds
  fewer than `max` hosts. A refusal is a `Welcome` with a constant code and no host is created.
- *Credential.* A per-pool join token (D18): a secret kept in the file the provider names, presented in the hello, compared in constant
  time. One token admits to one pool. The file is read on every hello, so removing a token, or listing a host id to deny, takes effect at
  that host's next hello; to remove a connected host now, cordon and drain it. A client certificate per pool is the alternative (D18).
- *Aws instances.* The launch template holds no secret (K6). An instance proves itself with the cloud's signed instance identity, which
  the aws provider verifies and matches to a pending acquire id; the template names only where to find the scheduler.
- *Lifecycle.* A host that stops heartbeating is Unknown, its leases stay and the agent self-fences; past the grace it is Absent, its
  leases are given up (5.6), and the host stays listed as lost for a retention window before it is dropped. A restarted agent registers
  again with the same id and re-announces the leases it holds.
- *Source.* A `RegisteredAgentSource` of `CapacitySource` (5.9) answers from admitted agents and ignores the bookings it is given: the lease
  file records leases and agents re-announce them.

### 5.9 The merged code and what it becomes
The merged slice is a GPU-host lease service: the lease table (`js-lease`) books a whole host from a pool (`js-pool-static`) for a client
on a TTL, through the `js-provider` trait. That is level 1 of two-level leasing (9.1) already. What it lacks is the machine side: nothing
on `main` launches or terminates a machine, and there is no agent wire. The GPU-host service keeps working through every follow-up; each
change below is additive and leaves the `--pool-config` file, the routes and the lease file readable.

| Merged item | Becomes | Follow-up |
|---|---|---|
| `js-provider::Provider` (`start`, `stop`, `observe`, `capacity`) | The host-booking surface of a pool, renamed `Pool` (and `ProviderId` to `PoolId`): book a free host for a lease, release it, observe it, list its hosts (D16). It is not a provider kind. | Rename PR (step 23) |
| `Flavour`, `flavour()`, the doc "delegated flavour only" | Deleted. One kind of thing runs work. The doc is rewritten. | Same PR |
| `StartAck`, `StopAck`, `Observed`, `StartSpec::new` | Stay: cloud calls can time out, so the three answers stay. `AcquireAck` and `ReleaseAck` are added beside them; the fence and no-retries rules are still to model. | Step 23 |
| `HostCapacity` and `capacity()`; the `js-provider` crate | `capacity()` becomes `list`, with provider-side identity (instance id, tags). The crate gains `CapacityProvider` beside `Pool`. | Step 23 |
| `StaticPool` | Stays as the `static` capacity provider and its pool. Its listed hosts are one configuration source. `acquire` claims a free listed host; `release` returns it. It never launches. | Registered hosts: step 30 |
| `pool.seat()`, `Seated`, `Unseated` | Stay for the in-memory static pool. For aws, instance tags are the record, so seating becomes re-adoption by tag. | Step 32 |
| `CapacitySource` | The extension seam. Sources are added: `RegisteredAgentSource`, `AwsSource`. Its lock-held, synchronous call becomes asynchronous, because acquire is slow (D19). | Steps 29, 30, 32 |
| `js-provider-fake` | The delegated fake becomes the fake capacity provider (fake aws), with scripted launch failure, slow boot, unknown acquire, unknown terminate, instance loss, leaked instances and quota refusal. | Step 28 |
| `js-conformance` | The machinery stays: `Subject`, `Check::ALL`, `Verdict::NotApplicable`. Its checks are re-keyed to the capacity-provider and agent contracts; "delegated flavour" wording goes. The labels-on-created-resources follow-up named in its crate doc lands with the first provider that creates resources. | Steps 28, 32 |
| `js-lease` | Stays. Adds an `acquiring` state before `pending`, `held` and `releasing`, the pool name in the lease file (additive), and an acquire record. | Step 29 |
| `js-lease-server` | Stays. `POST /v1/leases` accepts a class and label selectors (additive); a registration endpoint serves agent hello; a pool-scaler tick runs beside the reaper. | Steps 29 to 33 |
| `js-cli` | Gains `--pool`, `--class` and `--label` on `acquire`. | Step 33 |

## 6. Placement

### 6.1 Pipeline
The pipeline is the one in `crates/kbf-sched/src/scheduler.rs`: a tick takes queued requests in queue order; for each, *fit* produces
eligible (provider, pool) pairs; *rank* picks one; the grant becomes a `Commit` effect. Three traits make each stage replaceable:

- `Queue`: today a bare ordered set referenced from submit, requeue, unplace, forget_queued, refusal and result_committed. Wrapping it in
  a type is step 4 because later policies (aging, fair turns) plug in there and nowhere else. Quotas are not such a policy: the platform enforces them (N4).
- `Fit`: eligibility and room for a request on a pool. Today `Servable::fit` and `Worker::has_room`.
- `Rank`: a total order over eligible pairs. Today the least `Load`.

Constraint on the first extraction: kbf's `Queue`, `Fit` and `Rank` are the existing code behind traits, with identical output. Anything
new is a separate PR with its own proof. The traits are frozen only after the capacity-provider spike of step 12 has exercised them.

### 6.2 Resource classes as data
`LeaseKind` is a closed enum (`Action`, `WholeMachine`, `Vm`) whose per-kind admission is a `match` inside `Worker::has_room`. The
generic form:

```rust
struct ResourceClass {
    id: ClassId,                   // a string, e.g. "action", "whole_machine"
    admission: Admission,          // see below
    runtime_tag: RuntimeTag,       // what the agent runs for it (container, native)
    exclusive_when: Option<Rule>,  // e.g. memory at least pool memory
    fence_default: Option<Fence>,  // a declared class field, resolved at submit and recorded (5.2)
}
enum Admission {
    Book(Dims),                    // gate on these dimensions; count all in the booked total; check the rest against pool size only
    BookAll,                       // book every dimension; no overcommit
    Whole,                         // needs an empty pool and books all of it
}
```

The class separates dimensions that *gate* admission from dimensions that are *counted* for rank. kbf maps `Action` to
`Book({cpu_millis, gpus, vms})` as the gate set, bounded by the pool's separate lease-count cap (section 3); memory is gated only
against the pool's total (a request above it does not fit; a request equal to it is exclusive), and the grant still counts the whole
request, memory included, in the pool's booked total, on which `Rank` (`Load`) reads the memory share. This is today's behaviour (4.1),
and a model that excluded memory from the counted set would not reproduce kbf's rank order. kbf maps `Vm` to `BookAll` (the macOS VM
design states "No overcommit of memory", `docs/design/macos-vms.md`), and `WholeMachine` to `Whole`. `Vm` is unshipped, so this mapping is a design intent. For ucdk Job classes the recommended admission is `BookAll` (D7); this is
booking, not reservation, and per class, so it does not conflict with R1.

**Selecting pools.** A class says how a lease is admitted; it does not say where. A level-1 lease request (9.1) carries a class and label
selectors, and the scheduler offers it the pools whose `class` is that class and whose labels satisfy every selector (equality on
each key; richer set selectors are additive later). The merged pool serves one class,
`gpu-host`, admitted `BookAll`, one whole host per lease; a share of a host needs `Book` admission on the pool (step 29). A request that
matches no pool is refused; one that matches a pool at `max` queues. The map from a consumer's own classes (kbf's `action`,
`whole_machine`, `vm`) to class and selectors is in the consumer's configuration (5.7, 9.4), never in the scheduler's.

### 6.3 Oversubscription and 6.4 Devices
Both are in [design/placement.md](design/placement.md). In short: the `Book(Dims)` form carries kbf's rule that memory is checked once
against the pool's total yet still counted in the booking, with the agent's hard limit and the pressure backstop as protection and a
rerun elsewhere as recovery, while classes that need isolation use `BookAll`. Named devices are assigned from `assignments`; at most one
device lease per agent and whole-device GPUs only (D10). For a machine lease the consumer applies both inside its lease (9.1). A
whole-machine request reserves a worker once queued; large non-whole requests have no reservation, and R1 forbids adding one.

### 6.5 Rank for elastic pools
Spread is right for a fixed fleet. In any pool that scales, idle machines are terminated, so packing matters: it leaves hosts idle that
the scaler can release (5.8). Rank is therefore a trait with two shipped implementations: `Spread` (today's `Load`) and `FirstFit`, chosen
per pool. An optional dedup key on a
request (absent: no coalescing) lets kbf keep its in-flight coalescing by action key (the `in_flight` map in `crates/kbf-sched/src/scheduler.rs`) without the core owning the table.

## 7. Request history and sizing

The sizing service is in [design/sizing.md](design/sizing.md), sections 7.1 to 7.6. It acts on individual requests, so under two-level
leasing it is level 2 (9.1). In short:

- **7.1 The generic key** is `(class, variant, partition, basis)`; values with different bases never pool.
- **7.2 Sizing policy.** The constants scattered in kbf become one `SizingPolicy` value; `SizingPolicy::kbf()` returns today's values and a
  property test asserts every estimator output is unchanged (step 14). `Declared` mode (ucdk) makes the declared size the booking and
  the ceiling.
- **7.3 One limit rule.** The scheduler computes the limit (today 1.5 x the booking + 512 MiB), sends it in `Start.limits`, and the
  estimator's censoring decision uses the same function.
- **7.4 Ladder per class.** An unseen request climbs its class's ladder; an out-of-memory kill moves to the next rung above the killed
  run's booking and below `cap_of(request)`. For a scaling pool the cap is the largest shape the pool can acquire.
- **7.5 and 7.6.** Rule R1 and the shipped estimator disagree (D4); learning happens only after the scheduler committed the result.

## 8. Retries, loss and run records

### 8.1 Retry policy
`RetryPolicy` has the four axes of 4.1 by name (own-limit, platform fault, not started, loss) and adds *task failure*; instance reclaim
spends the platform-fault budget. The portable `max_retries` of a ucdk Job (UJ1) counts every failed attempt on three axes together. The
account is in [design/retries-and-runs.md](design/retries-and-runs.md).

### 8.2 Phases of a request
Before a lease a request is *Queued* (waiting for capacity or fit). For a pool that scales, Queued includes waiting for a host to boot: the
lease table shows the machine lease as `acquiring`, and no separate request phase exists for it. Then Leased, Running, and Completed,
Failed or Refused (4.1). Queued time, boot included, counts toward `UNSERVABLE_WAIT`; a pool at `max` with no host that fits the request
is unservable. There is no Held phase: it would exist only for tenant caps and budgets, which jobscheduler does
not have (N4). A start-contract failure (12.1) is a refusal at submit, and the request is never queued.

### 8.3 Run records
A submit returns only after a durable run record exists, and its retention has a configurable floor (kept in the state store; the record
and its fields are in [design/retries-and-runs.md](design/retries-and-runs.md)).

## 9. kbf as the first consumer; the daemon splits

### 9.1 Two-level leasing
- **Level 1.** A consumer (kbf, the ucdk adapter, a person with `js-cli`) leases capacity from the scheduler: a machine, or a share of
  one. The request names a class, label selectors that pick pools (6.2), the shape it needs, and a TTL. The lease service of `js-lease`
  and `js-lease-server` is this level: acquire, renew, release, a TTL, a max wall time, a reaper. The answer carries the lease id, the host
  and pool, and what the consumer needs to reach the agent.
- **Level 2.** The consumer sends `Run` and `Stop` to the agent on its host (5.2). It makes no scheduler call per action.

Consequences:

- `Request`, `Fit` and `Rank` (6.1) apply at level 1: a machine lease queues, is fitted to a pool and a host, and is ranked. Queue, fit,
  rank, fence, TTL and the start contract are level 1. The sizing history, the memory ladder and the per-attempt retry budgets (7, 8.1) act
  on individual requests, so they are level 2: kbf keeps them in `kbf-sched` until a per-request placement consumer is extracted (D15).
- R1's "a lease is a node or half a node" has its home: it is a level-1 lease. A whole host is `BookAll` or `Whole`; half a node is a
  share under `Book` admission. The merged table books whole hosts only; a share needs `Book` on the pool and an accounting of the booked
  share per lease (6.2, 6.3, step 29). The sub-question in D4 is closed by this.
- kbf's in-flight dedup, its queue and its placement stay in kbf. The scheduler never sees an action.
- When a lease ends or its instance is reclaimed, the scheduler stops the lease's work on the agent (a retained `Stop`, step 23); the agent
  also self-fences (5.4). The consumer sees the work session close or the lease lost, and reschedules its work onto another lease. A
  consumer drains before it releases.
- The scheduler's control stream and the consumer's work session are two sessions of the one agent wire. The scheduler tells the agent on
  its control stream which consumer holds the lease and a token minted for that lease; the agent dials the consumer, so the agent still
  only dials out (D22). The scheduler's own run service, which places ucdk requests (11), is a consumer in this sense.

### 9.2 The daemon splits three ways
Today's daemon (`crates/kbf-daemon/src/daemon.rs`) becomes:

- **`js-agent`**, generic: hello with capacity, labels and GPUs; heartbeat, session, acks, fence, drain and pressure; start and stop
  containers with limits through `js-contain`; mount host volumes, such as the shared input cache. It never sees storage credentials or
  REAPI types.
- **`kbf-executor`**, trusted and kbf-owned: fetches inputs from the CAS into a directory, asks the agent to run the container with that
  directory mounted, collects the outputs from a mounted directory, uploads them, and reports to the frontend. It holds the storage
  credentials. The action container does not.
- **kbf extras**: the storage map, pack counters and update status.

`kbf_daemon::Runtime` (`runtime.rs`) becomes the seam between executor and agent: the executor asks the agent to run a container, the
agent reports its result and usage. The kbf runtimes (container, native, fake, local) keep their implementations.
`kbf-driver-container`'s `delegate.rs`, `cgroup.rs`, `cgroup_usage.rs` and `numa.rs` move to `js-contain` with no REAPI types (step 19).
The kbf server is a consumer of machine leases (9.1), not a direct user of the scheduler's request type; a thin adapter turns an action
into work in a lease with `ActionKey` as payload.

### 9.3 What stays in kbf
REAPI front end (`crates/kbf-front`), the action cache write, in-flight dedup tables, placement of actions inside leased machines, the
queue of actions, the input cache and storage map, `kbf-executor`, `MissingInput` handling (the `Failure::MissingInput` mapping in
`crates/kbf-server/src/farm.rs` and `missing::inputs` in `crates/kbf-server/src/missing.rs`), CAS, packs, update plumbing, macOS
drivers, iOS device vocabulary and Xcode properties, the macOS logic that decides to send preparing or restoring (the phase values and
`SESSION_SWITCH_GRACE` live in the core, 5.3), the key derivation from remote execution commands, and the platform-property parser.

### 9.4 Mapping
| kbf today | Generic |
|---|---|
| `LeaseKind::Action` | class `action`, `Book({cpu_millis, gpus, vms})` plus the pool's lease-count cap (6.2), served by a container or native runtime |
| `LeaseKind::WholeMachine` | class `whole_machine`, `Whole`; at level 1 a machine lease of a pool's shape |
| `LeaseKind::Vm` | class `vm`, `BookAll` (unshipped) |
| a kbf worker pool in kbf's config | a selector (class and labels) that picks pools in the scheduler's configuration (5.7) |
| `ActionKey` | payload and optional dedup key |
| `hermetic` | class `action` declares self-fence as the default; a request marked `hermetic` resolves to run-on, and the resolved value is recorded |
| `Outcome::Completed{action_result}` | `Completed{result_blob}` |
| a daemon | `js-agent` on a host of a pool, with `kbf-executor` beside it; a lease is a machine, and actions run inside it |

## 10. The kbf / jobscheduler split

### 10.1 Principles
- Dependency runs one way: kbf depends on jobscheduler; jobscheduler never depends on kbf. ucdk depends on jobscheduler's public client
  API; jobscheduler never depends on ucdk. A layering test enforces both edges: no jobscheduler crate names a `kbf-*` or `ucdk-*` crate,
  in the style of `crates/kbf-integration-test/tests/layering.rs`. kbf's own layering test (pure set, forbidden list, clippy-deny check)
  is updated in the same PR that moves a pure crate, so the checks never go vacuous (step 16).
- A new provider is a new crate behind the `CapacityProvider` trait; the core and the lease server name no provider kind. A layering test
  fails if any crate below the binary names `aws` or another kind, and the binary's registry is the only place that lists them (5.7).
- jobscheduler is public and follows R5.
- Each step is a move or a trait introduction. Behaviour is pinned by the golden trace hashes, the golden persisted and wire corpus, and
  the simulation suite (section 13).

### 10.2 Crate map
| jobscheduler crate | From | Notes |
|---|---|---|
| `js-types` (merged) | generic half of `crates/kbf-types` | `Resources` as a named vector plus the separate lease-count cap, ids, `ResourceClass`, `Shape`, labels; later farm time, `Qos`, `NodePhase`, `StateMachine`, `Effect`, control record |
| `js-provider` (merged) | new | the `Pool` host-booking trait and, beside it, `CapacityProvider` (5.5, 5.9); `StartSpec` and its rules |
| `js-pool-static` (merged) | new | the `static` capacity provider and its pool: listed hosts, then registered hosts (5.7, 5.8) |
| `js-provider-fake` (merged) | new | the fake capacity provider, a fake aws (13.1) |
| `js-conformance` (merged) | new | both suites and their broken providers (13.2) |
| `js-lease` (merged) | new | the lease table: states, TTL, reaper, lease file; gains `acquiring` and the pool name (5.9) |
| `js-lease-server` (merged) | new | level 1 over HTTP; `CapacitySource`; gains registration and the pool-scaler tick |
| `js-cli` (merged) | new | the level-1 client; gains pool selection |
| `js-coverage` (merged) | new | the coverage ratchet (R6) |
| `js-config` | new | the configuration schema and loader of 5.7 |
| `js-provider-aws` | new | the `aws` capacity provider over a cloud-client trait with a fake (5.5); disabled by default |
| `js-provider-<kind>` | new | one crate per later kind; the core names none (5.7) |
| `js-sched` | `crates/kbf-sched` | per-request placement, generic over `Payload`, `ResultBlob`, `Fit`, `Rank`, `RetryPolicy`, `Queue`; keeps the fence and phase constants; scope per D15 |
| `js-caps` | `crates/kbf-caps/src/matching.rs` | schema-driven; kbf keeps its key tables and host detection |
| `js-sizing` | `crates/kbf-estimator` | `SizingPolicy`; resource-vector windows |
| `js-state` | the `Store` trait and conformance suite of `crates/kbf-state` | trait and fake only; SQLite and Postgres backends stay in kbf (D13) |
| `js-store` | generic rows of `farm/durable/*` | scheduling decisions over `js-state`; kbf extension rows stay |
| `js-agent` | generic half of `crates/kbf-daemon` | the agent of 5.2 and 5.4: session, leases, acks, fence, drain, pressure, hello and registration, start and stop containers with limits, host mounts |
| `js-contain` | `delegate.rs`, `cgroup.rs`, `cgroup_usage.rs`, `numa.rs` of `kbf-driver-container` | node-local containment; `js-agent` depends on it |
| `js-sim` | `crates/kbf-sim*` | invariants and replay |

`kbf-executor` is kbf's, not a jobscheduler crate (9.2). `js-sched`, `js-agent`, `js-contain` and `js-sim` are not on `main` yet.

`crates/kbf-types` today mixes generic types (`Resources`, lease id, `FarmTime`, `Qos`, `Effect`) with REAPI ones (`Digest`, failure
variants, `Answer`). Splitting it into two modules with re-exports is step 3, before anything moves.

### 10.3 Wire boundary
- New package `jobscheduler.agent.v1`: Hello, Welcome, Heartbeat, Start (the `Run` of 5.2), Cancel, Result, ResultAck, phase, usage, with an
  extension slot per message for payloads. It is the only agent wire, one call shape, and it also carries registration: `Hello` gains the
  host id, pool name, join credential, instance id, labels, GPU facts and `agent_version`; `Welcome` gains the admitted pool or a refusal
  code (5.8). The join credential travels in message metadata, never in a log line. `Start` gains `mounts`. `LeaseOffer` is not carried over: the daemon only logs it and acts on `Start` (4.4), so v1 drops
  it. The adapter keeps sending it on `kbf.worker.v1`, where old daemons expect it before `Start`.
- `kbf.worker.v1` is kept. The server hosts an adapter from v1 messages to generic messages, so the simulation and store see one shape.
- **Negotiation and release order.** Release order is: server and frontends first, daemons after. A new daemon always opens a v1 session
  first. Only when `Welcome` carries a new optional field (R3) advertising agent v1 does it close and reopen on the new service; a
  `Welcome` without the field means it stays on v1. Step 21b ships only after the release containing step 20 is the minimum server. An old
  server never sees the new package.
- `fence`, `limits`, `assignments`, `labels` and `mounts` are added to `Start` in *both* packages as optional fields (R3). Absent means
  today's behaviour: self-fence, derive limits locally, one device id, no labels, no mounts.
- Stale comment in `worker.proto` on `memory_bytes` is corrected in step 2.

### 10.4 How kbf keeps working at every step
Each PR leaves kbf as the sole implementer of any new trait; the golden trace hashes, the golden corpus, coverage, and the cross-version
matrix gate every step from 3 on; a wire change is optional-field only; rollback is a revert. The gates are in
[design/extraction-steps.md](design/extraction-steps.md).

## 11. ucdk Jobs on top

The full section is in [design/ucdk.md](design/ucdk.md), sections 11.1 to 11.3. What the rest of this document relies on:

- **11.1 Placement.** One jobscheduler instance is bound to one (app, environment). Its configuration declares that environment's
  providers and pools (5.7), and a Job runs as a container on a `js-agent` host of one of them. The scheduler holds standing cloud access,
  the role its aws provider assumes to launch and terminate instances; this replaces the earlier rule that it holds no standing
  credential. A caller's launch still acts with the caller's identity. Scheduled triggers, pool scaling, the reaper and the leftover sweep
  run with no caller present and use a delegated identity fixed when the definition or environment is created (K7). ucdk 1.0 keeps its
  direct launcher; jobscheduler is an optional second realization, supported only after a conformance kit passes on a real cloud. The
  launcher (`launch`, `status`, `logs`, `cancel`; UL1 to UL4) and the infrastructure provider live in ucdk and call the public client API.
- **11.2 Mapping.** A Job's image and arguments are the payload; its size is rounded up to a pool shape; `max_retries` is one budget over
  every failed attempt (8.1); the timeout is per attempt, enforced by the agent; a schedule trigger has one owner per realization.
- **11.3 Gaps.** The class declares the fence default (D6); the scheduler owns run history (8.3); a declared size is the booking and a hard
  ceiling (D7); the tenant is the (app, environment) pair from the authenticated caller (K7).

## 12. Lease limits, usage and labels

jobscheduler enforces no quota. Tenant caps, queue-depth caps, budgets, spend ceilings and dollar costs are not built here (N4): the
platform (Kubernetes quotas, cloud-provider quotas and instance limits) enforces them, and reaches jobscheduler only as a refused
`acquire` (`Refused(quota)`, 5.5). There is no cost guardrail; a pool's `max` is the only brake. What this section keeps is lease correctness and the records that sizing and cleanup need. State today: kbf has no
quota mechanism, and its "budgets" are retry and storage budgets. ucdk has shape limits only (attempts, timeout, argument size, size).

R1 forbids reservation and per-category admission for kbf farm work. Everything below applies to every class, none of it changes how kbf
places or runs work, and none of it is behind a flag.

### 12.1 Lease limits (always on)
Max wall time (required), max attempts, max resource size, max arguments size. These bound how long and how large one lease can be, so a
lease always ends: the wall time is the deadline of the attempt, and the fence (5.2) ends the lease when its agent cannot be heard.
Checked at submit, as part of the start contract (5.2), and refused there. A refusal carries a plain constant code; no central code table
is introduced until a second consumer needs one.

**Lease TTL and the reaper.** A held lease also has a TTL: the client renews it before it lapses, and a renewal never extends it past
its acquisition plus the max wall time. The reaper is a periodic tick, the only place time acts: it stops and frees every lease whose
TTL has passed, and retries a start, a stop, an acquire or a release whose outcome was unknown, freeing a lease only when the provider is
observed without it (5.6). So a client that disappears loses its lease at the end of its TTL, never later than its max wall time. The
lease table in this repository (`js-lease`) implements this.

The reaper also owns the machines. In the same tick it (a) releases a host that has been idle past its pool's `idle_timeout` while the
pool is above `min`, through `release`, and (b) releases a leaked instance: one that `list` returns, that carries this scheduler's tags,
and that no pool host or acquire record accounts for after a grace of ticks. A `list` that fails reaps nothing. Pool scaling (5.8) also
runs only in `tick`, so `js-lease` stays the one place time acts and scaling is deterministic under an injected clock.

### 12.2 Usage record (always on)
One record per lease: CPU-seconds, GB-seconds, GPU-seconds, peak memory with basis, wall phases, queue time, and whether it ran, was cached
or was joined. *Cached* (an action-cache hit) and *joined* (coalesced onto an in-flight run of the same key) are kbf-only record values;
other classes record *ran*. A machine lease also records the instance-hours its host ran, as attribution and not as a charge. Stored in the
state store, queryable, and feeding the sizing history (7). It is kept for sizing and for the run record (8.3), not for charging anyone. This is the "Accounting" record planned in `docs/design/scheduler.md`. Attempts charged to the
platform-fault budget (8.1) are recorded and flagged.

### 12.3 Labels (always on)
Every request carries labels. jobscheduler owns a small label schema and imports no ucdk type: a reserved prefix `jobscheduler.dev/` with
the keys `tenant`, `run`, `validation-run`, `definition`, `instance`, `pool`, `acquire` and `lease`, each value at most 63 characters of
`[a-z0-9._-]`. The caller mints the validation run id; the scheduler only carries and applies it. A user label under the reserved prefix
is refused at submit. Labels flow request, lease, `Start`, agent. A capacity provider tags each instance it launches with `pool`,
`acquire` and the scheduler instance, and the lease once one is bound, as `jobscheduler.dev/*` instance tags (aws), so `list` and the
reaper can find what is ours and attribution can read it. In ucdk the adapter translates the label set into `Stamp.extra`
(`ucdk/crates/ucdk-provider/src/stamp.rs`); a tagless kind cannot carry it, so a class that carries a validation run id is refused at
submit on such a kind unless its adapter defines a name rule that includes the id.

Runs are not nodes in any ledger, so the sweep key is the `instance` label. Destroying a Job definition, or the environment, cancels or
drains its runs and sweeps billable leftovers by that label; the sweep is part of the closed-world delete, with a test on the fake.

### 12.4 Real accounts need a go
A provider that bills a real account is registered disabled. A provider declared in configuration with such a kind stays disabled
(`enabled = false`, 5.7) until it is set to `true`, and that needs an explicit go for that action (R7). Conformance runs against
billable accounts only in a dedicated sandbox account with a billing alarm. The fake provider covers everything else.

## 13. Testing

### 13.1 Fake providers
The merged `js-provider-fake` becomes the fake capacity provider, a fake aws in memory with a controllable clock and scripted outcomes:
launch failure, slow boot, an `acquire` accepted but answered Unknown, a `release` answered Unknown, an instance that disappears
(reclaim), a leaked instance in `list`, a quota refusal, and a duplicate acquire id. A fake agent (in `js-sim`) models the agent
side: loss, fence, out-of-memory kill, node pressure, phase changes, unknown observations and slow starts. Starting points:
`kbf_daemon::Runtime`'s fake in `crates/kbf-daemon/src/runtime.rs` and `crates/kbf-sim-cell`. The fakes are the executable spec, as in
ucdk (`ucdk/crates/ucdk-provider-fake`).

### 13.2 Conformance suites
`js-conformance` keeps its machinery (`Subject`, `Check::ALL`, `Verdict::NotApplicable`, which lists a check a subject cannot take part
in and never skips it silently) and runs two suites, each over every implementation including the fakes.

*Capacity-provider suite:*

- `acquire` is idempotent on the acquire id; an Unknown `acquire` followed by a retry does not launch a second instance;
- `release` reads back; releasing an absent instance is fine; a `release` answered Unknown frees nothing; Absent frees a booking once;
- `list` returns only tagged instances, and a `list` error is not an empty list; tags are present on every created instance (the
  follow-up named in the `js-conformance` crate doc); a quota or capacity error maps to `Refused`.

*Agent suite:*

- start is idempotent on lease id; a `Cancel` is resent while the lease is listed and the lease leaves the heartbeat;
- a stale-lease result is refused; a duplicate is acknowledged and ignored;
- reconciliation frees an absent lease and keeps an unknown one; a lease omitted from a heartbeat is absent only once its `Start` is due
  (5.2, graded by session and process as in 4.4) and pending before then, and silence is not absence;
- a Start past its validity bound produces `NeverStarted`, no Result and no listing; the heartbeat lists unacknowledged results and the
  first heartbeat after re-adoption is complete;
- an unknown phase value is read as quarantined; usage is reported on failure and on kill;
- each start-contract rule (5.2) refuses before any call: non-positive wall time, unstated fence, reserved label key, retries passed on.

The hooks **read the real state**: for the fakes, their internal tables; for aws, an independent `DescribeInstances` made apart from the
adapter. A suite that asks the provider whether it did what it said cannot fail. The suite therefore ships one deliberately broken
provider per rule above, each asserting that the suite reports exactly that rule's failure, and a test that counts the broken providers
against the rule list so a new rule without one fails the build. These run on every CI run and are named in the suite's own test names.
Conformance against a real account runs only in a sandbox account, on the owner's go (12.4, R7).

### 13.3 Simulation
The scheduler invariants I1 to I16 and L1 to L4 are checked after every input in seeded sweeps. For the kbf instantiation, the golden
trace hashes (step 0) must be identical before and after each extraction PR. New invariants: an Unknown observation never frees a
booking; an Absent observation frees it exactly once; a pool never launches two instances for one acquire id; a pool never exceeds `max`
and is never below `min` while it has demand; a host holding a lease is never released; every instance in `list` is a pool host or is
being reaped.

### 13.4 Request-history replay
The replay harness of `kbf-estimator-replay` is the template; see [design/extraction-steps.md](design/extraction-steps.md).

### 13.5 No timing assertions
(R2.) Idle and boot timeouts, cooldowns, schedule ticks, timeouts, grace periods and retry budgets are tested with an injected clock that
advances only when the test says so, and by counting events (progress events, ticks applied). No test asserts an elapsed time or sleeps
against a timeout. The core already takes time as input (`Input{now, event}`). Adapters take a clock trait. A soak of real agents
counts explicit events (leases started, results acknowledged, zero unacknowledged) and asserts no elapsed time.

### 13.6 Coverage and compatibility
(R6, R3.) A crate in this repository has zero uncovered lines and branches, and a new crate enters at zero; `js-provider-aws` keeps its
cloud client behind a trait with a fake so the code that touches the network is minimal and covered. The lease file gains its pool name
and acquire records additively, and a test pins that the new server reads a file written before and that an older server skips the new
keys. The gates for moving kbf code (coverage by best instantiation, the golden persisted and wire corpus, the rolling-upgrade test) and
the request-history replay are in [design/extraction-steps.md](design/extraction-steps.md), with the gates of 10.4.

## 14. Extraction order

Each step is one small PR (or a short series, each green alone) that leaves kbf green; "Proves" says what its tests show; PR text opens
with what an operator sees. Steps 0 to 22 (golden gates, moves inside kbf, crates moving out, the wire and the agent) and their gates are
in [design/extraction-steps.md](design/extraction-steps.md). Steps 3 to 18 move kbf's per-request scheduler into `js-sched`; they apply
only if D15 keeps per-request placement in the core. The merged slice was built ahead of the ladder: `js-types` is the vocabulary of
step 16a; `js-provider` and `js-provider-fake` are the host-booking part of step 23 and the fake of step 12; `js-conformance` is step
20b's machinery; `js-lease`, `js-lease-server` and `js-cli` are level 1 (9.1), which the ladder did not anticipate.

**Phase D. New capability.**

23. Change `js-provider`: rename `Provider` to `Pool` and `ProviderId` to `PoolId`, delete `Flavour`, turn `capacity()` into `list`, add
    `CapacityProvider`, `AcquireAck`, `ReleaseAck` and `Instance` beside them (5.9), and model the fence and no-retries rules in
    `StartSpec`. Add the new `Stop` effect and its retained-intent row (5.1, 5.2). *Proves:* the merged tests pass unchanged apart from the
    names, so the GPU-host service is unchanged; a stop of a held lease is resent until the lease leaves the heartbeat.
24. Usage record per lease (12.2); labels (12.3). *Proves:* usage on failure; labels on every fake resource.
24b. The *task failure* retry axis of 8.1 and the portable `max_retries` budget. *Proves:* a UJ1/UJ2 case on the fake in which a
    non-zero exit is retried up to `max_retries` and a kbf class with budget zero is not retried.
25. Schedule triggers and run records; the launcher API. *Proves:* run record before return; missed ticks skipped; overlap rules, all
    with an injected clock.
26. ucdk side: the launcher wiring and, later, the provider crate, in the ucdk repository.

**Phase E. Capacity providers and pools.** Steps 27 to 29 need no agent. Step 30 needs steps 20 and 21a. Step 31 is tested on fake agents.

27. `js-config`: the schema of 5.7, the loader and its validation; the merged `--pool-config` file loads as one implicit provider.
    *Proves:* each rule of 5.7 refuses with the key and line; an old file loads unchanged; the layering test: no crate below the binary
    names a provider kind.
28. The fake capacity provider (13.1) and the capacity suite with its broken providers (13.2); `js-conformance` re-keyed. *Proves:* the
    suite is green on `static` and the fake and red on each broken provider.
29. The lease table: the `acquiring` state, the acquire record committed before the call, acquire off the table lock (D19), the pool
    name in the lease file, and share leases under `Book` admission. *Proves:* a crash between the record and the call resolves by `list`;
    a lease file written before the change loads; the GPU-host tests pass unchanged.
30. Agent registration in `js-agent` and the server: hello with identity and shape, admission, join credential, `RegisteredAgentSource`.
    *Proves:* a host with a wrong credential, an unknown pool, a duplicate id or a shape that does not satisfy is refused; a lost host's
    leases are given up once.
31. The pool scaler in `tick`: min, max, batch, cooldown, idle and boot timeouts. *Proves:* the invariants of 13.3 over seeds, with an
    injected clock and no elapsed-time assertion.
32. `js-provider-aws` against a fake cloud client, disabled by default; seating becomes re-adoption by tag. *Proves:* tags on every
    created instance; an Unknown `acquire` never launches twice; a restart re-adopts and reaps.
33. Consumer pool selection: class and label selectors in the lease request, and `--pool`, `--class`, `--label` in `js-cli`. *Proves:*
    no match is refused, a pool at max queues, a selector picks the right pool.
34. `kbf-executor` beside `js-agent`, with host mounts for the input cache (9.2). *Proves:* the action container holds no storage
    credential; the same seeded scenario yields the same event trace as before the split.
35. The first real aws pool, disabled by default; enabling it needs the go in R7.

## 15. Decisions

Each has options and a recommendation. None is final until the owner answers, except where marked settled. The ids run D1 to D22 with no gaps.

- **D1. Home of the code.** Settled: the code lives in this repository, where the merged crates already are. Crates extracted from kbf
  move in at step 22; until then they build in the kbf workspace, and kbf pins this repository by revision.
- **D2. Wire strategy.** (a) dual stack in the daemon; (b) a server-side adapter and a new package with negotiation; (c) edit v1 in
  place. (c) breaks R3. Recommend (b), daemon switched last and only after the server advertises the package. Registration rides the same
package (10.3).
- **D3. Durable leases across agent restart.** (a) required in v1; (b) not in v1, an agent capability flag. Recommend (b); kbf kills
  leases with the daemon today. An instance that terminates takes its leases with it, so "leases die with the host" is now the normal case.
- **D4. Sizing rule for seen work, and the ladder.** R1 says last peak plus buffer; the shipped estimator is a long-window maximum plus 20%
  and 128 MiB, and `on` mode applies only the kill-driven floor. Options: (a) implement R1 as `SeenRule::LastPeakPlusBuffer` and replay
  it; (b) keep the shipped estimator behind the generic API and amend R1; (c) both selectable per class. Where "a lease is a node or
  half a node" lives is settled: it is a level-1 machine lease (9.1). Second part: R1's "next rung above" is implemented as above the *booking*,
  not above the limit just used (7.4). Options: keep (current behaviour) or move to above-the-limit, which is a behaviour change with its
  own replay proof and not part of the extraction. Recommend (c) with (a) as the default after a replay supports it, and keeping the
  ladder as it ships.
- **D5. ucdk role.** (a) launcher only; (b) infrastructure provider only; (c) both. Recommend (c), launcher first, as an optional
  realization beside ucdk's direct launcher (11.1).
- **D6. Fence.** (a) self-fence for everything; (b) the class declares a default (self-fence unless the definition declares hermetic or
  idempotent) and every request resolves and records its fence at submit, so no default is silent. Recommend (b).
- **D7. Memory booking for non-kbf classes.** (a) same as kbf; (b) `BookAll` for ucdk job classes, with the declared size as booking and
  ceiling and history advisory only. Recommend (b); kbf actions keep today's rule.
- **D8. Validation run id and label key.** The caller mints; the scheduler carries and applies; the key is `jobscheduler.dev/validation-run`
  (12.3). Recommend yes.
- **D9. Triggers.** Cron-like triggers live in job definitions as data, with UJ3 semantics, when Job is realized by jobscheduler; the
  cloud scheduler role node is dropped in that realization and kept in the direct one (11.2). Recommend yes.
- **D10. Resources and capability schema.** Named resource vector in v1 with the lease-count cap separate; whole-device GPUs, no fractional
  GPUs. Capability schema: provider-registered, namespaced keys, unknown request keys refused, unknown report keys skipped. Recommend yes.
- **D11. Consensus.** The generic core relies on the state store's conditional writes for who places (`crates/kbf-sched/src/scheduler/handoff.rs`), not on a
  consensus log. `crates/kbf-raft` exists in kbf. Options: (a) the core requires only the store; (b) the core also supports a replicated
  log. Recommend (a); `kbf-raft` stays in kbf until a second user asks.
- **D12. Name collision.** jobscheduler's *capacity provider* and ucdk's *Provider* trait are different. Options: rename ours to *source*
  or *backend*; or keep and always qualify. The ucdk-side adapter lives in ucdk (11.1) and no jobscheduler code names ucdk's trait, so the
  clash is confined to prose. Recommend keeping "capacity provider" here and writing "infrastructure provider" for the ucdk side; the
  rename is the owner's option if the prose proves confusing. Inside this repository the merged trait `Provider` is renamed (D16).
- **D13. Where the state store lives.** `kbf-state` is general (thirteen kbf crates use it, 4.2). Options: (a) move it all into
  jobscheduler, which widens "kbf depends on jobscheduler" to installer, enrollment and history; (b) move only the trait, fake and
  conformance suite as `js-state` and keep the SQLite and Postgres backends in kbf; (c) keep it all in kbf behind a neutral crate.
  Recommend (b).
- **D14. Fleet scaling (reverses N2).** Settled by the project owner; recorded here with its reason. N2 said delegated providers scale
  inside their own service and jobscheduler does not scale fleets. With the delegated flavour dropped, nothing else scales a pool: a
  machine count held by hand is wrong whenever demand moves. So a pool's provider gets and gives machines between the pool's `min` and
  `max` (5.8). What does not change: `crates/kbf-coordinator` still only sets desired versions, and no cost guardrail is added (N4).
- **D15. Scope of the extracted core.** (a) The core is level 1 only: machines, small, with no per-request placement. (b) Level 1 is the
  base and per-request placement (`js-sched`, steps 3 to 18) is a consumer of it; kbf is a consumer too. (c) Per-request only, with level 1
  a client of it. Recommend (b): level 1 is merged and small, ucdk needs per-request launch, status and retries (11), and kbf keeps
  `kbf-sched` inside its leases until `js-sched` is proven. Steps 3 to 18 are conditional on this.
- **D16. Names.** (a) Rename the merged `Provider` to `Pool` and `ProviderId` to `PoolId`, delete `Flavour`, and add `CapacityProvider`;
  (b) keep `Provider` and add `CapacityProvider` beside it. Recommend (a): "provider" now means a capacity provider (3, D12), and two
  traits that differ by a prefix would be misread.
- **D17. Scaling policy.** Demand is the leases in `acquiring`, or that plus a look-ahead. Batch, cooldown and the two timeouts are
  per-pool configuration, unset meaning none. Recommend the simple form of 5.8 and no look-ahead until a trace shows a need.
- **D18. Self-registration auth.** (a) A per-pool join token; (b) a client certificate per pool, reusing the identity path of
  `crates/kbf-auth` and `crates/kbf-oidc`; (c) both, chosen per provider. Recommend (a) now, (b) added later without a wire change
  (R3). For aws, the cloud's signed instance identity, never a secret in the launch template.
- **D19. Acquire and the table lock.** The merged server calls the provider with the table lock held, so a slow call stalls every
  request. (a) Asynchronous acquire: the table records `acquiring` and returns, the scaler calls outside the lock, and the result is an
  event. (b) Keep the synchronous call with a timeout and answer every slow launch Unknown. Recommend (a).
- **D20. Config format and validation.** One TOML file, `deny_unknown_fields`, validated whole at load, secrets as paths, no reload in
  the first slice (a restart re-reads it). Alternatives: one file per provider, or YAML. Recommend one TOML file, as `js-pool-static` does.
- **D21. Who pays cold start.** (a) The queue: a request waits while an instance boots; (b) `min` warm hosts per pool. Recommend (a) as the
  default and `min` as the owner's per-pool choice; the wait counts toward `UNSERVABLE_WAIT` (8.2).
- **D22. Work-session topology.** (a) The agent dials the consumer; (b) the consumer dials the agent; (c) the scheduler relays. Recommend
  (a): agents only dial out, as the kbf daemon does today (4.4), and the scheduler stays out of the data path.

## 16. Risks

- **K1. The extraction changes kbf behaviour invisibly.** Mitigated by golden trace hashes (step 0), the golden persisted and wire
  corpus (step 1), golden outputs for the estimator and golden bytes for rows. Risk remains where the simulation does not cover a path;
  list uncovered paths per step.
- **K2. Capacity-provider absence handling is wrong.** Treating Unknown as Absent leaks an instance or terminates a busy host; retrying
  an `acquire` without a lookup launches twice and bills twice. Mitigated by the three-way rule, a durable acquire intent, the acquire-id
  tag, simulation invariants and a conformance suite that reads real state.
- **K3. A start-contract check changes kbf farm work.** Mitigated by the checks (5.2, 12.1) being written so that kbf requests pass them
  unchanged, and by the golden traces and corpus (steps 0 and 1) staying identical.
- **K4. Wire drift.** Two packages and an adapter can disagree. Mitigated by one conformance suite running through the adapter, by the
  negotiation rule and by the cross-version matrix at every wire step.
- **K5. One tenant fills a pool.** Without tenant caps in jobscheduler, one tenant's leases can drive a pool to its `max`. `max` is the
  bound; the cloud's quotas are the second, and they refuse an `acquire` (`Refused(quota)`). jobscheduler adds no third limit.
- **K6. Secrets.** `secret_env` references must never reach the run record, labels or logs. The record stores references only. No secret
  goes in user data or in an instance tag; an agent's join credential is short-lived or read from a file the host already has.
- **K7. Tenancy and identity.** The scheduler has no tenant concept (3, 4.1). A tenant must come from an authenticated caller, not a
  request field. kbf already authenticates frontends and nodes (`crates/kbf-auth`, `crates/kbf-oidc`); the design reuses that identity
  path and does not add a second. Scheduled triggers, pool scaling and the reaper run with no caller present, so they need a delegated
  identity. The scheduler also now holds standing cloud access: the role it assumes to launch and terminate. Scope it narrowly (launch
  from one template only, terminate only instances carrying the scheduler's tag). The tenant detail and the scope and rotation of these
  identities belong in a follow-up document due before step 25.
- **K8. Leaked instances.** A crash between `acquire` and its record, or a failed `release`, leaves a billing instance. Mitigated by the
  tag on every instance, `list`, the reaper and re-adoption after restart (5.5, 12.1).
- **K9. Launch storms and thrash.** Scaling up and down in quick succession wastes instance-hours. Mitigated by idle timeout, cooldown,
  batch size and `max` (5.8).
- **K10. Registration abuse.** A rogue agent joins a static pool and receives work. Mitigated by a per-pool credential, shape check, host
  limit and revocation (5.8), and by the lease token that binds an agent to one consumer (5.4).
- **K11. Cold start stalls consumers.** A request may wait minutes for an instance. Mitigated by `min` and the unservable wait (D21).
- **K12. Instance reclaim mid-lease.** The lease is given up, the work reruns against the platform-fault budget (8.1), and the consumer
  re-leases (9.1).
- **K13. Time.** Scaling that reads a clock outside `tick` is not reproducible; it runs only in `tick` with an injected clock (12.1, 13.5).
- **K14. Slow acquire under the table lock.** The merged server holds the lock across provider calls (`capacity.rs`); a cloud call would
  stall every request. Mitigated by asynchronous acquire (D19, step 29).
- **K15. Two-level leasing.** A consumer can double-book inside its lease, and the level-2 invariants are no longer the scheduler's. The
  agent bounds each run by its limits, so work cannot exceed the host; the consumer owns the rest, and its tests prove it.
