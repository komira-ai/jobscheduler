# jobscheduler: design

Status: design only; no code is proposed for merge. Project: jobscheduler. First user: kbf. Second user: ucdk.
Audience: kbf and ucdk engineers, and the project owner who decides the open items in section 15.

This is the first document of the jobscheduler repository (komira-ai/jobscheduler). Paths such as `crates/...` and `docs/...` refer to the
kbf repository (komira-ai/kbf) at commit 45e420656, the commit this document was written against. Paths beginning `ucdk/` refer to the
ucdk repository (komira-ai/ucdk) at commit f28c240. Every statement about existing code names the file it was read from. A statement
marked (not found) means a search of the named place did not find the thing; it is a keyword result, not a proof of absence. Crates of
jobscheduler itself are named `js-*`; the map is in 10.2, and until then the text says "the core".

## Standing rules cited in this document

These are rules the project already follows. The design cites them by tag.

| Tag | Rule |
|---|---|
| R1 | Scheduling rulings: the kbf server uses all CPUs; no per-category tables and no reservation admission; a lease is a node or half a node, plus telemetry recorded for later scheduling; an action seen before starts at its last peak plus a buffer; an unseen action climbs 4, 16, 32 GiB, then the whole host; an out-of-memory kill moves to the next rung above. |
| R2 | Tests never assert timing: no elapsed or deadline assertions, no real sleeps that race a timeout. Clocks are paused or injected; progress is counted as explicit events. |
| R3 | Formats change additively only: new protobuf fields, old binaries skip what they do not know; no new required fields, no raises, no finalize steps. |
| R4 | Configuration is flags, not environment variables. |
| R5 | kbf and jobscheduler are public repositories: no internal names, no people, no internal plans in code, docs, commits or PR text. |
| R6 | In kbf CI every crate holds 99% line and 99% branch coverage, or a recorded floor that may only rise (`coverage-floors`). In this repository's CI the rule is stricter: every crate has zero uncovered lines and branches, recorded in `coverage-baseline` (the floors file here); no count may rise against the base branch, and a new crate enters at zero. |
| R7 | Cloud spend, uploads and deploys happen only on an explicit go from the project owner for that action. |

## 1. Summary

jobscheduler is a generic scheduler for jobs and compute. A client submits a request that says what it needs (CPU, memory, devices,
capabilities) and what to run (an opaque payload). The scheduler places it on a provider, tracks the lease, accepts one result, and
remembers how much the work actually used so the next request is sized better. kbf already contains such a scheduler, tied to Bazel
remote execution. This design extracts the generic part so that ucdk Jobs, and any later client, can use the same machine, and so that
kbf runs on it as one more client.

The decisions, in one place:

1. **One core, many providers.** The scheduler core stays a pure, deterministic state machine (`crates/kbf-sched`,
   `crates/kbf-types/src/state.rs`). It never calls a cloud or a node. Providers sit at the edge and turn their world into core events
   and core effects into their world's calls.
2. **Two provider flavours behind one contract.** An *agent* provider is a long-lived process that dials out and obeys (the kbf
   daemon). A *delegated* provider is a cloud batch service the scheduler calls (AWS Batch on Fargate, Cloud Run Jobs). They share
   one trait but not one call shape: an agent's outcomes arrive as events on its stream, a delegated provider's are answers to calls (5.2).
3. **Absence has three answers, not two.** A lease is present, absent (observed gone), or unknown (could not be observed). Only
   "absent" frees a delegated lease. Unknown never does.
4. **Placement is queue, then fit, then rank, each behind a trait.** kbf's three lease kinds become *resource classes* expressed as
   data. kbf's oversubscription of action memory is one admission mode of a class, not an assumption of the core.
5. **Request history is a generic sizing service.** The estimator and memory ladder move behind a `SizingPolicy` and a generic key.
   kbf passes today's constants. Results are identical before and after.
6. **kbf works at every step.** The split is a ladder of small PRs. Each is a move or a trait introduction with kbf as the only
   implementer. Step 0 first checks in golden trace hashes and step 1 golden persisted and wire bytes; every later kbf step must leave
   both byte-identical.
7. **The wire grows additively.** `kbf.worker.v1` is kept. A generic `jobscheduler.agent.v1` package is added; the daemon moves to it
   last, and only after the server advertises it (10.3).
8. **ucdk Jobs may use jobscheduler as a second realization.** ucdk 1.0 launches directly against the cloud; jobscheduler is an
   optional realization of Job, bound to one (app, environment), with launch, status, logs and best-effort cancel mapped onto it (11).
9. **Cost guardrails are split into always-on checks and opt-in policy.** Always on, and never touching kbf farm behaviour: the
   start-contract checks (per-run caps such as wall time, fence, reserved label keys), resource-second accounting, and labels stamped
   on every billable resource. Opt-in per class behind a flag, off by default: per-tenant concurrency caps. Budgets are v2. No dollar
   ceilings until a provider reports a price.

## 2. Goals and non-goals

Goals:

- G1. A scheduler core usable without any Bazel or REAPI type.
- G2. kbf's behaviour unchanged by the extraction, proven by golden trace hashes from its simulation suite (`docs/design/simulation.md`,
  invariants I1 to I16, L1 to L4) and golden persisted and wire bytes.
- G3. A provider contract that a kbf daemon, a cloud batch service and a test fake all satisfy, with one flavour-aware conformance suite.
- G4. Placement that handles exclusive work, named devices and oversubscription as data.
- G5. A request history that sizes repeat work and survives kills without hiding them.
- G6. A durable run record for every submitted job before the submit call returns.
- G7. Guardrails that make a runaway job or tenant bounded, without changing kbf farm work.

Non-goals:

- N1. Preemption and fair-share between tenants. kbf has neither (`crates/kbf-sched/src/lib.rs`, the "Not yet" list;
  `docs/design/scheduler.md`). The design adds the seam (the `Queue` type) and no policy.
- N2. Autoscaling of agent fleets. Delegated providers scale inside their own service. Fleet version management stays in
  `crates/kbf-coordinator`, which sets desired versions for host updaters and is not a capacity scaler (`crates/kbf-coordinator/src/lib.rs`).
- N3. Re-adoption of running leases across an agent restart as a scheduler feature. kbf kills leases with the daemon today; re-adoption
  is an optional provider capability for later (the heartbeat rules of 5.4 hold for any unit a daemon does re-adopt).
- N4. A dollar-cost model. Money appears only where a provider returns a price.
- N5. Replacing REAPI, the CAS or the action cache in kbf. These stay in kbf.

## 3. Terms

- **Job.** A unit of work a client wants run, described by a *definition* (payload, resource need, retry policy, timeout, labels). A kbf
  action is a job whose payload references CAS digests. A ucdk Job is a job whose payload is a container image and arguments.
- **Request.** One submission of a job, with a request id. It enters the queue and ends in one outcome. In kbf it is
  `kbf_sched::input::Request` (`crates/kbf-sched/src/input.rs`) and the scheduler calls it an operation.
- **Attempt.** One try at a request on one provider. Retries and reruns create attempts. Exactly one attempt's result is accepted.
- **Lease.** The scheduler's grant of resources on a provider to one attempt, identified by (term, sequence)
  (`LeaseId`, `crates/kbf-types/src/lease.rs`). Per R1, a lease on an agent covers a node or half a node. A lease is recorded durably
  before the provider is told to start (commit before start, `Scheduler::lease_committed` in `crates/kbf-sched/src/scheduler.rs`).
  A lease is per request on a provider; a daemon is a provider that holds many leases.
- **Provider.** Anything that can run leases. An *agent* provider holds one outbound stream to the scheduler and obeys it (a kbf
  daemon); a *delegated* provider is a cloud service the scheduler calls. It reports capacity and capabilities, starts and stops
  leases, and reports what it can and cannot observe.
- **Pool.** A named capacity unit a provider advertises: a resource vector plus labels and a phase. An agent has one pool (its node). A
  delegated provider has a pool per quota.
- **Resource vector.** Named non-negative integers (`cpu_millis`, `memory_bytes`, `gpus`, `vms`, and later others) plus, on a pool only,
  a lease-count cap. Today `Resources` (`crates/kbf-types/src/work.rs`) has five fields: the four dimensions and
  `leases: Option<u64>`, the most Action and VM leases a node holds at once (`None` is no cap; a request carries `None`; `fits` and the
  arithmetic leave it alone). The cap is not a booked dimension and stays a separate typed field (step 11).
- **Resource class.** Data that says how a kind of lease is admitted: which dimensions gate admission and which are counted in the booked total, whether it needs an empty pool, which
  runtime serves it. Replaces the closed `LeaseKind` enum (`crates/kbf-types/src/lease.rs`).
- **Capabilities.** Typed key/value facts a provider reports (ISA level, features, device attributes) under a provider-owned schema; a
  request states needs the same way (`crates/kbf-caps`).
- **Booking.** The amount of a pool's resource vector the scheduler counts as taken by a lease. A booking is not a limit; the limit is
  what the provider enforces (7.3).
- **Request history.** What the scheduler has learned about past attempts of similar work: peak memory, CPU parallelism, kills,
  failures. Keyed by a *sizing key* (class, variant, partition (the tenant), basis; 7.1).
- **Fence.** The rule by which a lease ends when its provider cannot be heard. *Self-fence*: the agent stops the work after a fixed time
  without contact. *Run-on*: the work may continue through a lost connection.
- **Tenant.** The party a request is billed and capped against. kbf has no tenant concept in the scheduler today (not found in
  `crates/kbf-sched/src`).
- **Held and Queued.** Two pre-lease phases of a request. *Held*: submitted but not admitted (per-tenant cap or budget).
  *Queued*: admitted and waiting for capacity or fit. kbf has only Queued today (8.2).

## 4. What exists in kbf today

### 4.1 Scheduler core
`crates/kbf-sched` is a pure, single-threaded state machine: no async, network, clock, randomness or hashed collections
(`#![deny(clippy::disallowed_*)]` in `crates/kbf-sched/src/lib.rs`; layering checked in `crates/kbf-integration-test/tests/layering.rs`,
which hard-codes the pure-crate set and fails if a pure crate is renamed or removed). Its interface is
`StateMachine { type Input; fn apply(&mut self, Input) -> Vec<Effect> }` in `crates/kbf-types/src/state.rs`; time arrives inside every
input. Effects are `Commit`, `Start`, `Answer`, `Waiting`, `Refuse`. Inputs (`crates/kbf-sched/src/input.rs`) are WorkerUp, Capacity,
Heartbeat, Phase, Submit, Committed, Started, Report, Tick, Cordon, Drain, Uncordon. An operation moves Queued, Leased, Running, then
Completed, Failed or Refused (`crates/kbf-sched/src/scheduler/state.rs`). `Phase`, `NodePhase` and the constants `SESSION_SWITCH_GRACE` and the fence
constants are core inputs and values (`input.rs`, `fence.rs`), not kbf extras.

- **Queue.** An ordered set of (urgency, operation id) in `crates/kbf-sched/src/scheduler.rs`; urgency is `Qos` (Interactive 300, Ci 200,
  Batch 100, custom) from `crates/kbf-types/src/qos.rs`. The front always submits `Ci` (`crates/kbf-front/src/execution.rs`). No quota,
  per-tenant fairness, aging or queue-length cap exists in the scheduler, the front or the server (searched for queue limits and
  backpressure; not found).
- **Fit and rank.** Each tick grants up to `PLACEMENT_ROUND = 256` leases. `Servable::fit` (`crates/kbf-sched/src/servable.rs`) admits
  live, uncordoned workers in phase Serving whose driver serves the lease kind, whose capabilities match
  (`crates/kbf-caps/src/matching.rs`), that have room and are not reserved. A worker that earlier attempts ended on is avoided as a
  *preference*: skipped while another matching worker could run the request once its bookings end, taken only when none could. The winner
  is the least `Load` (`crates/kbf-sched/src/load.rs`): the largest of CPU, memory and lease-count share, then fewest leases, then name.
  It spreads; it does not pack.
- **Admission by kind** (`Worker::has_room`, `scheduler.rs`). An Action's admission does not compare its memory with the node's free
  memory. It checks CPU, GPU and VM slots against free capacity, the request's memory against the node's total (it must be at most the
  total), the lease cap, and exclusivity (a request whose memory equals the node's runs alone; `Worker::exclusive`). The grant still adds
  the whole request vector, memory included, to the worker's booked total (`servable.rs`), and `Load` ranks on that memory share
  (`load.rs`), so a worker can hold leases whose memory is more than it has. Concurrent leases are bounded by the node's reported action
  slots (`crates/kbf-server/src/worker.rs`). Memory is deliberately oversubscribed, with a cgroup limit, a pressure backstop
  (`crates/kbf-daemon/src/pressure.rs`) and a rerun elsewhere as recovery. A `WholeMachine` lease needs an empty node and reserves one
  once queued. `Vm` is marked planned (`crates/kbf-types/src/lease.rs`): no front accepts it and no driver reports it, though `has_room`
  requires its full request, memory included, to fit the node's free capacity.
- **Fencing** (`crates/kbf-sched/src/fence.rs`). A lease lives while its worker is heard and lists it; there is no renew call. Grace
  60 s, daemon self-fence 40 s, start validity 14 s, related by compile-time assertions.
- **Cordon and drain** (`crates/kbf-sched/src/cordon.rs`). Operator state held by the scheduler: a cordon names a worker, holds across
  its streams, and is gone after a server restart. This differs from a daemon announcing that it is draining (5.3).
- **Retries.** Four separate axes: own-limit out-of-memory climbs the ladder and spends nothing shared; a shared `FARM_RERUNS` budget
  covers busy-node kills, lost runs, infra failures, a daemon proven stopped and a lease given up after Start was sent;
  `NOT_STARTED_RERUNS` covers Starts that never arrived; a lease lost `LOSS_REQUEUES` (3) times refuses the operation, the third loss included; the scheduler has it off by default and the server enables it with `with_loss_requeues(LOSS_REQUEUES)` (`crates/kbf-sched/src/scheduler/memory.rs`, `fault.rs`, `loss.rs`).
- **Unservable work** is reported by `Effect::Waiting` and refused after `UNSERVABLE_WAIT` (300 s); waits caused only by cordon are never
  refused. While the store is down nothing is placed or given up (`crates/kbf-sched/src/scheduler/outage.rs`); `crates/kbf-sched/src/scheduler/hold.rs` takes back an unservable refusal when a worker held by another scheduler could run the operation, and `handoff.rs` forgets a queued operation another scheduler took; together they let several schedulers share one store.

### 4.2 Server and durable record
`crates/kbf-server/src/farm.rs` owns the scheduler under one lock. A `Commit` effect is written to the state store and fed back as
`Committed`; only then is `Start` sent. `crates/kbf-state/src/store.rs` is an ordered key-value store with transactions, conditions, TTL
keys, counters and a change feed, with SQLite, Postgres and fake backends and a conformance suite; thirteen crates depend on it
(`grep -l kbf-state crates/*/Cargo.toml`: cli, daemon, deploy, enroll-registry, front, history, installer, integration-test, pack-adopt,
pack, server, setup, state-copy), most unrelated to scheduling. `crates/kbf-server/src/farm/durable.rs` and its submodules write rows for
operations, queue, leases (the fencing token), sessions, owners, losses and estimator state. The control log
(`crates/kbf-log/src/control.rs`) encodes `Request.kind` as a closed enum and `Request.key` as a required `ActionKey`; decode fails with
`UnknownValue` or `Missing` on anything else (golden tests: `crates/kbf-log/tests/control_golden.rs`,
`crates/kbf-server/tests/state_golden.rs`). Several frontends share the store; each places only on workers it owns and
`crates/kbf-server/src/farm/durable/intake.rs` adopts others' queued operations. Retention: `FINISHED_RETENTION` defaults to 60 s and is configurable (`--finished-retention`); `done/` rows are kept for the finished retention but at least `DONE_FLOOR`, 600 s (`durable.rs`). `lib.rs` and `docs/design/scheduler.md` say queue persistence is not done, while `durable.rs` writes queue rows at submit; the docs describe the older single-process shape (fixed in step 2).

### 4.3 Request history
- `crates/kbf-estimator` (no dependencies, no unsafe code): about 700 bytes per key (`summary.rs`), a two-level key (`key.rs`: instance,
  platform class, basis, shape, optional configuration; the second level drops the configuration), a versioned checksummed codec
  (`codec.rs`), an LRU table plus a pinned safety table (`store.rs`).
- Memory is the larger of (window maximum + 20% + 128 MiB), the held value and the kill floor, rounded up to 256 MiB; the window maximum
  is a max over about the last 257 to 288 runs, not a percentile. CPU is the 90th percentile of recent parallelism in 250 m steps,
  minimum 1000 m (`run.rs`, `summary.rs`). An own-limit kill sets a floor; two kills in 256 runs make a key raise-only for seven days;
  samples that hit a limit never raise. A key is *mixed* when the largest of its last 32 samples exceeds 4 times their median
  (`MIXED_RATIO`); its estimate is then the default booking or its floor, and is never lowered. With fewer than 5 samples (`N_MIN`), or
  while raise-only, the estimate is at least the default booking (`summary.rs`, `memory`).
- `crates/kbf-sched/src/scheduler/memory.rs` (1286 lines) holds the ladder (default 4, 16, 32 GiB via `--memory-ladder-gib`, then the whole host), per-digest
  floors and per-key floors. `MemoryLadder::raised(booked, cap)` returns the first rung above the *booking* of the killed run and below `cap`,
  else `cap`; `cap` (`Scheduler::memory_cap`) is the memory of the largest live node, cordoned or not, that serves the request's kind,
  platform, CPU and GPU, and always the node the killed run used: a fleet query, not a constant. A busy-node kill reruns elsewhere and never raises the booking.
- `crates/kbf-server/src/learned.rs` has modes off, shadow, on (`--learned-sizes`). In `on` the kill-driven key floor is applied. The
  estimate is computed to compare with what happened and to print a `size estimate:` line; I found no path that books
  `Estimate.memory_bytes` for a first run (searched `learned.rs` only). So R1's "seen work starts at last peak plus a buffer" is not what
  sizes first runs today, and the shipped estimator is a long-window maximum, not a last peak. The "node or half a node" part of R1 is not
  located in code (search of `kbf-sched` and `config.rs` found only unrelated uses of "half"); see D4.
- Offline, `crates/kbf-estimator-replay` replays exported data through a real `Scheduler` in virtual time (`crates/kbf-estimator-replay/src/replay/sim.rs`);
  `crates/kbf-estimator-replay/tests/shadow_diff.rs` asserts the server adapter and the replay agree.

### 4.4 Node side
- Wire: `crates/kbf-proto/proto/kbf/worker/v1/worker.proto`, service `Worker.Session`, one outbound mutually authenticated stream per
  node: Hello, Welcome, Heartbeat, Start, Cancel, Result, ResultAck and usage messages with a `basis`. `Offer` (daemon to server) is read and discarded by the server (`crates/kbf-server/src/worker.rs`). `LeaseOffer` (server to daemon) is sent before every `Start` (`crates/kbf-server/src/farm.rs`) and only logged by the daemon, which runs nothing until `Start` (`crates/kbf-daemon/src/lease.rs`); the proto labels both as placeholders. Neither changes scheduling. `Start.device_id` is on the wire and no code in `crates/kbf-daemon*` or `crates/kbf-driver*` reads it.
  The `Phase` enum is serving, preparing, restoring, quarantined, memory-pressure, draining; a server that does not know a value reads it
  as a quarantine (it takes no lease).
- The `Start.memory_bytes` comment says the container driver sets a soft limit with "no hard per-lease cap". The driver sets a hard
  `memory.max` of 1.5 times the booking plus 512 MiB (`crates/kbf-driver-container/src/cgroup.rs`); the same rule is `memory_max()` in
  `crates/kbf-estimator/src/run.rs` and again in the native driver. The comment is stale and the rule exists in three places (7.3). A second comment in the same proto is stale too: `MEMORY_KILL_OWN_LIMIT` says the server runs the work again "with its memory booking doubled, up to the largest node", while the code climbs the ladder of 4.3.
- Daemon: `crates/kbf-daemon/src/daemon.rs` (session, storage map, pack counters, update status), `lease.rs`, `acks.rs` (results resent
  until acknowledged), `contact.rs`, `window.rs`, `clock.rs` (self-fence on a suspend-counting clock), `pressure.rs`, `report.rs`
  (advertised capacity lowered by reservations). The runtime hook is `kbf_daemon::Runtime` (`runtime.rs`): `driver`, `serves(kind)`,
  `run(Work)`, `failed_usage`, `kill`.
- The daemon is a push stream with level-triggered semantics, which shapes the provider contract (5.2). A daemon that refuses a `Start`
  sends no Result and does not list the lease; the scheduler gives it up as a Start that never arrived (`Start` comment in `worker.proto`).
  The server resends `Cancel` on every heartbeat that lists a lease it no longer holds; `Cancel` is a reconciliation reply for a lease
  the scheduler has already given up, carries no reason, and the server has no path that stops a lease it still holds
  (`crates/kbf-server/src/farm/beat.rs`, `phase.rs`). A heartbeat that leaves out a lease does not requeue it at once. The scheduler
  gives it up only when its `Start` is due (`reconcile` in `crates/kbf-sched/src/scheduler/give_up.rs`):
  - sent to the current session: after `START_GRACE` (60 s) since the `Start` went out, reason `NotStarted`, spending
    `NOT_STARTED_RERUNS`;
  - sent to an earlier session of the same daemon process: at once, reason `Reconnected`, spending no budget;
  - sent to another process proven stopped by its successor: at once, reason `Restarted`, a farm fault;
  - sent to another process not proven stopped: after `HANDOVER_GRACE` (45 s), reason `Replaced`, a farm fault (`fence.rs`).

  A lease whose `Start` was not yet sent, or whose result was already reported, is kept whether listed or not. The `Heartbeat` contract
  in the proto comment is stricter than this: a daemon lists every unit it holds or re-adopted, including leases whose result is not yet
  acknowledged, before its first heartbeat. The design keeps the proto's stricter rule as the agent's duty and the scheduler's graded rule
  as what the core enforces.
- The scheduler derives a fence policy (`Request::fence`: hermetic work runs on) that is not on the wire; the daemon self-fences every
  lease. Leases die with the daemon; re-adoption is planned only.

### 4.5 Simulation
Invariants I1 to I16 and L1 to L4 (`docs/design/simulation.md`) are checked after each input in seeded runs of `crates/kbf-sim` and
`crates/kbf-sim-cell` (`crates/kbf-sched/tests/sim/cell/check.rs`). I15 says only that two runs of a seed in one build give the same
trace hash. Two hash mechanisms exist. `kbf-sim` hashes its trace lines with SHA-256 (`crates/kbf-sim/src/trace.rs`), and those lines
embed `{:?}` of effects, messages and partitions (`crates/kbf-sim/src/sim.rs`). The F1 capacity world hashes a text stream with a 64-bit
FNV-1a (`crates/kbf-sched/tests/sim_f1_capacity/world.rs`); it writes inputs with `{:?}` and effects in a hand-written format. No golden
hash is stored (a search of `crates/*/tests` found only same-process comparisons; the one literal hash found is the SHA-256 of empty
input). So today nothing compares a trace before and after a PR, and any Debug-format change moves the hash. Step 0 covers both
mechanisms and both line formats, and states whether the 64-bit hash is replaced.

### 4.6 ucdk
ucdk declares compute and does not schedule it. `Job` is in `ucdk/crates/ucdk-proto/proto/ucdk/resource/v1/resource.proto` (image, args,
size, retries, timeout default 600 s, environment, secret environment, on-demand or cron trigger). It lowers to a `run` node and a
`Schedule` role node (field 11, role "schedule"; `ucdk/crates/ucdk-plan/src/catalog/`), which the clouds realize as their own scheduler
service. ucdk keeps one state per environment (`ucdk/docs/architecture.md`): an environment is one provider, account and region with its
own ledger. ucdk's `Provider` trait (`ucdk/crates/ucdk-provider/src/provider.rs`) creates, observes, updates and deletes
*infrastructure objects*; it is a different thing from the jobscheduler provider (D14). Objects carry a `Stamp` (owner, node,
scheme, and other fields, with an open `extra` map that only kinds with tags carry; a create asking a tagless kind for extras is refused,
`ucdk/crates/ucdk-provider/src/stamp.rs`). The launcher, `run` verb, run records, real cloud providers and cost logic are not built.

This design needs these properties of a launcher and a Job; they are stated here so the document stands alone:

- UL1. Launch a job by name; the call returns after a durable run record exists.
- UL2. Read the status of a run from its record.
- UL3. Read logs from a cursor, with a `complete` flag.
- UL4. Cancel is best effort and reports what was done.
- UJ1. A bounded number of retries of failed attempts (any failed attempt, including a non-zero exit).
- UJ2. A timeout, applied per attempt.
- UJ3. Schedule semantics: ticks deduplicated, overlap allowed or forbidden, missed ticks skipped with no catch-up, default zone UTC.

## 5. The provider interface

### 5.1 Role of a provider
The core stays pure. A provider is a driver on the edge: a small server adapter feeds the core its inputs (`Capacity`, `Heartbeat`,
`Report`, `Phase`) and executes the `Start` effect. The core holds no provider reference, so the core crate stays free of async and I-O.
The core has no `Stop` effect today; stopping a lease the scheduler still holds is new (5.2, step 23).

### 5.2 Contract
A sketch, not a final signature. Calls that touch the provider are async at the edge.

```rust
trait Provider {
    fn id(&self) -> ProviderId;                    // stable value; the scheduler never branches on its name
    fn flavour(&self) -> Flavour;                  // Agent | Delegated
    fn pools(&self) -> Vec<PoolReport>;            // name, vector free and total, labels, phase. Agent: Hello/Heartbeat. Delegated: quota read
    fn capabilities(&self) -> Capabilities;        // typed keys under a schema this provider registers
    fn classes(&self) -> Vec<ClassId>;             // resource classes it can serve

    /// Record the intent to start one attempt. Idempotent on the lease id. What is answered now and what arrives later: table below.
    async fn start(&self, s: StartSpec) -> StartAck;
    /// Retain the intent to stop one lease.
    async fn stop(&self, l: LeaseId, why: StopReason) -> StopAck;
    async fn observe(&self, l: LeaseId) -> Observed;       // Present(state) | Absent | Unknown
    async fn list(&self, scope: Scope) -> Vec<Observed>;   // by label, for sweeps (delegated)
    fn events(&self) -> EventStream;   // Started | Result | Usage | PhaseChange | CapacityChange | NeverStarted | Lost
    fn price(&self, r: &ResourceVector, d: Duration) -> Option<Price>;   // None when there is no price; never guessed
}
```

The daemon adapter must not invent answers, so each flavour has its own outcome table:

| Call | Agent (stream; level-triggered) | Delegated (call and answer) |
|---|---|---|
| `start` | `Sent` or `NoStream`. The outcome is an event: `Started` (the lease is listed in a heartbeat), a `Result`, or `NeverStarted` (no Result and not listed past the validity bound). There is no Accepted or Refused(code). | `Accepted`, `Refused(code)` or `Unknown`, answered by the call. |
| `stop` | New in this design (step 23). Today `Cancel` only answers a heartbeat that lists a lease the scheduler no longer holds. Here `stop` is a retained intent, resent on every heartbeat that lists the lease. No read-back at call time. The outcome is an event: a Result, or the lease gone from the heartbeat. | `StopReport` with the state read back (accepted, already gone, refused, unknown). |
| `observe` | Reads the last heartbeat set and the stream identity; it is not a query. Listed is Present. A lease the heartbeat omits is Absent only once its `Start` is due (4.4): on the same session, `START_GRACE` after the `Start` went out; if the `Start` went to an earlier session of the same process, at once; if it went to another process, after `HANDOVER_GRACE`, or at once when that process is proven stopped. Before then the lease is pending (Started not yet seen), not Absent. The reason picks the rerun budget (4.4). Silence past the grace is the fence rule of 4.1, not Absent. | A query: Present, Absent (explicit not-found) or Unknown. |

`StartSpec` carries: lease id; the payload reference (opaque to the core); the booking; a separate `limits` vector (what the provider
must enforce); `assignments` (named resources granted, such as a GPU index or a device id); `fence` (self-fence or run-on); `labels`; a
deadline for the attempt; and, for an agent, the validity anchor `heartbeat_seq` and `valid_for_ms` (the newest heartbeat the server had
taken, and how long after the daemon sent it the Start may still be acted on, measured on the daemon's clock alone; 0 means no bound).
For a delegated provider the anchor is replaced by a submit deadline. Today the daemon receives booking only, derives limits itself, and
receives one `device_id` (`worker.proto`); `limits`, `assignments`, `fence` and `labels` are the additive fields of step 7.

**Start contract.** Before the first provider call for a request, the core refuses it unless: the wall-time limit is positive; the
fence is stated (requests resolve it at submit from the definition or from a default the class declares as data; the resolved value is in
the run record, so no default is silent; D6); labels are free of reserved keys (12.5); and retries are owned by the core and are never
passed to a provider. These checks apply to every request of every class, kbf's included, and must change no kbf behaviour: the kbf adapter supplies a positive wall-time limit and no reserved labels, and the `action` class declares kbf's fence. They are not the opt-in policy of section 12. A provider may add refusals and may never relax these. Each rule has a conformance case (13.2).

### 5.3 What each part generalises
- **Capacity.** The Hello entries `cpus`, `mem_gib`, `gpu`, `vm.slots`, `action_slots` become `Resources` in
  `crates/kbf-server/src/worker.rs`: a named map of dimensions plus the separate lease-count cap. Advertised capacity stays "what the
  provider is willing to give": an agent lowers it by its own reservations (`NodeReport::within` in `crates/kbf-daemon/src/report.rs`)
  and the scheduler never sees the reservation.
- **Capabilities.** The comparators in `crates/kbf-caps/src/matching.rs` (exact, at-least ISA level, feature subset, countable minimum,
  set membership, single device satisfies all) become a schema a provider registers: key to comparator kind, with namespaced keys. A
  request key unknown to the schema is refused at submit; an unknown key in a provider *report* is still skipped, as R3 requires.
- **Phases.** The generic phase set is the wire's. A pool takes new leases only in `serving`.

  | Phase | Meaning | New leases |
  |---|---|---|
  | serving | normal | yes |
  | preparing, restoring | the node is switching sessions, tied to a whole-machine lease (`SESSION_SWITCH_GRACE`) | no |
  | memory-pressure | only while the node holds a lease | no |
  | draining | announced by the provider (SIGTERM): leases run to their end, nothing is requeued | no |
  | quarantined | held out | no |

  An unknown phase value is read as quarantined (fail closed). This is the one deliberate exception to "skip what you do not know", since
  skipping a phase could place work on a node that is not serving. `NodePhase` and the fence constants move with the core (`js-types`,
  `js-sched`, 10.2); the macOS logic that decides to enter preparing or restoring stays in kbf (9.2). Provider-announced draining is distinct
  from operator cordon and drain (4.1), which are scheduler state.
- **Start payload and result.** `Start.action_digest` and `Result.action_result` become an opaque payload reference and an opaque result
  blob. The kbf adapter interprets digests; the ucdk adapter interprets an image and arguments.
- **Failure taxonomy.** Keep the classes in `crates/kbf-daemon/src/runtime.rs`: Killed, Failed, Invalid, MissingInput, TimedOut,
  OutOfMemory (own limit), BusyNode (node pressure), Lost (restarted or fenced). A delegated provider's "preempted" result maps to
  `BusyNode`: the platform took the capacity back, the work is rerun elsewhere, and the platform-fault budget is spent (8.1). The single mapping point to the wire stays `result_of` in `crates/kbf-daemon/src/lease.rs`. `MissingInput` is kbf-only; the generic set names it "InputUnavailable" and the kbf adapter maps it.
- **Usage.** A usage report carries a `basis` and optional blocks, as `ResourceUsage` does, plus the censoring signals the estimator
  needs: limit hit, swap peak, out-of-memory kill count, and node pressure at start. Usage is reported on failure too
  (`Runtime::failed_usage`).

### 5.4 Agent providers
The kbf daemon is the model. The agent dials out and holds one stream; `Hello` carries capabilities and phase, `Welcome` carries the epoch
and heartbeat interval, `Heartbeat` lists the leases the agent holds, `Start` and `Cancel` come down, `Result` goes up and is repeated
until `ResultAck` (`crates/kbf-daemon/src/acks.rs`). Rules that carry over unchanged:

- A lease lives while the agent is heard and lists it. There is no renew call.
- A `Start` that arrives later than its validity window is refused by the agent, which then sends nothing; the scheduler gives the lease
  up as never started (`NeverStarted`).
- A `Cancel` for a lease the scheduler no longer holds is resent while the lease is listed. A lost Cancel is therefore not an error.
  (A stop of a lease the scheduler still holds is new; see `stop` in 5.2.)
- The heartbeat set includes leases whose result is not yet acknowledged. A daemon that holds or re-adopts units lists all of them before
  its first heartbeat on a new stream; otherwise the scheduler may requeue the lease once its `Start` is due (4.4) and the work runs
  twice. The first heartbeat after re-adoption must be complete.
- The agent self-fences at 40 s of silence against 60 s of scheduler grace; the compile-time assertions in
  `crates/kbf-sched/src/fence.rs` keep the relation. A run-on lease is allowed only when the request says it is safe (D6).
- `ResultAck` values ACCEPTED, DUPLICATE, STALE, UNKNOWN_LEASE and MISMATCH are kept. One result is accepted per request, and only while
  the committed lease matches (`result_committed` in `crates/kbf-sched/src/scheduler.rs`).

### 5.5 Delegated providers
A delegated provider is a cloud batch service. The scheduler submits and the cloud places inside a quota. Mapping:

| Core step | Delegated call |
|---|---|
| Start | submit one job or execution, named by the lease id, with labels |
| Stop | terminate it; report whether the cloud accepted, and the state read back |
| Observe | read the job by lease id; list by label for sweeps |
| Capacity | quota and current use; a pool, not a node |
| Result | poll or event: succeeded, failed with reason, timed out, preempted |
| Retry and timeout | the scheduler owns the retry budget and passes none to the cloud; the cloud enforces the per-attempt timeout; the scheduler keeps its own attempt record |

Fence for a delegated lease is "the cloud owns it": the lease ends only when the cloud says so, or when the scheduler observes the job
absent. A delegated pool has no whole host; its ceiling for one run is its quota (7.4).

### 5.6 Failure and absence semantics
This is the most important part of the contract. A lease the scheduler holds can be in one of three observed states, and the core treats
them differently. For an agent the three are derived from the heartbeat stream (5.2 table); for a delegated provider they are answers.

| Observed | Meaning | Core action |
|---|---|---|
| Present | the provider confirms the lease exists, with a state | keep the lease; update state |
| Absent | the provider answered (delegated), or the stream's heartbeat omitted it with its `Start` due (agent; 5.2), and the lease does not exist | the lease is over: if no result was accepted, give up and rerun against the right budget |
| Unknown | no answer, a partial answer, or an error other than not-found | keep the lease; never free its resources; retry the observation; escalate to an alert after a configured count of unknown observations |

Rules:

- **Start outcome unknown.** For a delegated provider, a `start` that returns Unknown (timeout, connection reset) leaves a durable intent.
  The adapter looks the lease up by id before any retry and never issues a second submit for the same lease id without a lookup. Because
  the lease record is committed before Start (4.1), the intent already exists. For an agent the intent is the lease record and the
  validity bound ends it.
- **Stop outcome.** A delegated `stop` returns what the provider did and what it then read back, and never claims more than was observed.
  An agent `stop` claims nothing at call time; its outcome is an event.
- **Agent silence is not absence.** For an agent, silence past the grace period triggers the fence rules of 4.1. For a delegated
  provider, silence is Unknown until an explicit not-found.
- **Provider outage.** While a provider is unreachable, no new leases are placed on it, and its leases keep their resources booked. This
  is the store-outage rule of `crates/kbf-sched/src/scheduler/outage.rs` applied per provider: expiry timers restart with a grace window on recovery.
- **Cordon and drain.** Operator `Cordon` and `Drain` events apply to a pool and are scheduler state, lost on a server restart. A wait
  caused only by a cordoned pool is never refused as unservable. Provider-announced draining is a pool phase (5.3), not an operator event.
- **Poison.** Repeated losses of the same request end it: a lease lost `LOSS_REQUEUES` (3) times refuses the operation (4.1). The value
  becomes a per-class retry policy value (8.1).
- **Duplicate results.** A second result for an accepted request is answered DUPLICATE and ignored. A result for a lease that is not the
  committed lease is STALE.

## 6. Placement

### 6.1 Pipeline
The pipeline is the one in `crates/kbf-sched/src/scheduler.rs`: a tick takes queued requests in queue order; for each, *fit* produces
eligible (provider, pool) pairs; *rank* picks one; the grant becomes a `Commit` effect. Three traits make each stage replaceable:

- `Queue`: today a bare ordered set referenced from submit, requeue, unplace, forget_queued, refusal and result_committed. Wrapping it in
  a type is step 4 because later policies (tenant caps, aging, fair turns) plug in there and nowhere else. A request a tenant cap or budget holds is
  *Held*, outside the queue's ordered set of admitted requests; it becomes *Queued* when admitted (8.2).
- `Fit`: eligibility and room for a request on a pool. Today `Servable::fit` and `Worker::has_room`.
- `Rank`: a total order over eligible pairs. Today the least `Load`.

Constraint on the first extraction: kbf's `Queue`, `Fit` and `Rank` are the existing code behind traits, with identical output. Anything
new is a separate PR with its own proof. The traits are frozen only after the delegated spike of step 12 has exercised them.

### 6.2 Resource classes as data
`LeaseKind` is a closed enum (`Action`, `WholeMachine`, `Vm`) whose per-kind admission is a `match` inside `Worker::has_room`. The
generic form:

```rust
struct ResourceClass {
    id: ClassId,                   // a string, e.g. "action", "whole_machine"
    admission: Admission,          // see below
    runtime_tag: RuntimeTag,       // which provider runtime serves it
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

### 6.3 Oversubscription as kbf does it
The generic core must not assume memory gates admission. The `Book(Dims)` form carries kbf's rule as data: dimensions not listed in
`Dims` are not compared with free capacity; they are checked once against the pool's total (a request whose memory equals the pool's is
exclusive and runs alone; a request above it does not fit that pool), yet they are still added to the booked total and still count in
`Rank`. Their protection is the provider's job (a hard limit per lease, the pressure backstop, and a rerun on another node on a busy-node kill, which never raises the
booking). Classes that need isolation use `BookAll`.

### 6.4 Devices and exclusive work
`crates/kbf-sched/src/devices.rs` is the model for named assignments: pick the first free device by id that satisfies the attributes;
release on result acceptance or give-up; a re-sent report that drops the device does not release it. The generic
`assignments: Vec<{resource, id}>` generalises this to GPU indices, NUMA nodes and device ids. Two limits stay, with their reasons:

- At most one device lease per agent (`DEVICE_LEASES_PER_WORKER = 1`), because sandbox isolation between device leases is unproven. A
  class can raise this when its provider can prove isolation.
- GPUs are whole-device only. Fractional GPUs are out of scope (D12).

A whole-machine request reserves a worker once queued, which stops small requests from starving it. Large non-whole requests have no
reservation today (`docs/design/simulation.md`, scenario F1.5, documents this); R1 forbids adding reservation admission, so the design
adds none and the scenario is kept as a known behaviour.

### 6.5 Rank for elastic pools
Spread is right for a fixed fleet and wrong for an elastic pool, where packing lets idle nodes drain. Rank is therefore a trait with two
shipped implementations: `Spread` (today's `Load`) and `FirstFit` for delegated pools, chosen per provider. An optional dedup key on a
request (absent: no coalescing) lets kbf keep its in-flight coalescing by action key (the `in_flight` map in `crates/kbf-sched/src/scheduler.rs`) without the core owning the table.

## 7. Request history and sizing

### 7.1 The generic key
`(class, variant, partition, basis)` where `class` is opaque, `variant` is optional and dropped for the fallback level (as `Key::l2()`
drops the configuration in `crates/kbf-estimator/src/key.rs`), `partition` is the tenant, and `basis` is the measurement instrument.
Values with different bases never pool. kbf derives its class from the remote execution command shape
(`crates/kbf-server/src/action_usage/shape.rs`); ucdk derives it from job definition id and image digest.

### 7.2 Sizing policy
Constants that are scattered today become one value:

```rust
struct SizingPolicy {
    limit_of: fn(booking) -> limit,   // today memory_max: 1.5 x + 512 MiB
    round: u64, min: u64,             // 256 MiB steps
    slack_frac: Ratio, headroom: u64, // 20% + 128 MiB
    n_min: usize,                     // 5; gates lowering below the default booking
    mixed_ratio: u64,                 // 4: a key whose largest recent sample exceeds this times the median is never lowered
    default_memory: u64,              // the booking an under-sampled, mixed or raise-only key falls back to
    raise_only: RaiseOnly,            // kills in a run window that make a key raise-only, and for how long
    decay_runs: usize, decay_days: u32, stale_days: u32,
    cpu_step: u64, cpu_min: u64,      // 250 m, 1000 m
    ladder: MemoryLadder,
    seen: SeenRule,                   // LastPeakPlusBuffer | WindowMax
    mode: SizeMode,                   // Learned | Declared (ucdk: the declared size is the booking and the ceiling)
}
```

`SizingPolicy::kbf()` returns the current values and the property test asserts that every existing estimator output is unchanged (step
14). The codec's length depends on fixed slot counts, so those stay fixed or the codec version byte is raised
(`crates/kbf-estimator/src/codec.rs`). In `Declared` mode history only informs a warning or a suggested size and never raises a booking
(11.3).

### 7.3 One limit rule
The 1.5x + 512 MiB rule lives in `cgroup.rs`, in `memory_max()` in the estimator, and in the native driver. The Start `limits` field plus
`SizingPolicy::limit_of` make the scheduler the one source: the scheduler computes the limit, sends it, and the estimator's censoring
decision ("did this run hit its limit") uses the same function. A provider may still enforce a stricter limit and say so in usage.

### 7.4 Ladder per class
`--memory-ladder-gib` is server-wide. The generic form is per resource class: an unseen request climbs its class's ladder; an
out-of-memory kill moves to the next rung above the killed run's *booking* and below the cap, else to the cap (`MemoryLadder::raised`, 4.3).
The cap is an explicit core query, `cap_of(request)`, which keeps `Scheduler::memory_cap`: the memory of the largest live pool,
cordoned or not, that serves the request's kind, platform, CPU and GPU needs, and always the pool the killed run used. It is not
Fit's candidate set, which leaves cordoned pools out; a `cap_of` built on Fit's view would change the rung a killed run climbs to
during a cordon or drain. For a delegated pool it is the quota ceiling for one run; when a pool states none there
is no whole-host rung and the ladder ends at its top rung. Because the limit is 1.5x + 512 MiB, a 12 GiB booking has an 18.5 GiB limit
and the next rung (16 GiB) is below the limit that just failed; whether to move to "above the limit" is D4, not part of the extraction.
The scheduler interface stays `set_size_key`, `key_floor_bound`, `lower_key_floor`, `restore_floors`
(`crates/kbf-sched/src/scheduler/memory.rs`). The ladder becomes a plugin behind `RetryPolicy`, because `memory.rs` today mixes retry
accounting with the ladder and is 1286 lines.

### 7.5 What rule R1 says and what ships
R1 says a seen request starts at its last peak plus a buffer. The shipped estimator is a long-window maximum with 20% plus 128 MiB, and
in `on` mode only the kill-driven floor is applied (4.3). The design does not decide this. `SeenRule` has both values; kbf stays as
shipped until the project owner picks (D4). Whichever is chosen, lowering a booking stays off until a replay on farm data supports it
(shadow mode first, as `--learned-sizes` does, then `shadow_diff` in `crates/kbf-estimator-replay/tests/shadow_diff.rs` to prove parity;
replay output is an upper bound).

### 7.6 Learning handshake
`Learned::stage` then `requeued` or `finish` (`crates/kbf-server/src/learned.rs`) learns from an attempt only after the scheduler committed the result, so a discarded result does not teach. This becomes `SizeLearner` with kbf types removed; persistence is a `SizeStore` trait (get_all, put_batch, delete, scan) with rows written outside the scheduler transaction, best-effort after kills and write-behind otherwise.

## 8. Retries, loss and run records

### 8.1 Retry policy
Four budgets exist (4.1). The generic core gets `RetryPolicy` with those four axes by name: *own-limit* (sizing ladder, spends nothing
shared), *platform fault* (shared budget, which also takes a delegated provider's preempted attempts), *not started*, and *loss*. It adds a fifth, *task failure*: the attempt ran and the work
failed (a non-zero exit). kbf's budget for it is zero, because a failed action is a result and not a retry. For ucdk classes the portable
`max_retries` (UJ1) is one budget that counts every failed attempt on the platform-fault, not-started and task-failure axes together,
so a Job whose container exits non-zero is retried exactly as under the cloud's native batch engines; timeout (UJ2) applies per attempt.
The ladder and loss axes are extra capabilities, not part of the portable contract. The ucdk conformance kit has a UJ1/UJ2 case that
fails on a non-zero exit; step 24b adds the axis.

### 8.2 Phases of a request
Before a lease: *Held* (submitted, not admitted: per-tenant cap or budget) and *Queued* (admitted, waiting for capacity or
fit). Then Leased, Running, and Completed, Failed or Refused (4.1). Only Queued time counts toward `UNSERVABLE_WAIT`; a Held request
is never refused as unservable. kbf uses Queued only, because its classes do not opt in to tenant caps. A per-run cap is a refusal at submit (12.1), never a Held reason.

### 8.3 Run records
A submit returns only after a durable run record exists. kbf's store retention is short (a 60 s default in the scheduler, at least 600 s for done rows).
Jobs need a configurable floor, with a default of days, and the state store holds it. Record fields: request id, definition reference,
phase (Held or Queued before a lease), tenant, labels, the resolved fence, attempts (provider, lease, start, end, outcome, usage), final
outcome, log cursor. Log text itself is not in the record; the record holds a cursor and a `complete` flag. The record is written with
additive fields (step 6).

## 9. kbf's daemons as the first provider

### 9.1 What changes in kbf
- `kbf_daemon::Runtime` becomes the in-agent provider hook with `Work.action_digest` replaced by a payload reference. The kbf runtimes
  (container, native, fake, local) keep their implementations.
- The daemon's generic half (session, lease table, acks, fence, drain, pressure) is separated from its kbf half (storage map, pack
  counters, update status). Both live in `crates/kbf-daemon/src/daemon.rs` today.
- `kbf-driver-container`'s `delegate.rs`, `cgroup.rs`, `cgroup_usage.rs` and `numa.rs` move to a node-local containment crate with no
  REAPI types. Only `ContainerRuntime` keeps them.
- The kbf server stops using the scheduler's request type directly; a thin adapter builds a generic request with `ActionKey` as payload.
- The daemon adapter implements the agent half of the outcome table in 5.2: it reports what the stream shows and nothing it cannot see.

### 9.2 What stays in kbf
REAPI front end (`crates/kbf-front`), the action cache write, in-flight dedup tables, `MissingInput` handling
(the `Failure::MissingInput` mapping in `crates/kbf-server/src/farm.rs` and `missing::inputs` in `crates/kbf-server/src/missing.rs`), CAS, packs, storage map and update plumbing, macOS drivers, iOS device vocabulary and
Xcode properties, the macOS logic that decides to send preparing or restoring (the phase values and `SESSION_SWITCH_GRACE` live in the
core, 5.3), the key derivation from remote execution commands, and the platform-property parser.

### 9.3 Mapping
| kbf today | Generic |
|---|---|
| `LeaseKind::Action` | class `action`, `Book({cpu_millis, gpus, vms})` plus the pool's lease-count cap (6.2), serves by container or native runtime |
| `LeaseKind::WholeMachine` | class `whole_machine`, `Whole` |
| `LeaseKind::Vm` | class `vm`, `BookAll` (unshipped) |
| `ActionKey` | payload and optional dedup key |
| `hermetic` | class `action` declares self-fence as the default; a request marked `hermetic` resolves to run-on, and the resolved value is recorded |
| `Outcome::Completed{action_result}` | `Completed{result_blob}` |
| a daemon | an agent provider with one pool; a lease is per request on it |

## 10. The kbf / jobscheduler split

### 10.1 Principles
- Dependency runs one way: kbf depends on jobscheduler; jobscheduler never depends on kbf. ucdk depends on jobscheduler's public client
  API; jobscheduler never depends on ucdk. A layering test enforces both edges: no jobscheduler crate names a `kbf-*` or `ucdk-*` crate,
  in the style of `crates/kbf-integration-test/tests/layering.rs`. kbf's own layering test (pure set, forbidden list, clippy-deny check)
  is updated in the same PR that moves a pure crate, so the checks never go vacuous (step 16).
- jobscheduler is public and follows R5.
- Each step is a move or a trait introduction. Behaviour is pinned by the golden trace hashes, the golden persisted and wire corpus, and
  the simulation suite (section 13).

### 10.2 Crate map
| jobscheduler crate | From | Notes |
|---|---|---|
| `js-types` | generic half of `crates/kbf-types` | `Resources` as a named vector plus the separate lease-count cap, lease id, farm time, `Qos`, `NodePhase`, `StateMachine`, `Effect`, control record |
| `js-sched` | `crates/kbf-sched` | generic over `Payload`, `ResultBlob`, `Fit`, `Rank`, `RetryPolicy`, `Queue`; keeps the fence and phase constants |
| `js-caps` | `crates/kbf-caps/src/matching.rs` | schema-driven; kbf keeps its key tables and host detection |
| `js-sizing` | `crates/kbf-estimator` | `SizingPolicy`; resource-vector windows |
| `js-state` | the `Store` trait and conformance suite of `crates/kbf-state` | trait and fake only; SQLite and Postgres backends stay in kbf (D15) |
| `js-store` | generic rows of `farm/durable/*` | scheduling decisions over `js-state`; kbf extension rows stay |
| `js-agent` | generic half of `crates/kbf-daemon` | session, leases, acks, fence, drain, pressure |
| `js-provider` | new | the trait of section 5, delegated skeleton, conformance suite with its deliberately broken providers |
| `js-contain` | `delegate.rs`, `cgroup.rs`, `cgroup_usage.rs`, `numa.rs` of `kbf-driver-container` | node-local containment |
| `js-sim` | `crates/kbf-sim*` | invariants and replay |

`crates/kbf-types` today mixes generic types (`Resources`, lease id, `FarmTime`, `Qos`, `Effect`) with REAPI ones (`Digest`, failure
variants, `Answer`). Splitting it into two modules with re-exports is step 3, before anything moves.

### 10.3 Wire boundary
- New package `jobscheduler.agent.v1`: Hello, Welcome, Heartbeat, Start, Cancel, Result, ResultAck, phase, usage, with an extension slot
  per message for provider payloads. `LeaseOffer` is not carried over: the daemon only logs it and acts on `Start` (4.4), so v1 drops
  it. The adapter keeps sending it on `kbf.worker.v1`, where old daemons expect it before `Start`.
- `kbf.worker.v1` is kept. The server hosts an adapter from v1 messages to generic messages, so the simulation and store see one shape.
- **Negotiation and release order.** Release order is: server and frontends first, daemons after. A new daemon always opens a v1 session
  first. Only when `Welcome` carries a new optional field (R3) advertising agent v1 does it close and reopen on the new service; a
  `Welcome` without the field means it stays on v1. Step 21b ships only after the release containing step 20 is the minimum server. An old
  server never sees the new package.
- `fence`, `limits`, `assignments` and `labels` are added to `Start` in *both* packages as optional fields (R3). Absent means today's
  behaviour: self-fence, derive limits locally, one device id, no labels.
- Stale comment in `worker.proto` on `memory_bytes` is corrected in step 2.

### 10.4 How kbf keeps working at every step
1. Each PR leaves kbf as the sole implementer of any new trait.
2. The gate for each PR from step 3 on is: the golden trace file (step 0) is byte-identical; the golden corpus (step 1) decodes and
   re-encodes byte-identical; per-crate line and branch coverage is stated old and new, not lower (R6), and a moved crate passes kbf's 99%
   with its own tests only (zero once it is in this repository, 13.6); the farm build shows no local actions. A change to a trace line's format or to a Debug output is a step 0 change,
   not an extraction side effect.
3. A wire change is optional-field only. The cross-version tests (`crates/kbf-integration-test/tests/cross_version.rs`) run a matrix:
   {old, new} daemon against {old, new} frontend; two frontends of different versions sharing one store, with a daemon reconnecting
   between them; and a downgrade after the switch.
4. Rollback is a revert of the PR. No stored row changes until step 6, which is additive: kbf rows stay byte-identical (golden test); a
   row for a non-kbf class is written only when that class is enabled, and disabling the class, not a revert, is its rollback.

## 11. ucdk Jobs on top

### 11.1 Placement of the scheduler and the two roles
**One scheduler, one environment.** A jobscheduler instance is bound to exactly one (app, environment): that environment's account,
region, pools and run store. It is deployed into the environment as an ordinary ucdk concept and shown as machinery in the plan. For a
launch a caller makes, it holds no standing cloud credential and acts with the caller's launch identity. Scheduled triggers (11.2) and
the leftover sweep (12.5) run with no caller present, so they use a delegated identity fixed when the definition or environment is
created, with permission limited to launching that definition's runs and sweeping by label; that standing access is a risk (K7). ucdk 1.0 keeps its direct launcher (`ucdk run` or the generated workflow, against the cloud). jobscheduler is an optional second realization of Job; it is offered as supported only after a
conformance kit passes on a real cloud. Its run store is not the environment ledger; ledger entries name the scheduler instance and the
Job definitions, and runs are not nodes.

- **Launcher.** `launch` submits a request and returns after the run record is durable (UL1); `status` reads the record (UL2); `logs`
  returns text from a cursor with a `complete` flag (UL3); `cancel` calls `stop` and reports what was done (UL4). The ucdk `run` verb
  and the generated run workflow authenticate and call this API.
- **Infrastructure provider.** The ucdk-side crate that implements ucdk's `Provider` trait for Job definitions and schedules, its emulator
  and its kit run (`ucdk/crates/ucdk-provider/src/kit.rs`) live in the ucdk repository and call jobscheduler's public client API.
  jobscheduler exposes only definitions, schedules, runs and a list-by-label call, and names no ucdk type.

Definitions are infrastructure, runs are runtime events. Launcher first (D5).

### 11.2 Mapping
| ucdk Job field | Request field |
|---|---|
| image, args, environment | payload; secret references resolved by the provider, never stored in the record |
| size (abstract CPU and memory) | resource vector, rounded up by the provider, shown in the plan; the booking and the ceiling (11.3) |
| max_retries | one budget over every failed attempt (8.1) |
| timeout | per-attempt deadline enforced by the provider |
| trigger schedule | one owner per realization (below) |

`Qos::Batch` orders the queue only. With no preemption (N1) it does not make a job interruptible, and the ucdk `Job` message has no
`interruptible` field today, so no row maps one.

**Schedule ownership.** A Job has exactly one scheduler. Under the direct realization the cloud scheduler role node calls `launch`
(or the cloud's own job API) and `overlap: forbid` is a guard there. When Job is realized by jobscheduler, the lowering drops the cloud
scheduler role node and emits the trigger into the definition, where jobscheduler owns tick dedupe, overlap, missed ticks (skipped, no
catch-up) and zone (UJ3). The choice is in the lowering table, and UJ3 is written once as a kit case that both realizations pass.

### 11.3 Gaps
- **Fence.** kbf hermetic actions may run on through a lost connection; infrastructure Jobs are not idempotent and must self-fence, or
  carry an explicit idempotency declaration. The class declares the default; run-on only when the definition declares hermetic or
  idempotent (D6). The fence field is on `Start` (10.3).
- **Run store.** ucdk's ledger is infrastructure state with leases (`ucdk/crates/ucdk-ledger`), not a run store. The scheduler owns run
  history (8.3). Retention is an open portable minimum in ucdk.
- **Retries.** The scheduler can do more than UJ1 (backoff, ladder). These are capabilities a Job may use, not part of the portable
  contract.
- **Sizing.** A ucdk Job's declared size is shown in the plan, so for ucdk classes it is the booking and a hard ceiling (`Declared`
  mode, 7.2). Learned history only informs a warning or a suggested size and never raises it (D7).
- **Tenant.** For ucdk the tenant is the (app, environment) pair, taken from the authenticated caller (K7).

## 12. Cost guardrails

State today: kbf has no quota, budget or spend mechanism. Its "budgets" are retry and storage budgets. Per-lease usage accounting is
planned (`docs/design/scheduler.md`, Accounting). ucdk has shape limits only (attempts, timeout, argument size, size), and its 1.0 plan
does not include dollar cost control. The mechanisms below are therefore guardrails that bound a runaway job or tenant, not cost control.

R1 forbids reservation and per-category admission for kbf farm work. The mechanisms split in two. **Always on, for every class:** the
per-run caps and other start-contract checks (12.1), accounting records (12.3) and labels (12.5); none changes how kbf places or runs
work. **Off by default, per class opt-in, behind a named flag:** tenant concurrency and queue-depth caps (12.2) and budgets (12.4).
kbf's `action` class does not opt in, so kbf placement does not change.

### 12.1 Per-run caps (v1, always on)
Max wall time (required), max attempts, max resource size, max arguments size. Checked at submit, as part of the start contract (5.2),
and refused there; a per-run cap is never a Held reason. A refusal carries a plain constant code; no central code table is introduced
until a second consumer needs one.

### 12.2 Concurrency and queue-depth caps (v1, opt-in)
A per-tenant cap on concurrent leases and on queued requests, for opted-in classes. This is a `Queue` concern: a request at its tenant's
cap is *Held*, not Queued, and the queue skips that tenant and leaves other tenants unaffected. It is not a `Fit` concern, so it cannot
reduce the capacity a kbf lease sees. Caution: a per-tenant cap is per-category admission in a loose sense. D8 asks the owner to confirm
it is acceptable for non-kbf classes.

### 12.3 Resource-second accounting (v1, always on)
One record per lease: CPU-seconds, GB-seconds, GPU-seconds, peak memory with basis, wall phases, queue time, and whether it ran, was cached
or was joined. *Cached* (an action-cache hit) and *joined* (coalesced onto an in-flight run of the same key) are kbf-only record values;
other classes record *ran*. Stored in the state store, queryable, and feeding the sizing history and any budget. This is the "Accounting" record
planned in `docs/design/scheduler.md`. Attempts charged to the platform-fault budget (8.1) are recorded and flagged, not billed.

### 12.4 Budgets (v2, opt-in)
Per tenant per period, in resource-seconds first. Money only where a provider returns `price()`. Checked at admission (a request over
budget is Held), accrued from usage at lease end. Soft by default (warn), hard only when configured. Demoting a tenant's work to `Batch`
after a soft limit needs the `Queue` seam and a QoS rewrite, which is queue policy and belongs with v2 and the fairness work this design
does not build. v1 therefore has no demotion.

### 12.5 Labels on billable resources (always on)
Every request carries labels. jobscheduler owns a small label schema and imports no ucdk type: a reserved prefix `jobscheduler.dev/`
with the keys `tenant`, `run`, `validation-run`, `definition` and `instance`, each value at most 63 characters of `[a-z0-9._-]`. The
caller mints the validation run id; the scheduler only carries and applies it. A user label under the reserved prefix is refused at
submit. Labels flow request, lease, `Start`, provider. A delegated provider writes them as tags or labels onto each billable resource
it creates, so cost reports and leak sweeps can find them. In ucdk, the adapter that lives in ucdk translates the label set into
`Stamp.extra`. The name-encoding fallback of the stamp (`encode_name`) does not carry `extra`, and a create that asks a tagless kind for
extras is refused. So a class that carries a validation run id is refused at submit on a kind that cannot carry labels, unless that
kind's adapter defines a name rule that includes the id.

Runs are not nodes in any ledger, so there is no node key to stamp. The sweep key is the `instance` label. Destroying a Job definition,
or the environment, cancels or drains its runs and sweeps billable leftovers by that label; the sweep is part of the closed-world
delete, with a test on the fake.

### 12.6 Spend needs a go
A provider that bills a real account is registered disabled. Enabling it needs an explicit go for that action (R7). Conformance runs
against billable accounts only in a dedicated sandbox account with a billing alarm. The fake provider covers everything else.

## 13. Testing

### 13.1 Fake providers
A `FakeProvider` in `js-sim` models both flavours with a controllable clock and injectable loss, fence, out-of-memory kill, node
pressure, phase changes, unknown observations and slow starts. Starting points: `kbf_daemon::Runtime`'s fake in
`crates/kbf-daemon/src/runtime.rs` and `crates/kbf-sim-cell`. The fake is the executable spec, as in ucdk (`ucdk/crates/ucdk-provider-fake`).

### 13.2 Provider conformance suite
Every provider runs the same suite, including the kbf daemon adapter and the fake itself. The suite is flavour-aware: checks that expect
a synchronous answer apply to delegated providers, and checks that expect an event order apply to agents. It checks:

- start is idempotent on lease id; a second start does not create a second job or run;
- delegated: `stop` reports what happened and a read-back agrees; agent: a `Cancel` is resent while the lease is listed and the lease
  leaves the heartbeat;
- a stale-lease result is refused; a duplicate is acknowledged and ignored;
- reconciliation frees an absent lease and keeps an unknown one; for an agent, a lease omitted from a heartbeat is absent only once its `Start` is due (5.2, graded by session and process as in 4.4) and pending before then, and silence is not absence;
- agent: a Start past its validity bound produces `NeverStarted`, no Result and no listing; the heartbeat lists unacknowledged results and
  the first heartbeat after re-adoption is complete;
- an unknown phase value is read as quarantined;
- usage is reported on failure and on kill;
- labels are present on every created resource;
- each start-contract rule (5.2) refuses before any provider call: non-positive wall time, unstated fence, reserved label key, retries
  passed to a provider.

The hooks **read the provider's real state**: for the fake, its internal tables; for a cloud, the cloud's API read independently of the
adapter. A suite that asks the provider whether it did what it said cannot fail. `js-provider` therefore ships one deliberately broken
provider per rule above, each asserting that the suite reports exactly that rule's failure, and a test that counts the broken providers
against the rule list so a new rule without one fails the build. These run on every CI run and are named in the suite's own test names.

### 13.3 Simulation
The scheduler invariants I1 to I16 and L1 to L4 are checked after every input in seeded sweeps. For the kbf instantiation, the golden
trace hashes (step 0) must be identical before and after each extraction PR. New invariants for providers: an Unknown observation never
frees a booking; an Absent observation frees it exactly once; a delegated lease never has two live submissions. Once tenant caps exist (step 26), two more: a tenant cap is never exceeded, and a Held request never counts toward the unservable-wait refusal.

### 13.4 Request-history replay
`kbf-estimator-replay` becomes the template: a generic trace of (arrival, need, duration, key, capacity) and a pluggable arm, run through the real scheduler in virtual time with null, floor and candidate arms and Wilson intervals, labelled an upper bound. Export and row shaping stay kbf glue.

### 13.5 No timing assertions
(R2.) Quota windows, schedule ticks, timeouts, grace periods and budgets are tested with an injected clock that advances only when the
test says so, and by counting events (progress events, ticks applied). No test asserts an elapsed time or sleeps against a timeout. The
core already takes time as input (`Input{now, event}`), so the core needs no change. Adapters take a clock trait. A soak of real daemons
counts explicit events (leases started, results acknowledged, zero unacknowledged) and asserts no elapsed time.

### 13.6 Coverage
(R6.) A crate in this repository has zero uncovered lines and branches, and a crate still in kbf holds kbf's 99% or its recorded floor.
The gate counts a generic by its best single instantiation, and a function compiled into several test binaries by its best-covered copy,
so every branch of a body must be hit by one instantiation in one test binary: the trivial test type gets the full suite, or
arm-bearing bodies are made non-generic helpers. A second instantiation is not a substitute. Until the repository split (step 22) the
`js-*` crates build in kbf's workspace under kbf's rule: each move step lists the destination crate's own tests that carry its 99% and
moves the floor entries (`coverage-floors`) in the same PR, recording old and new per crate. At the split each crate enters this
repository's `coverage-baseline` at zero, so it is brought to zero uncovered lines and branches with its own tests before step 22.
`js-sched` must get there with only its own tests, not those of `kbf-sim` or `kbf-sim-cell` (its generic code is exercised mostly from
those today), so tests move with it or are written first. Test target names must not collide with files that tests write under `target/`. A bug fix ships with a test that
failed before it.

### 13.7 Persisted and wire compatibility
The golden trace hashes say nothing about stored or sent bytes. Step 1 commits a corpus of control-log rows, store rows and wire messages
produced by the last released binaries. Every step that touches a persisted or sent type decodes the corpus and re-encodes it
byte-identical. A rolling-upgrade recovery test recovers state written by the previous release with the new one, and reads the new
release's writes with the previous one.

## 14. Extraction order

Each step is one small PR (or a short series, each green alone) that leaves kbf green. "Proves" says what the PR's tests show. Gate on
every step from 3 on: see 10.4 item 2. PR text opens with what an operator sees.

**Phase 0. Gates.**

0. Golden traces. Write every trace line in an explicit, hand-written format that does not use `{:?}` (in `crates/kbf-sim/src/sim.rs` and
   the scenario worlds), covering both hash mechanisms of 4.5 (SHA-256 in `kbf-sim`, 64-bit FNV-1a in the F1 capacity world) and saying
   whether the 64-bit hash is replaced. Check in golden hashes per scenario and seed for the CI sweep. Own PR; the old hashes are regenerated once and the reason stated. *Proves:* the golden file fails on a one-event change in a scenario.
1. Golden corpus (13.7) and the cross-version harness skeleton. *Proves:* the corpus decodes and re-encodes byte-identical on main.

**Phase A. Inside kbf, no behaviour change.**

2. Documentation corrections: queue persistence text in `crates/kbf-sched/src/lib.rs` and `docs/design/scheduler.md`; the stale sentence in `crates/kbf-estimator/src/lib.rs` ("Only the offline replay (`kbf-estimator-replay`) calls it yet: the server
   adapts its run records to these types in a later change"; `crates/kbf-server/src/learned.rs` calls it); the lines of `docs/design/failure-classes.md` that still say the booking doubles (found by searching for "doubl"; line 527 says the code no longer doubles); the `memory_bytes` and `MEMORY_KILL_OWN_LIMIT` comments in `worker.proto`. *Proves:* nothing in code changes; docs agree with the files cited.
3. Split `crates/kbf-types` into a generic module and a REAPI module with re-exports. *Proves:* the workspace builds with no import edits
   outside the crate.
4. Introduce `Queue` around the ordered set. *Proves:* golden traces unchanged.
5. Introduce `Fit` and `Rank` with the existing code as the implementation. *Proves:* golden traces unchanged; a test with a second
   trivial `Rank` shows the seam is real.
6. Additive durable fields. Add optional `class_id` (absent means the `kind` enum), `payload`, `labels` and `tenant` to the control-log
   `Request`; the old fields are still written for kbf classes, and decoders accept the new fields. A non-kbf row is written only when
   its class is enabled, and a class stays disabled until every frontend sharing the store reads it. *Proves:* kbf rows byte-identical
   (`control_golden.rs`, `state_golden.rs`); a cross-version test shows an old reader rejects a non-kbf row instead of misreading it.
7. Add optional `fence`, `limits`, `assignments`, `labels` to `kbf.worker.v1` `Start`; teach scheduler and daemon; absent means today's
   behaviour. *Proves:* cross-version matrix both directions; behaviour identical with fields absent.
8. Replace the `LeaseKind` match in `has_room` with `ResourceClass` data; kbf maps its three kinds and stores keep the enum. *Proves:* the
   lease-kind tests (`crates/kbf-sched/tests/lease_kind.rs`, `vm_slots.rs`, `gpu.rs`) pass unchanged.
9. `RetryPolicy`. 9a: introduce the trait with the current four axes and code behind it. 9b: split `memory.rs` below the file-size limit
   with no code change. *Proves:* `crates/kbf-sched/tests/memory.rs`, `requeue.rs`, `lost.rs`, `sim_memory.rs` unchanged.
10. Make `Request` generic over `Payload` and `Outcome` over the result blob, instantiated with `ActionKey`. *Proves:* golden traces
    unchanged; the trivial instantiation carries the full suite (13.6).
11. Make `Resources` a named vector with the four dimensions; `leases` stays a separate typed field. *Proves:* corpus bytes identical; a
    stored or wire `Resources` with an unknown extra dimension is skipped by the old decoder and preserved by the new.
12. Delegated spike. In a test-only crate, build `FakeProvider` with the delegated flavour against the draft traits (`Provider`, `Queue`,
    `Fit`, `Rank`, `RetryPolicy`, `Payload`); run the three-way observation invariants (Unknown never frees a booking, Absent frees
    exactly once, no double submission) over seeds. The fake lives in this test-only crate until step 18, when it moves into `js-sim`;
    the `js-provider` crate is created at step 23. Freeze the traits only after it passes. *Proves:* the observation invariants of 13.3
    hold over the seeds; a trait change found here costs no crate move.

**Phase B. Move crates out (still building in the kbf workspace).**

Steps 13 to 15 come before `js-types` moves at 16a, so each may use only types that still live in `kbf-types`. The estimator has no
dependencies on other kbf crates (4.3). Where `SizingPolicy` (resource-vector windows) or the capability schema needs a shared type, step
14 or 15 states which type it takes and re-exports it, or the step is ordered after 16a.

13. Move the `Store` trait, fake and conformance suite as `js-state`; the backends stay (D15). *Proves:* all three backends pass the suite
    from the new path; the corpus is unchanged.
14. Move `kbf-estimator` as `js-sizing` with `SizingPolicy`; kbf passes today's values. *Proves:* golden outputs identical on recorded
    input; the codec version is unchanged; `shadow_diff` passes.
15. Move `kbf-caps` matching as `js-caps` with the schema; kbf supplies its tables. First pin kbf's present behaviour on an unknown request
    key with a test that passes on main (own commit). Then make the schema's unknown-key policy a parameter (refuse, or skip and report),
    with kbf supplying today's value; both are tested on the trivial schema. *Proves:* the existing capability tests pass. kbf's policy
    does not change in this PR.
16. 16a: move `js-types`. 16b: move `js-sched`, and update in the same PR the layering test's pure set, forbidden list and clippy-deny
    check to cover both crates. 16c: the server adapter translating events and effects. *Proves:* golden traces unchanged; `js-sched` has
    no `kbf-*` dependency; `js-sched` holds 99% line and branch with its own tests only.
17. Move the generic durable transitions into `js-store`; kbf extension rows (floors, key floors, estimator rows, done-of) stay. *Proves:*
    recovery tests, multi-frontend tests and the rolling-upgrade test (13.7) unchanged.
18. Move the simulation harness and invariants into `js-sim`; kbf's simulation instantiates it. *Proves:* same seeds, same golden hashes.

**Phase C. Wire and agent.**

19. Extract `js-contain` from `kbf-driver-container`. *Proves:* the driver's cgroup tests pass from the new crate.
20. Define `jobscheduler.agent.v1`. 20a: the proto and the server adapter, no daemon change. 20b: the flavour-aware conformance suite and
    its broken providers (13.2) on the fake. 20c: the kbf daemon passes the suite through the adapter. *Proves:* the suite is green on
    the daemon and the fake and red on each broken provider.
21. 21a: split the daemon into `js-agent` and kbf extensions with no wire change. 21b: switch the daemon to agent v1 using the negotiation of 10.3; it ships only after the release containing step 20 is the minimum server. *Proves:* the cross-version matrix including the downgrade after the
    switch; the same seeded scenario run through the real daemon on the fake runtime yields the same event trace as the v1 path; a farm
    soak counts explicit events with zero unacknowledged results and asserts no elapsed time.
22. Repository split, after Phase C and as a mechanical move: create the jobscheduler repository with history-preserving copies; kbf pins
    it by revision; the layering test becomes a cross-repository dependency check; each crate enters `coverage-baseline` at zero (R6); the
    public-repo lint runs on the new repository. *Proves:* golden traces and corpus unchanged.

**Phase D. New capability.**

23. Create `js-provider`: the delegated-provider skeleton, built from the step 12 spike, with a fake cloud; the new `Stop` effect and
    its retained-intent row (5.1, 5.2). *Proves:* the three-way observation rules (5.6) in simulation; a stop of a held lease is
    resent until the lease leaves the heartbeat (agent) or read back (delegated).
24. Accounting record per lease; labels. *Proves:* usage on failure; labels on every fake resource.
24b. The *task failure* retry axis of 8.1 and the portable `max_retries` budget. *Proves:* a UJ1/UJ2 case on the fake in which a
    non-zero exit is retried up to `max_retries` and a kbf class with budget zero is not retried.
25. Schedule triggers and run records; the launcher API. *Proves:* run record before return; missed ticks skipped; overlap rules, all
    with an injected clock.
26. Tenant caps as `Queue` plug-ins, off by default behind a named flag (R4); per-run caps are start-contract refusals (12.1) already
    in force. *Proves:* with the flag off, golden traces are unchanged; a frontend started without the flag next to one with it agree
    on persisted state; with it on, the invariants "a tenant cap is never exceeded" and "a Held request never counts toward the
    unservable-wait refusal" (13.3) hold across seeds.
27. Budgets (v2).
28. ucdk side: the launcher wiring and, later, the provider crate, in the ucdk repository.
29. First real delegated provider, disabled by default; enabling it needs the go in R7.

## 15. Decisions

Each has options and a recommendation. None is final until the owner answers. The ids run D1 to D15 with no gaps.

- **D1. Home of the code.** This repository holds the design from the start; the question is where code lives. (a) Code in this
  repository now; (b) workspace crates inside kbf through step 21, split at step 22. Recommend (b): no cross-repository flag day while
  the dependency edge is still two-way.
- **D2. Wire strategy.** (a) dual stack in the daemon; (b) a server-side adapter and a new package with negotiation; (c) edit v1 in
  place. (c) breaks R3. Recommend (b), daemon switched last and only after the server advertises the package.
- **D3. Durable leases across agent restart.** (a) required in v1; (b) not in v1, a provider capability flag. Recommend (b); kbf kills
  leases with the daemon today.
- **D4. Sizing rule for seen work, and the ladder.** R1 says last peak plus buffer; the shipped estimator is a long-window maximum plus 20%
  and 128 MiB, and `on` mode applies only the kill-driven floor. Options: (a) implement R1 as `SeenRule::LastPeakPlusBuffer` and replay
  it; (b) keep the shipped estimator behind the generic API and amend R1; (c) both selectable per class. Also decide where "a lease is a
  node or half a node" lives, since it was not found in code. Second part: R1's "next rung above" is implemented as above the *booking*,
  not above the limit just used (7.4). Options: keep (current behaviour) or move to above-the-limit, which is a behaviour change with its
  own replay proof and not part of the extraction. Recommend (c) with (a) as the default after a replay supports it, and keeping the
  ladder as it ships.
- **D5. ucdk role.** (a) launcher only; (b) infrastructure provider only; (c) both. Recommend (c), launcher first, as an optional
  realization beside ucdk's direct launcher (11.1).
- **D6. Fence.** (a) self-fence for everything; (b) the class declares a default (self-fence unless the definition declares hermetic or
  idempotent) and every request resolves and records its fence at submit, so no default is silent. Recommend (b).
- **D7. Memory booking for non-kbf classes.** (a) same as kbf; (b) `BookAll` for ucdk job classes, with the declared size as booking and
  ceiling and history advisory only. Recommend (b); kbf actions keep today's rule.
- **D8. Tenant concurrency caps.** Allowed as opt-in `Queue` behaviour for non-kbf classes? Options: (a) yes; (b) no caps in v1, per-run
  caps only. Recommend (a), since without it one tenant can fill a delegated pool; kbf's class never opts in.
- **D9. Guardrail scope.** (a) v1 = per-run caps, tenant concurrency cap, accounting, labels; (b) include budgets. Recommend (a).
- **D10. Validation run id and label key.** The caller mints; the scheduler carries and applies; the key is `jobscheduler.dev/validation-run`
  (12.5). Recommend yes.
- **D11. Triggers.** Cron-like triggers live in job definitions as data, with UJ3 semantics, when Job is realized by jobscheduler; the
  cloud scheduler role node is dropped in that realization and kept in the direct one (11.2). Recommend yes.
- **D12. Resources and capability schema.** Named resource vector in v1 with the lease-count cap separate; whole-device GPUs, no fractional
  GPUs. Capability schema: provider-registered, namespaced keys, unknown request keys refused, unknown report keys skipped. Recommend yes.
- **D13. Consensus.** The generic core relies on the state store's conditional writes for who places (`crates/kbf-sched/src/scheduler/handoff.rs`), not on a
  consensus log. `crates/kbf-raft` exists in kbf. Options: (a) the core requires only the store; (b) the core also supports a replicated
  log. Recommend (a); `kbf-raft` stays in kbf until a second user asks.
- **D14. Name collision.** jobscheduler's *provider* and ucdk's *Provider* trait are different. Options: rename ours to *runner* or
  *backend*; or keep and always qualify. The ucdk-side adapter lives in ucdk (11.1) and no jobscheduler code names ucdk's trait, so the
  clash is confined to prose. Recommend keeping "provider" here and writing "infrastructure provider" for the ucdk side; the rename is the
  owner's option if the prose proves confusing.
- **D15. Where the state store lives.** `kbf-state` is general (thirteen kbf crates use it, 4.2). Options: (a) move it all into
  jobscheduler, which widens "kbf depends on jobscheduler" to installer, enrollment and history; (b) move only the trait, fake and
  conformance suite as `js-state` and keep the SQLite and Postgres backends in kbf; (c) keep it all in kbf behind a neutral crate.
  Recommend (b).

## 16. Risks

- **K1. The extraction changes kbf behaviour invisibly.** Mitigated by golden trace hashes (step 0), the golden persisted and wire
  corpus (step 1), golden outputs for the estimator and golden bytes for rows. Risk remains where the simulation does not cover a path;
  list uncovered paths per step.
- **K2. Delegated-provider absence handling is wrong.** Treating Unknown as Absent double-runs work and double-bills. Mitigated by the
  three-way rule, a durable start intent, the step 12 spike, simulation invariants and a conformance suite that reads real state.
- **K3. A guardrail leaks into kbf farm work.** Mitigated by tenant caps being off by default and class opt-in, the cap living in `Queue`, not `Fit`, and the always-on checks (5.2) being
  written so that kbf requests pass them unchanged. A
  golden-trace check with the flag off is part of step 26.
- **K4. Wire drift.** Two packages and an adapter can disagree. Mitigated by one conformance suite running through the adapter, by the
  negotiation rule and by the cross-version matrix at every wire step.
- **K5. Rules in conflict.** A rule (R1) forbids per-category admission; D8 proposes opt-in tenant caps. The conflict is stated, not
  hidden, and the owner decides.
- **K6. Secrets.** `secret_env` references must never reach the run record, labels or logs. The record stores references only; the
  provider resolves them.
- **K7. Tenancy and identity.** The scheduler has no tenant concept (3, 4.1). A tenant must come from an authenticated caller, not a
  request field. kbf already authenticates frontends and nodes (`crates/kbf-auth`, `crates/kbf-oidc`); the design reuses that identity
  path and does not add a second. Scheduled triggers and the leftover sweep run with no caller present (11.1), so they need a delegated
  identity with standing access to launch and sweep for one definition; the tenant detail and the scope and rotation of that identity belong in
  a follow-up document due before step 25.
