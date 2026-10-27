# What exists in kbf today

Companion to [docs/design.md](../design.md), which cites this file as section 4 (4.1 to 4.6). The text is the section as first written,
unchanged except that this header is added. Paths refer to the kbf repository (komira-ai/kbf) at commit 45e420656 and the ucdk
repository (komira-ai/ucdk) at commit f28c240. A statement marked (not found) is a keyword result, not a proof of absence.

## 4.1 Scheduler core
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

## 4.2 Server and durable record
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

## 4.3 Request history
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

## 4.4 Node side
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

## 4.5 Simulation
Invariants I1 to I16 and L1 to L4 (`docs/design/simulation.md`) are checked after each input in seeded runs of `crates/kbf-sim` and
`crates/kbf-sim-cell` (`crates/kbf-sched/tests/sim/cell/check.rs`). I15 says only that two runs of a seed in one build give the same
trace hash. Two hash mechanisms exist. `kbf-sim` hashes its trace lines with SHA-256 (`crates/kbf-sim/src/trace.rs`), and those lines
embed `{:?}` of effects, messages and partitions (`crates/kbf-sim/src/sim.rs`). The F1 capacity world hashes a text stream with a 64-bit
FNV-1a (`crates/kbf-sched/tests/sim_f1_capacity/world.rs`); it writes inputs with `{:?}` and effects in a hand-written format. No golden
hash is stored (a search of `crates/*/tests` found only same-process comparisons; the one literal hash found is the SHA-256 of empty
input). So today nothing compares a trace before and after a PR, and any Debug-format change moves the hash. Step 0 covers both
mechanisms and both line formats, and states whether the 64-bit hash is replaced.

## 4.6 ucdk
ucdk declares compute and does not schedule it. `Job` is in `ucdk/crates/ucdk-proto/proto/ucdk/resource/v1/resource.proto` (image, args,
size, retries, timeout default 600 s, environment, secret environment, on-demand or cron trigger). It lowers to a `run` node and a
`Schedule` role node (field 11, role "schedule"; `ucdk/crates/ucdk-plan/src/catalog/`), which the clouds realize as their own scheduler
service. ucdk keeps one state per environment (`ucdk/docs/architecture.md`): an environment is one provider, account and region with its
own ledger. ucdk's `Provider` trait (`ucdk/crates/ucdk-provider/src/provider.rs`) creates, observes, updates and deletes
*infrastructure objects*; it is a different thing from the jobscheduler provider (D12). Objects carry a `Stamp` (owner, node,
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
