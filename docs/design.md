# jobscheduler: design

Status: design, with a first slice merged. Project: jobscheduler. First user: kbf. Second user: ucdk.
Audience: kbf and ucdk engineers, and the project owner who decides the open items in section 15.

This is the first document of the jobscheduler repository (komira-ai/jobscheduler). Paths such as `crates/...` and `docs/...` refer to the
kbf repository (komira-ai/kbf) at commit 45e420656, the commit this document was written against. Paths beginning `ucdk/` refer to the
ucdk repository (komira-ai/ucdk) at commit f28c240. Every statement about existing code names the file it was read from. A statement
marked (not found) means a search of the named place did not find the thing; it is a keyword result, not a proof of absence. Crates of
jobscheduler itself are named `js-*`; the map is in 10.2.

Code now exists on `main` of this repository: `js-types`, `js-provider`, `js-pool-static`, `js-provider-fake`, `js-conformance`, `js-lease`,
`js-lease-server`, `js-cli`, `js-coverage`, `js-agent-wire` (the agent wire, protocol major 1), `js-registry` and `js-agent` (session,
facts and a lifecycle over a `ContainerRuntime` trait; a docker and podman backend is in review). Section 5.9 says what each becomes; 10.2 marks which crates are merged. Everything else
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
3. **Providers get machines; pools are declared in configuration.** A capacity provider (`default`, aws, later others; `static` for tests
   only) offers `acquire(shape)`, `release`, `list` and `observe`. The configuration declares providers and pools (an ordered list of
   providers, shape, `max_machines`, labels); a consumer picks pools by class and labels; a new provider is a new crate (5.5, 5.7).
4. **Machines register themselves; capacity comes from an ordered hierarchy.** Every host runs `js-agent` and registers: the
   `DefaultProvider` is the hosts that registered and never creates or destroys compute; an elastic provider boots a machine whose agent
   then registers like any other. A pool lists providers in order and takes capacity from the first that can satisfy; it specifies only
   `max_machines`, and nothing is kept warm (5.8, [design/providers.md](design/providers.md)). Scaling reverses the former non-goal (D14).
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
    quotas already enforce them, and a pool's `max_machines` is the overload brake it adds. The usage record and the labels stay because
    sizing, cleanup and attribution need them (12).
13. **A machine serves many jobs; it is capped; it is kept clean.** After a job `js-agent` removes the container and scratch and frees the
    slot, queued work that fits runs on the same machine at once, and a machine is released only after an idle time. `max_machines` is
    never exceeded and growth is a few machines per tick; this is overload protection, not a cost guardrail (5.10, 5.11).
14. **A job declares `contained` or `host`; a machine can be tainted.** A host job taints its machine (the scheduler records this itself, not on the agent's report), `js-agent` checks after every job
    and taints on any finding, and a tainted machine takes no work and is replaced, never cleared on the agent's word (5.12).
15. **Capabilities are typed data; consumers translate into constraints.** An agent advertises scalars, version sets and counters; the
    scheduler matches equality, membership, version ranges and counters, and knows no consumer's words (5.13).
16. **A fresh VM per run, never reused.** The machine is reused, the guest is not: a VM is cloned, booted, used and destroyed for each
    run, and a lease may hold several runs. VMs share a host with containers under the caps; a whole-node class gives a performance test
    the host alone (5.14, 5.15).
17. **GPUs only on the GPU pool.** GPUs exist only on the Mac Studio machines of the GPU pool (`gpu_class` `apple-m`); a GPU request is
    satisfiable only there (D32, [design/capabilities.md](design/capabilities.md)).
18. **One service in Kubernetes.** The target is one jobscheduler service (scheduler, lease API, agent API) with a single writer on a
    volume; kbf is a client and `js-agent` registers with it. The single-host `js-lease-server` is the stepping stone (5.16, D33).
19. **The operator-listed pool is for testing; a request no host could ever serve is refused.** Production hosts register. A request
    that no registered host (and no elastic provider) could ever satisfy is refused with `no-capable-host`; one a busy host could serve
    waits (D35, settled; [design/providers.md](design/providers.md)).

### 1.1 What this amendment deletes and adds

Deleted: the delegated provider flavour and its former section 5.5 (`Flavour` leaves `js-provider`); the delegated-only unknown rules
(the delegated outcome-table column, its start and stop outcome rules, "silence is Unknown"), so the three-answer rule covers two places,
a lease on an agent and a machine at a capacity provider (5.6); non-goal N2 as written, the quota-ceiling cap of the ladder (7.4), the "no
standing cloud credential" rule of 11.1, and D1 as an open question; the kbf history (former section 4), moved unchanged to
`design/kbf-today.md`, with 5.3, 7, 11 and steps 0 to 22 moved to companions.

Added: the capacity-provider interface and the `static` and `aws` kinds (5.5); standing cloud access for the aws provider, the role it
assumes (K7, 12.4); `js-agent` as the one runner, with self-registration, per-pool join credentials and admission (5.4, 5.8, D18), and
`kbf-executor` beside it (9.2); two-level leasing and the `acquiring` state (9.1); config-driven providers and pools (5.7); pool scaling
up to `max_machines` (5.8); machine reuse, `max_machines` caps with a per-tick `max_step` and a consumer's own cap, and
the `contained`/`host` isolation with post-job checks and taint (5.10 to 5.12, [design/machines.md](design/machines.md)); the mapping of
the merged crates (5.9); the fake aws and the fake agent's taint paths (13.1); steps 27 to 35 (with 30b and 31b), decisions D14 to D25
and risks K8 to K18. (That amendment named the pool keys `min_machines` and `max_machines`; the last amendment below keeps only `max_machines`.)

Added by the capabilities amendment: typed capabilities, a constraint language and refusal codes (5.13,
[design/capabilities.md](design/capabilities.md)); a VM backend in `js-agent` with a fresh VM per run, the guest channel and VM integrity
(5.14); shared hosts and a whole-node class (5.15, [design/vm-backend.md](design/vm-backend.md)); the terms of section 3; steps 36 to 47;
decisions D26 to D31 (D26 open); risks K19 to K27. Text moved to companions to keep this file short: 5.6, 12.1 to 12.3 and steps 23 to 35.

Added by the refinement that followed: a fresh VM per run, the warm pool left as future work in [design/vm-backend.md](design/vm-backend.md);
GPUs only on the GPU pool (D32); one service in Kubernetes: objects, state store, high availability, upgrades and the move from the single
host (5.16, [design/deployment.md](design/deployment.md)); step 45 (now several runs per lease) and steps 48 to 53; D32 to D34; K28 to K31.

Added by the amendment after that (the owner's review of this design), text in [design/providers.md](design/providers.md): the `static`
pool, whose hosts the operator lists, is **for testing only**, and the GPU machines move to registering through `js-agent` (5.5, 5.9); a
`default` provider (the `DefaultProvider`: hosts that register themselves, never creating or destroying compute), with admission against
registered hosts that refuses a request no host could ever serve with `no-capable-host` and queues one a busy host could serve (D35,
settled); elastic providers whose launched machine runs `js-agent` and registers itself, the provider's job ending at boot plus registration;
a per-pool **provider hierarchy**, an ordered list that falls through only when an earlier entry cannot satisfy now (5.8); steps 30c, 31c and
54; D35 (settled by the owner; reading (a)) to D41; K32 to K34. Deleted: `min_machines` everywhere (the key, its validation, the scaling rule that kept a floor, the floor
choice of D21 and D23(c), the mitigation in K11): pools specify `max_machines` only, and nothing is kept warm. The `register = true` provider flag becomes the `default` kind.

Added by the rollout amendment: D35 settled by the owner (reading (a); the other reading is recorded as considered and rejected); section 17
and [design/rollout.md](design/rollout.md), rolling out the server, the agents and kbf as two services without generalising kbf's update
coordinator; D42 to D45; K35 and K36; step 55.

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
- G8. A pool's machine count, where its providers can launch, is the scheduler's to scale up to `max_machines`, with no floor (5.8).
- G9. Providers and pools are configuration; a new provider is a new crate behind the trait (5.7).

Non-goals:

- N1. Preemption and fair-share between tenants. kbf has neither (`crates/kbf-sched/src/lib.rs`, the "Not yet" list;
  `docs/design/scheduler.md`). The design adds the seam (the `Queue` type) and no policy.
- N2. Fleet version management. It stays in `crates/kbf-coordinator`, which sets desired versions for host updaters and is not a capacity
  scaler (`crates/kbf-coordinator/src/lib.rs`).
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
- **Capacity provider.** A driver that supplies a pool's machines. Kinds: `default` (hosts that registered themselves; creates nothing),
  the elastic kinds that launch and terminate machines (`aws`, later `gcp`, `k8s`, `vsphere`, `proxmox`), and `static` (listed hosts, for
  tests only) (5.5). It never runs work. Plain "provider" in this document means a capacity provider.
- **DefaultProvider.** The `default` kind: the hosts that registered themselves with the scheduler. It never creates or destroys compute.
  **Elastic provider:** a kind whose `acquire` boots a machine that runs `js-agent` and registers itself.
  **Hierarchy:** a pool's ordered list of providers; capacity is taken from the first that can satisfy (5.8).
  **Capable host:** a registered host that could serve a request once free (providers.md).
- **Shape.** What one host offers: a resource vector plus capabilities (and GPU facts). A pool has one shape.
- **Pool.** Declared in configuration: an ordered list of providers, a shape, a `max_machines` count of hosts, and labels (5.7). The scheduler books hosts from pools.
- **Isolation.** What a job declares in `StartSpec`: `contained` (a container with the message's limits; the default) or `host` (it may
  change the machine). **Tainted:** a host that ran a host job or failed a post-job check; it takes no new work and is replaced (5.12).
  **Idle time:** how long a host holds no lease and no job before it is released (`idle_timeout`, 5.10).
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
  the same way (`crates/kbf-caps`). A value is a `text`, a `version`, a set of either, or a `count` (5.13). On the merged wire they are
  string pairs today; the typed form is an additive field.
- **Constraint.** One need of a request over one capability: key, op, value and scope (`host` or `guest`). A request's constraints all
  hold or the host does not match (5.13).
- **Base image, VM, run.** A *base image* is a named, immutable disk image primed by an operator, with a profile of facts. A *run* is one
  execution of a job inside a lease; a lease may hold several. A *VM* is a fresh clone of a base image, booted for one run and destroyed
  after it; it is never reused, not even by the next run of the same lease (5.14).
- **Whole-node lease.** The lease class `whole_machine`: the host is quiet and exclusive for the lease, with no VM running (5.15).
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
- A **capacity provider** supplies machines: the `default` provider is the hosts that registered themselves, an elastic one launches and
  terminates them and the machine registers itself. It never runs work (5.5).
- A **pool** is the scheduler's view of the machines of one provider that fit one shape: its hosts are the machines whose agents have said
  hello. The lease table books hosts from pools (5.9).

On the agent wire a lease is granted by one party and the rules of 5.4 apply per session to that party. The control stream's `Heartbeat`
lists machine leases (granted by the scheduler); a work session's lists placements (term, sequence), granted by the leasee. The scheduler
never cancels a placement it did not make. When a machine lease is given up the scheduler tells the consumer, which reruns its placements
(8.1, 9.1).

The core stays pure and holds no provider reference. An adapter on the edge feeds it inputs (`Capacity`, `Heartbeat`, `Report`, `Phase`)
from agent sessions and executes the `Start` effect; another drives the capacity providers from the pool scaler (5.8). The core has no
`Stop` effect today; stopping a lease the scheduler still holds is new (5.2, step 23).

### 5.2 The agent contract: one call shape
The agent is a stream, not a Rust trait. Its messages are the `jobscheduler.agent.v1` package (10.3):

- `Hello` (agent to scheduler): host id; pool name and provider identity (for a launched instance, its instance id); `agent_version`;
  capacity as a resource vector (`cpus`, `mem_gib`, `gpus`, lease-count cap); GPU facts; labels; capabilities (string pairs today;
  typed `facts`, refreshed by heartbeat, are the additive form of 5.13); phase; the leases it holds.
  Proof of admission is not a field: it travels in message metadata (5.8).
- `Welcome` (scheduler to agent): epoch, heartbeat interval, and the pool the host was admitted to, or a refusal (5.8).
- `Heartbeat`: the leases or placements the agent holds on that session (5.1), including those whose result is not yet acknowledged; and,
  when the host is tainted, the taint (reason, job, check) until the scheduler acknowledges it (5.12).
- `Start` (scheduler or leasee to agent; `StartSpec` is its body): start one container: image, arguments, limits, environment by
  reference, host mounts (directories under the agent's configured roots, 5.4), the fence, labels, and the validity anchor below.
- `Stop` and `Cancel`: stop one lease; `Result` and `ResultAck`: one result, resent until acknowledged; `Usage` with a `basis`.

An agent runs only containers, with the limits the message states (`js-contain`, step 19), or, when it has a VM backend, a fresh VM (5.14),
and holds no credential of the work (5.4). The
agent adapter must not invent answers, so its outcome table is the stream's:

| Call | Outcome |
|---|---|
| `start` | `Sent` or `NoStream`. The outcome is an event: `Started` (the lease is listed in a heartbeat), a `Result`, or `NeverStarted` (no Result and not listed past the validity bound). There is no Accepted or Refused(code). |
| `stop` | New in this design (step 23). Today `Cancel` only answers a heartbeat that lists a lease the scheduler no longer holds. Here `stop` is a retained intent, resent on every heartbeat that lists the lease. No read-back at call time. The outcome is an event: a Result, or the lease gone from the heartbeat. |
| `observe` | Reads the last heartbeat set and the stream identity; it is not a query. Listed is Present. A lease the heartbeat omits is Absent only once its `Start` is due (4.4): on the same session, `START_GRACE` after the `Start` went out; if the `Start` went to an earlier session of the same process, at once; if it went to another process, after `HANDOVER_GRACE`, or at once when that process is proven stopped. Before then the lease is pending, not Absent. The reason picks the rerun budget (4.4). Silence past the grace is the fence rule of 4.1, not Absent. |

The adapter's events are `Started | Result | Usage | PhaseChange | CapacityChange | NeverStarted | Lost`.

`StartSpec` carries: lease id; the payload reference (opaque to the core); the booking; a separate `limits` vector (what the agent must
enforce); `assignments` (named resources granted, such as a GPU index or a device id); `mounts`; `fence` (self-fence or run-on); `isolation`
(`contained` or `host`; absent means `contained`; a `host` job needs three consents, 5.12); `labels`;
a deadline for the attempt; and the validity anchor `heartbeat_seq` and `valid_for_ms` (the newest heartbeat the server had taken, and how
long after the daemon sent it the Start may still be acted on, measured on the daemon's clock alone; 0 means no bound). Today the daemon
receives booking only, derives limits itself, and receives one `device_id` (`worker.proto`); `limits`, `assignments`, `fence`, `labels`,
`mounts` and `isolation` are the additive fields of step 7.

**Start contract.** Before the first call for a request, the core refuses it unless: the wall-time limit is positive; the fence is stated
(requests resolve it at submit from the definition or from a default the class declares as data; the resolved value is in the run record,
so no default is silent; D6); labels are free of reserved keys (12.3); and retries are owned by the core and are never passed on. These
checks apply to every request of every class, kbf's included, and must change no kbf behaviour: the kbf adapter supplies a positive
wall-time limit and no reserved labels, and the `action` class declares kbf's fence. They are lease limits (12.1), not quota policy. No
agent, executor or pool may relax these; a pool may add refusals. Each rule has a conformance case (13.2). The merged `StartSpec::new` enforces the
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
| tainted | the scheduler granted a `host_jobs` lease or sent a host `Start`, or a post-job check found something (5.12); sticky until the provider replaces the host or an operator clears it | no |

An unknown phase value is read as quarantined (fail closed), the one deliberate exception to "skip what you do not know", since skipping a
phase could place work on a node that is not serving. Agent-announced draining is distinct from operator cordon and drain (4.1).

**Failure taxonomy.** The classes of `crates/kbf-daemon/src/runtime.rs` are kept: Killed, Failed, Invalid, MissingInput, TimedOut,
OutOfMemory (own limit), BusyNode (node pressure), Lost (restarted or fenced). Spot or instance reclaim shows as the agent being Lost, or
as a capacity-provider `observe` of Absent with a reclaim reason; the work is rerun elsewhere and the platform-fault budget is spent (8.1),
as for BusyNode. The one mapping point to the wire stays `result_of` in `crates/kbf-daemon/src/lease.rs`.

### 5.4 The agent
The kbf daemon is the model. The agent dials out and holds one stream; `Hello` carries capacity, labels, GPUs and phase, `Welcome` carries
the epoch and heartbeat interval, `Heartbeat` lists the leases the agent holds, `Start`, `Stop` and `Cancel` come down, `Result` goes up and
is repeated until `ResultAck` (`crates/kbf-daemon/src/acks.rs`). Rules that carry over, each applied per session to the party that
granted the lease (5.1):

- A lease lives while the agent is heard and lists it. There is no renew call.
- **After every job** the agent removes the container and the scratch directory, runs the post-job checks, and only then frees the slot
  (the unit leaves the heartbeat and the booking returns to the host). Queued work that fits is placed on the same machine at once. A
  cleanup failure or a finding taints the host instead of freeing the slot; a host job's machine was already tainted by the scheduler (5.10, 5.12).
- A `Start` that arrives later than its validity window is refused by the agent, which then sends nothing; the scheduler gives the lease
  up as never started (`NeverStarted`).
- A `Cancel` for a lease the scheduler no longer holds is resent while the lease is listed. A lost Cancel is therefore not an error.
  (A stop of a lease the scheduler still holds is new; see `stop` in 5.2.)
- The heartbeat set includes leases whose result is not yet acknowledged. An agent that holds or re-adopts units lists all of them
  before its first heartbeat on a new stream; otherwise the scheduler may requeue the lease once its `Start` is due (4.4) and the work
  runs twice. The first heartbeat after re-adoption must be complete.
- Self-fence is per session. Silence on a work session fences that leasee's placements at 40 s against 60 s of grace; the compile-time
  assertions in `crates/kbf-sched/src/fence.rs` keep the relation. Silence on the control stream stops new leases and starts the
  machine-lease TTL clock; it kills a placement only once that TTL has also passed, so a consumer that keeps renewing keeps its work
  through a scheduler outage. This is also how a terminated or partitioned instance stops its work. A run-on lease is allowed only when
  the request says it is safe (D6).
- `ResultAck` values ACCEPTED, DUPLICATE, STALE, UNKNOWN_LEASE and MISMATCH are kept. One result is accepted per request, and only while
  the committed lease matches (`result_committed` in `crates/kbf-sched/src/scheduler.rs`).

What the agent trusts, and who trusts it:

- The agent obeys the peers it was told to obey: the scheduler on its control stream, and the leasee named in a lease (9.1) on the work
  session, authenticated by a token the scheduler minted for that lease. It obeys no other caller.
- The agent proves itself to the scheduler with the join credential of its pool, or on aws the instance proof (5.8), and sends it only
  on a TLS stream whose server identity it has pinned. A credential admits a host to one pool only.
- The agent holds no storage credential, no registry credential and no REAPI type. Inputs reach a container as a mounted directory and
  outputs leave as a mounted directory. The component that fetches inputs, uploads outputs and reports to a frontend is the consumer's
  trusted executor (for kbf, `kbf-executor`, 9.2). It runs on the host beside the agent, in its own process and identity, and shares the
  mounted input, output and cache directories with it; it is part of the host image or package, so the aws launch template and the
  updater carry it. The agent sends `Result` and `Usage` on the work session; the executor reports to the frontend.
- The agent mounts only paths under roots its own local configuration lists, never a path from a message alone. The shared input cache is
  read-only in containers and read-write only for the executor. The executor's credentials live outside every listed root, readable
  only by the executor's user. A container's network namespace reaches neither the instance metadata endpoint nor the executor's control
  socket. So the action container never holds a storage credential.
- A compromised agent can report results only for leases committed to its own host id, run the work placed on it, and misreport usage
  (which feeds sizing). It cannot obtain another host's lease token, join another pool or hold storage credentials.

### 5.5 Capacity providers
A capacity provider supplies machines for a pool. It does not run work and never sees a request. A sketch, not a final signature; calls
are async at the edge:

```rust
trait CapacityProvider {
    fn id(&self) -> ProviderId;                              // stable value; the scheduler never branches on its kind
    fn can_launch(&self) -> bool;                            // false for `default` and `static`: the scheduler never calls acquire on them
    fn verify_instance(&self, hello: &Hello) -> Option<InstanceId>; // elastic kinds: match a hello to a launched instance (5.8)
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
- **`Accepted` means launched, not ready.** The machine's own `js-agent` registers it: the host joins its pool when the agent says hello
  with proof that matches the acquire (`verify_instance`, 5.8). The provider's job ends at that hello, or at failure or timeout. Until
  then the instance is `registering` (5.3) and counts toward the pool's `max_machines` from the moment its intent is recorded (5.11).
- **`Refused(code)`** is a plain constant: `quota`, `capacity`, `shape`, `denied`. It is an answer. A cloud's quota error is reported
  here and is the platform's limit speaking; jobscheduler adds none (N4).
- **`Unknown`** is no answer (timeout, reset, throttle). It never launches a second instance and never frees one (5.6).
- **`release`** terminates, then reads the instance back, and reports what it read (`Gone`, `Terminating`, `Refused`, `Unknown`). It
  never claims more than was observed. Releasing an instance that is already gone is a release.
- **`list`** returns only instances carrying this scheduler's tags (12.3), with this scheduler's instance id in the filter, narrowed by
  scope. It is the record after a restart: the
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

**Kinds.** Three, fully in [design/providers.md](design/providers.md):

| Kind | `acquire` | `release` | `list` / `observe` | Identity of an instance |
|---|---|---|---|---|
| `default` | `Refused(capacity)`: it creates nothing and is never asked. Hosts register themselves (5.8); the `DefaultProvider` is the core of the merged `js-registry` (5.9). | Terminates nothing; a host leaves by deregistering, removal or age-out. | The host table. `observe`: Present while heard; Unknown on silence; Absent only on explicit deregister or operator removal. | the host id |
| `static` (**testing only**) | As `default`, over hosts the operator lists in the file. Production configurations declare none. | As `default`. | The listed hosts; the same observe rule. | the host id |
| `aws` | `RunInstances` from a launch template with the shape's instance type, in the pool's subnets, with the acquire id as the idempotency token and the tags of 12.3. The role's scope is in 12.4. | `TerminateInstances`, then `DescribeInstances` by id. | `DescribeInstances` filtered by tag; by id. A not-found or a terminated state is `Absent`; a throttle or an API error is `Unknown`. A spot reclaim is `Absent` with a reason. | the instance id and its tags |

`aws` is an elastic provider: it boots a machine that runs `js-agent`, which registers itself, and the machine is then an ordinary host of
the pool (5.8). Later elastic kinds are named only, each a crate behind the trait: `gcp`, `k8s` (pods as machines, or nodes through a
cluster autoscaler), `vsphere`, `proxmox`, and bare metal with power control. None is designed here.

### 5.6 Failure and absence semantics
Text in [design/absence.md](design/absence.md). A thing the scheduler holds (a lease on an agent, derived from the heartbeat stream; an
instance at a capacity provider, from `observe`) is Present, Absent or Unknown. Only Absent frees a booking; Unknown never does. An
unknown `acquire` leaves a durable intent and is looked up by its tag before any retry; agent silence is not absence; a host leaves a pool
only after `release` reads back Absent; a provider outage freezes its pool; a cordon is scheduler state; a lease lost three times refuses
the operation; a VM that vanishes under a run is a failed run (5.14).

### 5.7 Configuration
The scheduler is configured by one file named by a flag (`--config`; R4), in the TOML style of `js-pool-static` (`deny_unknown_fields`,
unique names). Every value is configuration; none is compiled in. A secret is a path to a file, never a value. The one environment
exception is the cloud's own credential chain (`AWS_*`, ambient role credentials), which R4 permits as a provider-standard variable.

A full example (shapes, a `default` provider, an aws provider, pools with a hierarchy, and a test-only static provider) and the section of
kbf's own file that maps its classes to pool selectors are in [design/configuration.md](design/configuration.md). A consumer picks pools
in its own configuration, never the scheduler's. A pool is `name`, `providers` (the ordered hierarchy; `provider` is shorthand, 5.8),
`shape`, `class`, `labels`, `max_machines`, `provider_max_machines`, `max_step`, `idle_timeout`, `cooldown`, `boot_timeout`,
`max_boot_failures`, `max_taints_in_a_row`, `regain_grace`, `allow_host_jobs` (default false) and, to admit registering hosts, `join_token_file`.

Rules, enforced when the file loads and refused with the key and the line:

- Unknown keys are refused. Provider, shape and pool names are unique. A pool names existing providers (each at most once) and an existing
  shape. **There is no `min_machines`:** the key, and `min`, are refused with a message saying a pool specifies `max_machines` only. A
  pool must set `max_machines`, at least 1, unless its providers are all `static` (test only), when it defaults to the hosts listed and may
  not exceed them. `provider_max_machines` names providers of the pool, each at most `max_machines`. `max_step` defaults to 2 (at least 1,
  at most `max_machines`). A `static` provider in a file without the `--allow-test-providers` flag is refused. A
  provider of a kind the binary lacks is refused.
- A provider with `enabled = false` loads, and its pools take no leases. A key that holds a secret (`token`, `secret`, `password`) takes a
  path; an inline value is refused.
- **The core names no provider kind.** A kind registers a constructor (`kind` string to a function that builds a `CapacityProvider` from its
  table). A new provider is a new crate `js-provider-<kind>` and one line in the binary's registry. A layering test checks that no crate
  below the binary names `aws` or any other kind (10.1).
- Adding a key is additive (R3). A scheduler reads an older file unchanged.

The merged `--pool-config` file keeps working as a **stepping stone**: it is a configuration with one implicit `static` provider, one pool
of class `gpu-host` and the listed hosts, until a `--config` file is given (5.9). It is the operator-listed pool of the test provider, and
it goes away when the GPU machines run `js-agent` and register (step 54). The `--allow-test-providers` gate applies to a `--config`
file; the merged `--pool-config` path needs no new flag until then.

### 5.8 Pools: scaling and registration
**Membership.** A host belongs to exactly one pool. It joins when its agent's hello is admitted (registration.md) and leaves only by `release`
reading back Absent (5.6), or, for a `default` host, when its agent deregisters or the operator removes or denies it.

**The hierarchy.** A pool has an ordered list of providers, `providers`, and takes capacity from the first that can satisfy. The default
is the `default` provider (registered hosts), then the elastic provider. The scheduler first uses any free capable host, in hierarchy
order; only when none exists and none is booting does it ask the first elastic entry that *can satisfy by acquiring* (an acquire is
possible under its `provider_max_machines` share and the pool's `max_machines`, and it is enabled and not cooling down) to launch a machine.
"Cannot satisfy now" means exactly: no free capable host, and no acquire possible. A busy registered host does not satisfy a request now,
so the request falls through to the elastic entry; with every entry exhausted it waits (8.2). If a host frees while a later entry's
machine is booting, the request goes to the freed host and the booting machine is kept and released by the idle rule. The rules, the
caps, the admission of a request against registered hosts (D35) and the elastic-registration flow are in
[design/providers.md](design/providers.md).

**Scaling.** Scaling a pool that has an elastic entry is the scheduler's decision and the provider's act; a pool of `default` hosts only
is not scaled (it creates and destroys nothing). **Nothing is kept warm: there is no minimum, and a pool holds zero machines of an elastic
entry when idle.** The pool scaler runs in `tick` (12.1) with an injected clock (13.5), so a seeded run scales deterministically. Per
pool, each tick:

- *Acquire.* Let `demand` be the leases in `acquiring` that no free capable host in any entry, no host about to be freed and no `registering`
  instance can serve (queued work that fits is placed first, 5.10), and `inflight` the instances being launched. Acquire
  `demand - inflight` instances, in the entry chosen by the hierarchy, at most `max_step`, and never beyond `max_machines`, which counts
  every machine the pool owns including in-flight acquires and tainted hosts (5.11); every acquire has a fresh acquire id. Demand is in
  machines' worth of the shape (once share leases land, in shape-fits, not instances). A `Refused` starts a `cooldown` for that entry.
- *Boot timeout.* An instance still `registering` after `boot_timeout`, or lost before its first hello, is released and reaped, and
  counts as an acquire failure: the pool backs off exponentially from `cooldown` and, after `max_boot_failures` in a row, stops acquiring
  and raises an alert until the owner clears it (a failure brake, not a cost limit). Its leases, if any, stay `acquiring`.
- *Release.* An **elastic** host with no lease and no running job, not cordoned, not draining and not tainted, idle for `idle_timeout`
  (the one idle delay, 5.10), is released, and its entry is not re-acquired within `cooldown`. A registered `default` host is never
  released: the scheduler did not create it. A tainted elastic host with no lease is released at once; a tainted `default` host is
  refused at hello and stays out until an operator clears it (5.12). Selecting an elastic host moves it to `releasing` under the table lock in the same step; a
  `releasing` host takes no new leases, and a host with a lease committed first is not selected. A failed or Unknown release leaves it
  `releasing` and is retried; it serves again only if a read-back says Present after a `Refused`. Ranking packs onto the fewest hosts
  (`FirstFit`, 6.5) so that idle hosts exist.
- *`default` and `static` providers* launch nothing; they have no acquire and no release. The `static` kind is for tests only.

`max_machines` and `max_step` are overload protection, not cost guardrails (5.11). A pool at `max_machines` with a request no host can fit
is unservable (8.2). Cold start is paid by the queue (D38). Reuse, caps and taint: 5.10 to 5.12.

**Registration.** Every host registers: a machine says hello, so that nothing needs listing in advance, and a machine an elastic provider
launched registers itself the same way, with the cloud's signed identity instead of a join token. The rules (identity, admission with the
tainted-set check, join credentials and the deny file, instance identity, lifecycle, and the `default` provider's source) are in
[design/registration.md](design/registration.md). Proof of admission travels with the hello in message metadata, never in a log line.

### 5.9 The merged code and what it becomes
The merged slice is a GPU-host lease service: the lease table (`js-lease`) books a whole host from a pool (`js-pool-static`) for a client
on a TTL, through the `js-provider` trait. That is level 1 of two-level leasing (9.1) already. What it lacks is the machine side: nothing
on `main` launches or terminates a machine, and there is no agent wire. The GPU-host service keeps working through every follow-up; each
change below is additive and leaves the `--pool-config` file, the routes and the lease file readable. The operator-listed pool behind that
file is a stepping stone: `js-pool-static` stays as the `static` test provider, and the GPU machines move to running `js-agent` and
registering with a `default` provider (step 54). `RegisteredProvider` (`js-registry`) is the core of the `DefaultProvider`, and
`LayeredProvider`'s fixed order (static, then registered) becomes the configured hierarchy ([design/merged-code.md](design/merged-code.md)).

The map of each merged item to what it becomes, with the follow-up step, is in [design/merged-code.md](design/merged-code.md).

### 5.10 Machine reuse, 5.11 caps, 5.12 integrity
Text in [design/machines.md](design/machines.md). After a job the agent removes the container and scratch, checks the host and frees the
slot; queued work that fits goes on the same machine at once; a host is released only after `idle_timeout` (5.10). A pool never exceeds
`max_machines` and grows by at most `max_step` per tick for demand that does not fit: overload protection, not a cost guardrail (5.11). A
job is `contained` (default) or `host`; a host job or any failed check taints its machine, which is replaced or cordoned (5.12).

### 5.13 Capabilities as data
Text in [design/capabilities.md](design/capabilities.md). An agent advertises typed facts: scalars (`arch`, `os`, `os_version`), sets
(`xcode`, `sim.ios`, `base_images`), counters (`vms`) and the GPU class (`gpu_class`); the facts of each primed base image travel as its profile. They
ride an additive `facts` field of `Hello` (major 1 stays), refreshed by a full snapshot in the heartbeat when a revision changes,
acknowledged like the taint; an old server ignores them, and a new agent mirrors its scalars into the legacy string map. A request carries
up to 32 constraints that must all hold: equality and membership on text; `eq`, `gt`, `ge`, `lt`, `le`, `series`, `range` on versions (a
total order, `16 = 16.0`, no pre-release); comparisons on counters; `has`-forms on sets. An absent fact never matches; a `guest` scope
matches when one image's profile meets all its constraints. A malformed constraint is refused at submit with a constant code
(`constraint-unknown-key` and six more); an unknown op is refused, never skipped. kbf's table from its platform properties to
constraints is kbf's configuration; the companion has the worked example.

### 5.14 The VM backend
Text in [design/vm-backend.md](design/vm-backend.md). `js-agent` gains a VM runtime beside the container one. **A fresh VM per run; a
VM is never reused:** it is cloned from a base image, booted, given its inputs over a guest channel, run, collected, stopped and
destroyed, whatever the outcome; a lease may hold several runs and each has its own VM; the host, not the VM, is reused (5.10). The lifecycle stays one state machine over a small `Sandbox`
seam with two backend traits (`ContainerRuntime` unchanged, a new `VmRuntime`; D28). The guest channel is one hypervisor-tied stream per
VM with bounded frames; nothing of the host is shared writable; the guest is untrusted. The cleanup order is the VM, the scratch, the
checks, the slot; a failed destroy taints the host; destroying the VM is the cleanup of the guest. A base image is named, immutable and primed outside the agent; `base_images` is a capability.

### 5.15 Sharing a host, and the whole-node class
Text in [design/vm-backend.md](design/vm-backend.md). VMs and containers share one host's CPU, memory and `vms` counter: a VM counts in
full from clone to destroy; containers are admitted against the total less VM memory; the agent offers what it will give and raises its
offer only after a VM is destroyed. A whole-node lease makes the host quiet before it is usable: the agent announces `preparing`, confirms
that no VM is running and reports ready in its heartbeat, and the lease token is minted only then. The macOS VM sizing question (fixed
size classes or sizing per run; the boot cost per run is the trade-off) is open as D26.

### 5.16 Deployment: one service in Kubernetes
Text in [design/deployment.md](design/deployment.md). The target is one jobscheduler service in a Kubernetes cluster: the scheduler, the
lease API and the agent API in one process. The kbf frontend is a client that asks it for capacity; `js-agent`, the generic half of the
kbf daemons, registers with it from every machine. Today's single-host `js-lease-server` is the stepping stone and the same binary. It
runs as a `Deployment` of one replica under `Recreate`, with the lease file and the binding file on a volume that only one pod can use
(D33), secrets and configuration as mounted files and TLS end to end. Durable, not highly available: a restart or a node loss is a bounded
outage, leases survive in the file, agents re-hello, and a boot grace keeps leases from expiring unrenewed (D34). Servers upgrade first.

## 6. Placement

### 6.1 Pipeline
The pipeline is the one in `crates/kbf-sched/src/scheduler.rs`: a tick takes queued requests in queue order; for each, *fit* produces
eligible (pool, host) pairs; *rank* picks one; the grant becomes a `Commit` effect. Three traits make each stage replaceable:

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
    runtime_tag: RuntimeTag,       // what the agent runs for it (container, native, vm)
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
and a model that excluded memory from the counted set would not reproduce kbf's rank order. kbf maps `Vm` to `Book({cpu_millis, memory_bytes, vms})` at the sizes the VM is created with (D26, open): a VM books its configured CPUs and memory in full, no overcommit, and shares a host with containers (5.15); the earlier intent to admit `Vm` as `BookAll` is superseded by 5.15. `WholeMachine` maps to `Whole`, the only class that takes the whole host. `Vm` is unshipped, so this mapping is a design intent. For ucdk Job classes the recommended admission is `BookAll` (D7); this is booking, not reservation, and per class, so it does not conflict with R1.

**Selecting pools.** A class says how a lease is admitted; it does not say where. A level-1 lease request (9.1) carries a class and label
selectors, and the scheduler offers it the pools whose `class` is that class and whose labels satisfy every selector (equality on
each key; richer set selectors are additive later). The merged pool serves one class,
`gpu-host`, admitted `BookAll`, one whole host per lease; a share of a host needs `Book` admission on the pool (step 29). A request that
matches no pool is refused (`no-matching-pool`); one that matches a pool at `max_machines` queues. The request's constraints (5.13) then
pick a host, and for a VM class an image, inside the pools selected. A request for `host_jobs` is refused unless the pool allows them (5.12). The map from a consumer's own classes (kbf's `action`,
`whole_machine`, `vm`) to class and selectors is in the consumer's configuration (5.7, 9.4), never in the scheduler's.

### 6.3 Oversubscription and 6.4 Devices
Both are in [design/placement.md](design/placement.md). In short: the `Book(Dims)` form carries kbf's rule that memory is checked once
against the pool's total yet still counted in the booking, with the agent's hard limit and the pressure backstop as protection and a
rerun elsewhere as recovery, while classes that need isolation use `BookAll`. Named devices are assigned from `assignments`; at most one
device lease per agent and whole-device GPUs only (D10). For a machine lease the consumer applies both inside its lease (9.1). A
whole-machine request reserves a worker once queued, by draining one host (the drain rule in [vm-backend.md](design/vm-backend.md)); large non-whole requests have no reservation, and R1 forbids adding one.

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
  run's booking and below `cap_of(request)`. `cap_of(request)` is the largest machine the consumer holds a lease on or can lease from a pool matching the request's class and
  selectors, not the size of one lease. When the next rung exceeds the leases held, the consumer requests a whole-node lease (queued as
  a `whole_machine` request with its reservation) and reruns inside it.
- **7.5 and 7.6.** Rule R1 and the shipped estimator disagree (D4); learning happens only after the scheduler committed the result.

## 8. Retries, loss and run records

### 8.1 Retry policy
`RetryPolicy` has the four axes of 4.1 by name (own-limit, platform fault, not started, loss) and adds *task failure*; instance reclaim
spends the platform-fault budget. The portable `max_retries` of a ucdk Job (UJ1) counts every failed attempt on three axes together. The
account is in [design/retries-and-runs.md](design/retries-and-runs.md).

### 8.2 Phases of a request
Before a lease a request is *Queued* (waiting for capacity or fit). For a pool that scales, Queued includes waiting for a host to boot: the
lease table shows the machine lease as `acquiring`, and no separate request phase exists for it. Then Leased, Running, and Completed,
Failed or Refused (4.1). Queued time, boot included, counts toward `UNSERVABLE_WAIT`; a pool at `max_machines` with no host that fits the request
is unservable. A request no host of the pool (and no elastic entry) could ever serve is not queued at all: it is refused with
`no-capable-host` (D35). There is no Held phase: it would exist only for tenant caps and budgets, which jobscheduler does
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
- **Level 2.** The consumer sends `Start` and `Stop` to the agent on its host (5.2). It makes no scheduler call per action.

Consequences:

- `Request`, `Fit` and `Rank` (6.1) apply at level 1: a machine lease queues, is fitted to a pool and a host, and is ranked. Queue, fit,
  rank, fence, TTL and the start contract are level 1. The sizing history, the memory ladder and the per-attempt retry budgets (7, 8.1) act
  on individual requests, so they are level 2: kbf keeps them in `kbf-sched` until a per-request placement consumer is extracted (D15).
- R1's "a lease is a node or half a node" has its home: it is a level-1 lease. A whole host is `BookAll` or `Whole`; half a node is a
  share under `Book` admission. The merged table books whole hosts only; a share needs `Book` on the pool and an accounting of the booked
  share per lease (6.2, 6.3, step 29). The sub-question in D4 is closed by this.
- kbf's in-flight dedup, its queue and its placement stay in kbf. The scheduler never sees an action. Because kbf packs actions into
  the machines it holds, the scheduler's demand is machines' worth of resources, never a count of actions (5.11). A consumer may cap
  itself with `max_leased_machines` in its own configuration (configuration.md); the scheduler does not read it.
- A freed slot is reused at once (5.10): kbf's own loop places its next queued action that fits on the same machine.
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
| `LeaseKind::Vm` | class `vm`, `Book({cpu_millis, memory_bytes, vms})` at the sizes fixed at VM creation (D26 open; 5.15 supersedes the earlier `BookAll` intent; unshipped) |
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
| `js-provider` (merged) | new | merged as `Provider`; becomes `Pool` and gains `CapacityProvider` at step 23 (5.5, 5.9); `StartSpec` and its rules |
| `js-pool-static` (merged) | new | the pool of operator-listed hosts; kept as the `static` capacity provider, **for tests only**, and as the stepping stone of the GPU lease service (5.7, 5.9) |
| `js-provider-fake` (merged) | new | the merged fake provider; becomes the fake aws at step 28 (13.1) |
| `js-conformance` (merged) | new | the machinery; gains the two suites and their broken providers (13.2) |
| `js-lease` (merged) | new | the lease table: states, TTL, reaper, lease file; gains `acquiring` and the pool name (5.9) |
| `js-registry` (merged) | new | the registered hosts: `RegisteredProvider` is the core of the `DefaultProvider` (5.5, providers.md) |
| `js-lease-server` (merged) | new | level 1 over HTTP; `CapacitySource`; `LayeredProvider` (static, then registered) becomes the configured hierarchy; gains the pool-scaler tick; is the service of 5.16 (readiness, boot grace and a state check at steps 48 to 50) |
| `js-cli` (merged) | new | the level-1 client; gains pool selection |
| `js-coverage` (merged) | new | the coverage ratchet (R6) |
| `js-config` | new | the configuration schema and loader of 5.7 |
| `js-provider-aws` | new | the `aws` capacity provider over a cloud-client trait with a fake (5.5); disabled by default |
| `js-provider-<kind>` | new | one crate per later kind; the core names none (5.7) |
| `js-sched` | `crates/kbf-sched` | per-request placement, generic over `Payload`, `ResultBlob`, `Fit`, `Rank`, `RetryPolicy`, `Queue`; keeps the fence and phase constants; scope per D15 |
| `js-caps` | new at step 36; `crates/kbf-caps/src/matching.rs` joins at step 15 | the typed model, version order, schema and constraint evaluator of 5.13; kbf keeps its key tables and host detection |
| `js-guest-wire` | new | the frame codec of the guest channel, shared by the agent and the in-guest program (5.14) |
| `js-sizing` | `crates/kbf-estimator` | `SizingPolicy`; resource-vector windows |
| `js-state` | the `Store` trait and conformance suite of `crates/kbf-state` | trait and fake only; SQLite and Postgres backends stay in kbf (D13) |
| `js-store` | generic rows of `farm/durable/*` | scheduling decisions over `js-state`; kbf extension rows stay |
| `js-agent` | generic half of `crates/kbf-daemon` | the agent of 5.2 and 5.4: session, leases, acks, fence, drain, pressure, hello and registration, start and stop containers with limits, host mounts; facts and probes (5.13); the VM backend (5.14) |
| `js-contain` | `delegate.rs`, `cgroup.rs`, `cgroup_usage.rs`, `numa.rs` and `subids.rs` of `kbf-driver-container` | node-local containment; `js-agent` depends on it. In review, not merged yet (mark it (merged) in the pull request that merges it). The move keeps no REAPI types: a lease's booking is `js_types::Resources`; the cgroup's OOM outcome (`OomOutcome`), the usage counters (`CgroupUsage`) and the node's capacity (`Capacity`) are its own types; `memory_max` is the one function for a lease's memory cap. The OCI runner and the modules that name the CAS or the output tree are not moved yet. |
| `js-sim` | `crates/kbf-sim*` | invariants and replay |

`kbf-executor` is kbf's, not a jobscheduler crate (9.2). Every crate not marked (merged) is not on `main` yet; a merged crate's note
says what it gains.

`crates/kbf-types` today mixes generic types (`Resources`, lease id, `FarmTime`, `Qos`, `Effect`) with REAPI ones (`Digest`, failure
variants, `Answer`). Splitting it into two modules with re-exports is step 3, before anything moves.

### 10.3 Wire boundary
- New package `jobscheduler.agent.v1`: Hello, Welcome, Heartbeat, Start, Cancel, Result, ResultAck, phase, usage, with an
  extension slot per message for payloads. It is the only agent wire, one call shape, and it also carries registration: `Hello` gains the
  host id, pool name, instance id, labels, GPU facts and `agent_version`; `Welcome` gains the admitted pool or a refusal code (5.8).
  Proof of admission travels in message metadata, never in a log line. `Start` gains `mounts`. `LeaseOffer` is not carried over: the daemon only logs it and acts on `Start` (4.4), so v1 drops
  it. The adapter keeps sending it on `kbf.worker.v1`, where old daemons expect it before `Start`.
- `kbf.worker.v1` is kept. The server hosts an adapter from v1 messages to generic messages, so the simulation and store see one shape.
- **Negotiation and release order.** Release order is: server and frontends first, daemons after. A new daemon always opens a v1 session
  first. Only when `Welcome` carries a new optional field (R3) advertising agent v1 does it close and reopen on the new service; a
  `Welcome` without the field means it stays on v1. Step 21b ships only after the release containing step 20 is the minimum server. An old
  server never sees the new package.
- `fence`, `limits`, `assignments`, `labels`, `mounts` and `isolation` are added to `Start` in *both* packages as optional fields (R3). Absent means
  today's behaviour: self-fence, derive limits locally, one device id, no labels, no mounts, contained. `Heartbeat` gains an optional taint.
- **Capabilities and VMs (5.13, 5.14), all optional fields, major 1 unchanged:** `Hello.facts`; `Heartbeat.facts_rev` and `facts`;
  `HeartbeatReply.facts_ack` and `exclusive`; `Heartbeat.exclusive_ready`; `JobSpec.runtime` and `vm`; the lease request's `needs`. A new
  value an old peer does not know is skipped, except a constraint op (refused) and the phase (read as quarantined). **Needs support is advertised:** the server's `Welcome` and its version reply to a consumer carry `features`, a list of tokens including `needs`; a consumer sends non-empty `needs`, or a `vm` class request, only to a server that lists it, else fails closed naming the missing feature (an old server would skip `needs` and place the request unconstrained).
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
`acquire` (`Refused(quota)`, 5.5). There is no cost guardrail; a pool's `max_machines` and `max_step` are overload protection (5.11). What this section keeps is lease correctness and the records that sizing and cleanup need. State today: kbf has no
quota mechanism, and its "budgets" are retry and storage budgets. ucdk has shape limits only (attempts, timeout, argument size, size).

R1 forbids reservation and per-category admission for kbf farm work. Everything below applies to every class, none of it changes how kbf
places or runs work, and none of it is behind a flag.

### 12.1 to 12.3 Lease limits, usage and labels
Text in [design/lease-limits.md](design/lease-limits.md). 12.1: a required max wall time, max attempts and sizes, checked at submit with a
plain constant code; a held lease has a TTL its client renews, and the reaper (the only place time acts) frees expired leases, retries
unknown outcomes, and releases idle, tainted and leaked machines. 12.2: a usage record per machine lease, for sizing and attribution.
12.3: the reserved `jobscheduler.dev/` label keys, which flow from request to agent and tag every instance a provider launches.

### 12.4 Real accounts need a go
A provider that bills a real account is registered disabled. A provider declared in configuration with such a kind stays disabled
(`enabled = false`, 5.7) until it is set to `true`, and that needs an explicit go for that action (R7). Conformance runs against
billable accounts only in a dedicated sandbox account with a billing alarm. The fake provider covers everything else.

**Required IAM scope of the aws role** (a precondition of step 32; short session and external id, launch only from the configured template
and subnets, terminate only by this scheduler's tag, no re-tagging of foreign instances, read-only describes): [design/aws-iam.md](design/aws-iam.md).
A test checks the policy document the provider documents against it.

## 13. Testing

### 13.1 Fake providers
The merged `js-provider-fake` becomes the fake capacity provider, a fake aws in memory with a controllable clock and scripted outcomes:
launch failure, slow boot, an `acquire` accepted but answered Unknown, a `release` answered Unknown, an instance that disappears
(reclaim), a leaked instance in `list`, a quota refusal, and a duplicate acquire id; a launched machine whose fake agent registers late, never, or
with a wrong proof; and a fake `default` provider whose hosts register, go lost, deregister and taint on script (the fake aws is elastic, the
fake default is not; a hierarchy test runs both). A fake agent (in `js-sim`) models the agent
side: loss, fence, out-of-memory kill, node pressure, phase changes, unknown observations and slow starts, and the taint paths: a host
job (also one that reports no taint), each post-job check finding something, a cleanup failure, and a taint report lost and resent. A fake
VM runtime (in memory, scripted: clone failure, a boot that never says ready, hostile guest frames, an unannounced exit, a failed destroy,
a leaked clone, a changed image) and the fake agent's whole-node sequence are described in
[design/vm-backend.md](design/vm-backend.md). Starting points:
`kbf_daemon::Runtime`'s fake in `crates/kbf-daemon/src/runtime.rs` and `crates/kbf-sim-cell`. The fakes are the executable spec, as in
ucdk (`ucdk/crates/ucdk-provider-fake`).

### 13.2 Conformance suites
`js-conformance` keeps its machinery (`Subject`, `Check::ALL`, `Verdict::NotApplicable`, which lists a check a subject cannot take part
in and never skips it silently) and runs two suites, each over every implementation including the fakes.

*Capacity-provider suite:* in [design/invariants.md](design/invariants.md): `acquire` idempotent on the acquire id, `release` read back,
`list` tagged and never an empty list on error, and the pool never above `max_machines`; the `default` kind answers `Refused(capacity)` to
`acquire` and terminates nothing on `release`. The admission and hierarchy cases (refusal when no host could ever serve, queueing when a
busy host could, fall-through, the booting machine kept) are in [design/invariants.md](design/invariants.md).

*Agent suite:* the cases are in [design/agent-suite.md](design/agent-suite.md): start idempotency, cancel resend, stale and duplicate
results, graded absence, validity bound and `NeverStarted`, unknown phase, usage on failure, each start-contract rule, the reuse and
taint cases (container and scratch gone before the slot frees, the three consents for a host job, each post-job check, sticky and
resent taint, no `Start` on a tainted host), the per-session rules, and the VM cases (no VM serves two runs, destroy before the slot frees,
one VM per run, the whole-node sequence; listed in [design/agent-suite.md](design/agent-suite.md)). A third suite, the
**constraint suite** of [design/capabilities.md](design/capabilities.md), runs the evaluator over every op, kind and refusal code and the
ordering properties of versions; the server and the kbf table both pass its worked example.
The hooks **read the real state**: for the fakes, their internal tables; for aws, an independent `DescribeInstances` made apart from the
adapter. A suite that asks the provider whether it did what it said cannot fail. The suite therefore ships one deliberately broken
provider per rule above, each asserting that the suite reports exactly that rule's failure, and a test that counts the broken providers
against the rule list so a new rule without one fails the build. These run on every CI run and are named in the suite's own test names.
Conformance against a real account runs only in a sandbox account, on the owner's go (12.4, R7).

### 13.3 Simulation
The scheduler invariants I1 to I16 and L1 to L4 are checked after every input in seeded sweeps. For the kbf instantiation, the golden
trace hashes (step 0) must be identical before and after each extraction PR. The new invariants are in
[design/invariants.md](design/invariants.md): an Unknown observation never frees a booking and an Absent one frees it once; a pool never
launches twice for one acquire id and never exceeds `max_machines` or `max_step`; a tainted host takes no lease; a host's offer is never
above its total less booked, a VM id serves one run only, and a whole-node token is never minted before the agent reports ready; a GPU
request is never granted a host without one (D32); a boot never reissues an id and a second table fails at the lock (5.16).

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

Steps 23 to 35 (Phase D, new capability; Phase E, capacity providers and pools, with steps 30c and 31c for the `default` provider
and the hierarchy) and **Phase F, steps 36 to 47 (capabilities as data
and the VM backend)**: `js-caps` (36), the `facts` wire (37), agent probes
(38), needs and matching (39), kbf's table (40), the `Sandbox` seam (41), `VmRuntime` and its fake (42), the VM lifecycle (43), mixed-host
accounting (44), several runs per lease (45), whole-node readiness (46), the first real driver (47); the size fields of step 43 wait for D26.
**Phase G, steps 48 to 53 (one service in Kubernetes)**: readiness (48), the boot grace (49), the state check (50), manifests and image
(51), the client side (52), the move from the single host (53). Step 54 (the GPU machines register through `js-agent`, retiring the operator-listed
pool) closes the stepping stone. Step 55 (the rollout checks: the server's refusal of an agent major it does not speak, and the
version-pair check) follows. All are in [design/extraction-steps-d.md](design/extraction-steps-d.md).

## 15. Decisions

The full text of every decision, D1 to D45, is in [design/decisions.md](design/decisions.md); this section holds one line each and, in full,
D35. Each has options and a recommendation. None is final until the owner answers, except where marked settled. The ids run D1 to
D45 with no gaps; D26 (no recommendation yet) is open. A superseded decision says by which.

- D1 home of the code (settled: this repository). D2 wire strategy (a new package with negotiation). D3 durable leases across an agent
  restart (not in v1). D4 sizing rule and ladder. D5 ucdk role. D6 fence. D7 memory booking for non-kbf classes. D8 validation run id and
  label key. D9 triggers. D10 resources and capability schema. D11 consensus (the store's conditional writes). D12 name collision. D13 where
  the state store lives (the trait in `js-state`).
- D14 fleet scaling, reversing N2 (settled; amended by D38: no minimum). D15 scope of the core (level 1 as base). D16 names (`Pool`,
  `CapacityProvider`). D17 scaling policy (simple demand, `max_step`, `cooldown`). D18 self-registration auth (a join token per pool;
  signed instance identity for a launched machine). D19 acquire off the table lock. D20 config format (one TOML file).
- D21 who pays cold start: **superseded by D38** (the queue pays; no minimum). D22 work-session topology (the agent dials out). D23
  machine reuse (b; option (c), a floor of idle hosts, **superseded by D38**). D24 caps (`max_machines`, `max_step`, a consumer cap; D39 adds
  a per-provider share). D25 isolation and taint.
- D26 macOS VM sizing (**open**). D27 how sets and counters ride the wire. D28 runtime traits for VMs. D29 a fresh VM per run (settled). D30
  readiness of a whole-node lease. D31 matching what is inside an image. D32 GPU placement (settled; amended by D35 and D36). D33
  deployment shape and state store. D34 leases across a restart.
- **D35. Admission against registered hosts (settled by the project owner: reading (a)).** A request is refused with `no-capable-host`, at
  submit, only when no registered host could ever satisfy its constraints and shape (a host that exists, is not tainted and is busy, lost
  or cordoned is capable), unless an elastic provider of the pool could launch a machine that meets it (a launch counts as possible only
  once every slot that can free is counted as freed: tainted and denied hosts hold their slot until an operator acts, and a disabled
  entry or one with a zero share launches nothing). When a capable host exists but is busy, the request waits in the queue. A queued
  request whose last capable path goes waits `regain_grace` before it is refused (D41). Refusing whenever no host is free now was
  considered and rejected: it turns a busy fleet into errors and puts a queue in every caller. Text in [design/providers.md](design/providers.md).
- D36 the static pool is for testing only (settled): the operator-listed pool is a test provider; production hosts register through
  `js-agent`; the GPU lease service's listed pool is the stepping stone (step 54). D37 the default provider never creates or destroys
  compute, and an elastic provider boots a machine whose agent registers itself (direction settled). D38 no minimum machines anywhere
  (settled; supersedes D21 and D23(c)). D39 a provider hierarchy: an ordered list per pool, falling through only when an earlier entry
  cannot satisfy now (direction settled). D40 a host that frees while a later entry's machine boots takes the waiting request, and the
  booting machine is kept and released by the idle rule. D41 what is capable and what happens to queued requests when capability goes.
- D42 kbf's update coordinator is not generalised: one version per host, js-agent and kbf-executor ship in the one kbf host package, and the
  server updates by its own redeploy (settled; adopted from the coordinator's owner). D43 rollout order: server, then hosts, then kbf
  frontends. D44 compatibility rules: majors, additive fields, the server refuses an agent major it does not speak, a version-pair check at
  the agent if the two ever ship apart, frontends check the server's advertised version. D45 what would change the plan, and the signer
  decision that comes before any option. Text in [design/rollout.md](design/rollout.md).

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
- **K5. One tenant fills a pool.** Without tenant caps in jobscheduler, one tenant's leases can drive a pool to its `max_machines`. `max_machines` is the
  overload bound; the cloud's quotas are the second, and they refuse an `acquire` (`Refused(quota)`). jobscheduler adds no third limit.
- **K6. Secrets.** `secret_env` references must never reach the run record, labels or logs. The record stores references only. No secret
  goes in user data or in an instance tag; an agent's join credential is short-lived or read from a file the host already has.
- **K7. Tenancy and identity.** The scheduler has no tenant concept (3, 4.1). A tenant must come from an authenticated caller, not a
  request field. kbf already authenticates frontends and nodes (`crates/kbf-auth`, `crates/kbf-oidc`); the design reuses that identity
  path and does not add a second. Scheduled triggers, pool scaling and the reaper run with no caller present, so they need a delegated
  identity. The scheduler also now holds standing cloud access: the role it assumes to launch and terminate, scoped as in 12.4. The tenant detail and the scope and rotation of these
  identities belong in a follow-up document due before step 25.
- **K8. Leaked instances.** A crash between `acquire` and its record, or a failed `release`, leaves a billing instance. Mitigated by the
  tag on every instance, `list`, the reaper and re-adoption after restart (5.5, 12.1).
- **K9. Launch storms and thrash.** Scaling up and down in quick succession wastes instance-hours. Mitigated by idle timeout, cooldown (also
  after a boot failure and after a release), `max_step`, the boot-failure brake and `max_machines` (5.8, 5.11).
- **K10. Registration abuse.** A rogue agent joins a pool and receives work. Mitigated by a per-pool credential, pinned scheduler identity, shape check, host
  limit, id binding, a deny file with eviction (5.8), and by the lease token that binds an agent to one consumer (5.4).
- **K11. Cold start stalls consumers.** A request may wait minutes for an instance, since nothing is kept warm. Accepted (D38); bounded by the unservable wait (8.2),
  and a consumer that cannot wait queues its own work or uses registered hosts first, which the hierarchy does by default (5.8).
- **K12. Instance reclaim mid-lease.** The lease is given up, the work reruns against the platform-fault budget (8.1), and the consumer
  re-leases (9.1).
- **K13. Time.** Scaling that reads a clock outside `tick` is not reproducible; it runs only in `tick` with an injected clock (12.1, 13.5).
- **K14. Slow acquire under the table lock.** The merged server holds the lock across provider calls (`capacity.rs`); a cloud call would
  stall every request. Mitigated by asynchronous acquire (D19, step 29).
- **K15. Two-level leasing.** A consumer can double-book inside its lease, and the level-2 invariants are no longer the scheduler's. The
  agent bounds each run by its limits, so work cannot exceed the host; the consumer owns the rest, and its tests prove it.
- **K16. Reuse leaks state between jobs.** A file, process, mount or user left by one job is seen by the next, perhaps another
  consumer's. Mitigated by removing the container and scratch before the slot is freed, the post-job checks, taint on any finding or
  failed check (a check that cannot run is a finding), and a clean baseline (5.10, 5.12). The checks are a minimum and cheap: a host job
  or a determined escape can defeat them, which is why a host job taints unconditionally and host jobs are off by default.
- **K17. A compromised or wrong agent hides a taint.** The agent reports its own host's checks. Mitigated by the scheduler tainting a host job's machine itself, at grant, whatever the agent reports (5.12), and the agent persisting
  the taint before reporting, the scheduler never clearing on its word, replacement by the provider, and host jobs needing the pool's
  consent in the lease token; a compromised agent still cannot get another host's lease or join another pool (5.4).
- **K18. Taint storms.** A job class that taints every host drives replacement churn. Mitigated by replacement following `max_step`,
  `cooldown` and `max_machines`, and by `max_taints_in_a_row` stopping replacement with an alert (5.12).
- **K19 to K36** are in [design/risks-f.md](design/risks-f.md): constraints, stale facts, VM and image risks (K19 to K27), for the
  service one pod as one point of outage (K28), the whole-file rewrite (K29), the volume as the state (K30), root for secrets (K31), and
  for providers a refusal that a flapping host turns into an error (K32), a booting machine kept after its demand went (K33) and the test
  provider reaching production (K34), and for rollouts the agent fixes held behind the kbf package (K35) and the host updater's one signer
  (K36).

## 17. Rolling out two services

jobscheduler is the distributed service (a server, and `js-agent` on every host); kbf is a frontend that talks to it, so a release touches
two services. Detail, D42 to D45 and the risks: [design/rollout.md](design/rollout.md).
- **Who updates what.** The server updates by its own Kubernetes redeploy ([design/deployment.md](design/deployment.md); an operator act
  with the owner's go, R7). Hosts update through kbf's update coordinator, not generalised (D42): `js-agent` and `kbf-executor` ship
  together in the one kbf host package it installs, so its one version per host covers both. kbf frontends update with kbf.
- **Order (D43).** The jobscheduler server first, then hosts (the kbf coordinator, with its own slots and gates), then kbf frontends once
  the server they call supports what they send. Elastic machines boot the current image, so scale-up is their update path.
- **Compatibility (D44).** Protocol majors, with additive fields inside a major (R3, 10.3); the server speaks every major its agents and
  kbf clients use and refuses an agent major it does not speak; kbf clients check the server's advertised version; a version-pair check
  at the agent if `js-agent` and `kbf-executor` ever ship apart. D45 lists what would change the plan (K35, K36).
