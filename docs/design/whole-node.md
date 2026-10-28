# The whole-node lease (5.17)

Companion to design.md section 5.17. This file is the one home of the whole-node class: what it is, how it is admitted and queued, how a
host is drained, what caps keep it from starving a pool, how it ends, and how kbf uses it. Other files point here and do not restate it.
Section numbers refer to design.md. "Node" and "host" mean the same thing: one machine running one `js-agent`.

## 1. What it is

**Giving a whole node is a lease class of its own, `whole-node`.** It is not a large resource request. A request for many CPUs and much
memory under `Book` admission is a share that happens to be big; it can be placed beside other work when the numbers allow, it starts on
whatever state the host is in, it is never drained for, and it has no cap. A whole-node lease promises four things a big share does not:

1. **Exclusive.** It books every dimension of the host (CPU, memory, disk, all GPUs, all `vms`), and from the moment it is committed until
   the host is released back, nothing else is placed on the host: no other lease, no run placed by the scheduler, no `Start` for any other
   lease.
2. **Quiet.** Before the consumer can use it the host has no run, no VM and no leftover of an earlier job, the post-job checks are clean
   and the host is not tainted. The consumer receives the lease token only then (D30).
3. **Reserved.** A queued whole-node request is given exactly one host by draining it, so a stream of small requests cannot starve it.
   This is the only reservation the scheduler makes (R1 forbids adding any other: a large non-whole request has none).
4. **Capped.** Pool limits (section 6) bound how many hosts are reserved or held whole, so whole-node requests cannot starve a pool.

**Admission.** The class is admitted as `Admission::Whole` (6.2): the request needs a host with no lease and no run and books all of it.
`BookAll` stays as it is (book every dimension of a host that is free, no overcommit); ucdk Job classes are to stop using it (step 59), booking their
declared size and sharing a host (D7, D56); it has no reservation, no quiet gate, no drain and no cap. `Whole` is `BookAll` plus those four, and nothing else about it differs.

**How the old names relate (D46).**

| Name | Meaning now |
|---|---|
| kbf `LeaseKind::WholeMachine`, class `whole_machine` | kbf's name for this class; the consumer's configuration maps it to class `whole-node` (9.4) |
| `gpu-host`, whole-node admission (merged as `BookAll`, which `Whole` extends; the merged lease service, D32) | a **retained alias**: a whole-node class whose id stays `gpu-host` and whose `min_gpus` stays 1. It is still accepted, still reported under the id `gpu-host`, and still refuses a host without a GPU with the merged text (`class gpu-host needs at least 1 gpus`). Only its admission semantics unify with `whole-node`: it gains the reservation, the quiet gate and the caps when step 46 and steps 56 and 57 land; until then it behaves as merged. Generic `whole-node` has `min_gpus` 0 (a pool of performance machines needs no GPU); GPU-ness of a request comes from its constraints (`gpus > 0`, `gpu_class`). No merged assertion changes: `classes()`, the error text and requests with class `gpu-host` are as merged. |
| a node lease of class `action` or `vm` that books the shape of a host | not whole-node. It is a share whose size equals the host; nothing else fits beside it, but it gets no quiet start, no reservation, no drain and no cap, and it counts nothing against the whole-node caps. kbf's standing node leases (step 34b) are these. |

The GPU path is this class: a GPU request is served only as a whole-node lease on a pool whose shape has a GPU (the Mac Studio machines of
the GPU pool, D32). A GPU request under any other class matches no GPU pool and is refused `no-matching-pool` (pools serve the class they
name, 6.2). A VM still gets no GPU (5.14): GPU work runs as a job of the whole-node lease.

**Two kinds of pool.** A *dedicated* pool has `class = "whole-node"`: every lease in it is whole-node and there is no ordinary work to
protect (the GPU pool, a pool of performance machines). A *mixed* pool has `class = "action"` or `"vm"`; it offers whole-node leases only
when its `max_whole_node` is set to 1 or more, and then protects its ordinary work by the caps of section 6. A pool with no `max_whole_node`
does not offer the class.

## 2. Eligible hosts and admission

A whole-node request carries a class, label selectors that pick pools (6.2), constraints (5.13), and optionally a named host. Its eligible
hosts are the **capable** hosts of D41 in the pools it matches: in the table, not tainted, not denied, not draining (agent-announced), not
`releasing`, with facts that meet every constraint. Capabilities and constraints apply exactly as for any lease. A tainted or denied host is
never eligible and never becomes so by waiting (D41). Busy, lost (inside the retention window), cordoned and quarantined hosts are eligible:
each ends without a person replacing the machine. A host reserved for another request is eligible (it is not leaving) but is not available
until that reservation ends. A named host narrows the eligible set to that host; if it is not capable the request is refused.

**Refuse versus queue (D35).** At submit:

1. No pool matches class and selectors: refused `no-matching-pool`.
2. A pool matches but is configured not to give a whole node (a mixed pool without `max_whole_node`): it does not match, as in 1. A pool
   with `max_whole_node` set but a transient shortfall of hosts (section 6) is not this case; see 4.
3. No eligible host exists in any matching pool and no elastic provider of a matching pool could launch a machine that meets the
   request (the test of D35 for "could launch"): refused `no-capable-host`. This is the only refusal for lack of a host: it holds
   exactly when **no registered host could ever be given whole**.
4. Otherwise the request is accepted and queued, however busy the hosts are and whatever the caps allow at this moment. A queued request
   whose last eligible host leaves keeps waiting `regain_grace` and is then refused (D41). Time spent queued, reserved and draining counts
   toward the consumer's wait (`UNSERVABLE_WAIT`, 8.2) like any queued request.

## 3. Queueing and reservation

Whole-node requests wait in the pool's queue in queue order (6.1). At each tick, in queue order, for each whole-node request that holds no
host:

1. **A quiet host first.** If an available eligible host is already quiet (no lease, no placed run, no running job), the request takes it
   at once, subject to the caps (section 6). Choosing the host and placing nothing else on it commit under the table lock; the first
   commit wins (5.10). This needs no wait and is never delayed by `drain_after`.
2. **An elastic machine second**, if the pool's hierarchy has an entry that can launch one now (section 7).
3. **Otherwise a reservation**, once the request has waited `drain_after` (default zero: reserve at once, as kbf's whole-machine request
   does today; a pool where whole-node requests are rare may set a delay so a host that would empty by itself is not drained).

**One request reserves exactly one host.** A reservation is a mark on a host in the scheduler's table: `reserved` for request R. A request
holds at most one reservation and a host carries at most one. The host is chosen by the **drain cost**, smallest first:

1. the latest end bound of the work on the host: for each lease and each run the scheduler placed, its start plus its max wall time
   (12.1), minus now, and zero for a host with none. This is a worst case: every lease ends by its max wall time, and a renewal never
   extends it past that;
2. then the fewest leases on the host;
3. then the lowest host id (the order of the table), so the choice is deterministic on the injected clock.

A reserved request that finds another available eligible host already quiet takes that host and drops its reservation. Otherwise it keeps
the host it has and does not switch to a cheaper one later (no thrash). Two requests never share a host; the next request in queue order
picks the cheapest host that no reservation holds. A request that is cancelled by its consumer, or whose wait ends, drops its reservation
at once and the host returns to `serving`.

**Reservations survive a restart (D55).** A reservation is persisted in the lease file (5.16, [deployment.md](deployment.md)), in the same
atomic write as the leases and under the same single-writer, fsync and lock rules, before the scheduler acts on it (commit before act). A
*reservation record* holds: the request id, the host, the time reserved, the deadline, the request's two counters (`max_reserve_attempts`
give-ups and `max_replace` prepare failures) and its host exclusions with their end times. The record outlives the reservation only
while the request lives (it carries the counters of a request between reservations) and is deleted when the request is granted, refused,
cancelled or unclaimed. The queue itself is not persisted (the table holds leases, not requests); a consumer presents the same request id
again after a restart, as it re-presents any request, and the scheduler binds it to its record, so the drain in progress continues with its
original deadline (raised to at least boot plus the boot grace, never the stored deadline plus the grace; the lease-side grace is a separate setting wired by M3-n) and the counters keep the bounds of sections 4 and 5 across any number of
restarts. A record not claimed within `regain_grace` of the boot is dropped, and the host returns to `serving`. Why persisted: the drain
is the expensive step (up to a max wall time of idle host), and a restart that dropped it would restart every drain and let a restart loop
defeat `max_reserve_attempts`. A restore from a snapshot reads the records as they were then; a record naming a host or request that no longer
exists is dropped at boot. Cost: a few small rows, written with the same rewrite as the leases.

## 4. Draining

From the moment a host is reserved, **nothing new is placed on it**: no new lease, no placed run, no new `Start` for a lease that did not
exist. (The scheduler-side mark is `reserved`; the agent-announced phase `draining` of 5.3 is a different thing, and a reserved host is not
"draining" for D41, because it is not leaving.) What runs on it **finishes or reaches its end**, and nothing is killed:

- A lease ends by release, by TTL, or by its max wall time, whichever is first. The renewal answer of a lease on a reserved host carries
  `host_reserved: true` (an additive field, R3): it asks the consumer to place no more work there and to release when its work is done
  (a consumer drains before it releases, 9.1). A consumer that ignores it keeps the lease to its TTL or max wall time; a renewal is not
  refused for it. The agent does not refuse a `Start` for a lease that already existed, since that would end the consumer's work without its
  say; keeping to the request is the consumer's side of the bargain.
- The scheduler never stops a lease or a run because of a reservation (D48). The only work released early is the requesting consumer's
  own, if it chooses to release it.

**The drain bound.** Every reservation has a deadline, `drain_deadline` (pool configuration; default: the pool's max wall time, so that no
running lease is cut short by design: by that time every lease that was on the host at the reservation has ended). An operator may set it
shorter. When it passes with the host still not quiet, the reservation is **given up**: the host returns to `serving`, nothing on it was
touched, the request returns to the queue with that host excluded for it for `reserve_backoff`, and it reserves the cheapest other host by
the rule above. If no other host is available the request waits unreserved until the back-off ends. Give-ups are counted per request:
after `max_reserve_attempts` of them (default 2) the deadline of its next reservation is the pool's max wall time whatever
`drain_deadline` says, and that reservation cannot be given up for a non-quiet host, since by then every lease on the host has ended.
Load validation refuses nothing here; the counter alone makes the bound hold for every setting. Stopping other tenants' work was
considered and rejected (D48): a deadline that kills is a silent loss for a tenant who did nothing wrong, and it would let one consumer's
request destroy another's results; re-queueing wastes only the drain already spent, which the caps bound.

**The host is quiet** when the scheduler's table shows no lease and no placed run on it. The scheduler then commits the whole-node lease
(state `pending`), the host mark becomes `held`, and the lease follows section 5.

## 5. Ready, the token, and the clock

1. **Preparing.** The next heartbeat reply tells the agent that an exclusive lease is granted (`exclusive`: lease id, additive). The agent
   announces the phase `preparing` (no new leases), stops accepting a `Start`, confirms that no VM is running (one found is a finding and is
   destroyed), that no job is running (it waits for a run the scheduler did not know of to end, within `prepare_timeout`), and runs the
   post-job checks, including the VM store check.
2. **Ready.** When the host is quiet (no run, no VM, checks clean, not tainted by the agent's own marker) the agent reports
   `exclusive_ready: lease id` in the heartbeat and returns to `serving`. Only then does the scheduler **mint the lease token**; until then
   the lease stays `pending`, so a consumer that knows nothing about readiness (an older client) simply waits (D30). A scheduler-side taint
   recorded at the grant of a lease that allows host jobs (section 8) is not counted against quiet: it is the lease's own.
3. **Not ready in time.** `prepare_timeout` (pool configuration, default 10 minutes) is one deadline counted from the commit of the lease:
   the agent must report `exclusive_ready` or a refusal (item 4) before it. A silent agent (no `preparing` phase, no report, or a host that
   goes lost in the interval) is the same case as one that is slow. When it passes, the scheduler gives the lease up, tells the consumer,
   excludes that host for this request for `reserve_backoff`, and the request is placed again. A finding on the way taints the host as usual
   and ends the reservation on it.
4. **A readiness refusal is typed and acts at once.** The agent (or a readiness hook it runs, item 5) may instead report
   `exclusive_refused` in the heartbeat: the lease id, a **reason code** from the bounded set below, and a **detail** of text, which the receiver cuts to at most 256 bytes (the longest prefix within the bound that ends on a character
   boundary) when it decodes the heartbeat, so a long detail never costs the beat's results, held list or typed refusal; senders should cut too. The detail is recorded with the lease and shown to the consumer; the scheduler never parses it. The scheduler acts on the code
   at once and does not wait for `prepare_timeout`:

   | Code | Meaning | Host | Request |
   |---|---|---|---|
   | `not-quiet` | something the agent cannot clear is running or open | excluded for this request for `reserve_backoff` | re-queued on another host |
   | `noise-over-bound` | the consumer's noise check measured the host over its bound | as `not-quiet` | as `not-quiet` |
   | `setup-failed` | the consumer's or the agent's setup step (for example a cpuset) did not complete | as `not-quiet` | as `not-quiet` |
   | `host-unsafe` | the agent found something that makes the host unfit (a finding, 5.12) | tainted, as any finding | re-queued on another host |
   | `unsupported` | the host cannot run the consumer's readiness step at all | excluded for this request for the rest of its life | re-queued on another host |

   An unknown code (a newer agent) is read as `setup-failed` (R3). The codes are data in the scheduler's table of reasons, not branches in
   its logic: each row is the pair (host effect, request effect) above, so a new code is a new row. Every row, and a timeout, is a
   **prepare failure**: it ends the lease, clears the reservation on the host once the agent has restored it (section 8), and counts one
   toward the request's `max_replace` (default 3). It does **not** count toward `max_reserve_attempts`, which counts only drain give-ups
   (section 4); the two counters are separate and each is kept with the request (section 3). The request is re-queued on another eligible
   host at once if one is available; otherwise it waits until the exclusion ends (the back-off). After `max_replace` prepare failures the
   request fails `exclusive-not-ready`, and its detail names the last code and detail reported.
5. **Environment measurement is the consumer's, and runs inside `preparing`.** Whether the host is *quiet in the scheduler's sense* (no
   lease, no run, no VM, checks clean, not tainted) is all the scheduler's readiness means. Measuring the environment (timer noise, CPU
   frequency, other tenants' traces on a shared machine) is **not** part of it: the scheduler does not read, store or judge any such
   number. A consumer that needs one installs a readiness hook in the agent's configuration (a command the operator allowed, bounded by
   `prepare_timeout`); the agent runs it during `preparing`, after its own checks, and reports `exclusive_ready` only when the hook
   succeeds, or `exclusive_refused` with `noise-over-bound`, `not-quiet` or `setup-failed` when it does not. For kbf this is the noise
   probe and the cpuset setup. The hook is the consumer's code on the host, under the consumer's consents (5.12); the scheduler sees only
   its outcome.
6. **The clock starts at the token (D52).** The lease's TTL and its max wall time are counted from the moment the token is minted, not from
   the commit: the consumer is not charged for the drain or for `prepare_timeout`, and a lease that never becomes ready never held the
   consumer's time. Renewal and the reaper are as for any lease (12.1): a renewal never extends it past the token time plus the max wall
   time.

## 6. Caps: whole-node requests cannot starve a pool

All caps are pool configuration (no reload, D20) and are checked under the table lock. Counting: a host is *whole* when it is reserved or
held. A request waiting unreserved counts toward nothing.

| Key | Meaning | Default |
|---|---|---|
| `max_whole_node` | most hosts of the pool that are whole at once (reserved plus held) | dedicated pool: `max_machines`; mixed pool: none (class not offered) |
| `max_draining` | most hosts reserved and not yet held at once | 1 |
| `max_whole_node_per_consumer` | most hosts one authenticated consumer has whole at once in the pool (K7: the caller's identity, not a request field) | 1 for a mixed pool whose lease API authenticates per-consumer identities; `max_whole_node` (no limit) in a dedicated pool, and wherever the API has no per-consumer identity (the merged single-token service: every client is one consumer). An operator may set 1 explicitly. |
| `keep_for_ordinary` | fewest hosts of a mixed pool that are neither reserved nor held, so ordinary work always has them | 1 for a mixed pool, 0 for a dedicated one |
| `allow_last_capable` | whether a reservation may start on the last unreserved capable host of an ordinary request queued ahead of it | false |
| `drain_after`, `drain_deadline`, `reserve_backoff`, `max_reserve_attempts`, `prepare_timeout`, `max_replace` | times and the retry caps of sections 3 to 5 | see there |

**The effective ceiling** is `min(max_whole_node, capable_hosts - keep_for_ordinary)`, where `capable_hosts` are the pool's hosts that are
capable for its ordinary class (tainted, denied and releasing hosts excluded; lost ones counted). A reservation starts only while whole
hosts are below the ceiling, reserved hosts below `max_draining`, and the consumer below its limit; otherwise the request waits in queue
order. When the ceiling falls (a host is lost for good, a host taints), **a held lease is never revoked**; reservations beyond the ceiling
are dropped newest first at once, and none starts until the count is below the ceiling again. Because held leases stay, marks can for a
time exceed `capable_hosts - keep_for_ordinary`, and the ordinary floor below is then restored only when enough held leases end: after at
most the longest remaining max wall time among them (counted from their tokens).

**The last capable host (D49).** The pool-level count `keep_for_ordinary` keeps hosts for ordinary work in general. A second rule protects
work that depends on one host: a reservation does not start on host H when H is the only unreserved capable host (by facts and constraints)
of some ordinary request that is **queued ahead of this whole-node request**. Only older work is protected: queue order is the pool's
fairness, so a stream of newer ordinary requests that all need H cannot hold the whole-node request off. The pool may switch the rule off
(`allow_last_capable = true`), for instance when the operator accepts that a performance run is worth stalling one ordinary request. In a
dedicated pool there is no ordinary work and neither rule applies.

**Validation at load.** A mixed pool that sets `max_whole_node` needs `max_whole_node + keep_for_ordinary <= max_machines`, and every
limit is at least 1 where it is a maximum; a file that breaks this is refused (D20). A dedicated pool may set `max_whole_node` below
`max_machines`; the surplus hosts then serve nothing, which validation reports.

**Why these and not others.** One cap (`max_whole_node`) alone does not stop one consumer taking every slot; the per-consumer limit does.
A fraction alone does not protect a two-host pool; a count and a floor do. `max_draining` bounds idle capacity during drains (a draining
host does no work), and the ceiling bounds the capacity held by granted leases. No reservation ever starts below the ordinary floor of
`keep_for_ordinary` hosts, and the unreserved remainder of the pool is never touched by this class.

**Starvation bounds.** Ordinary work: in a mixed pool at least `keep_for_ordinary` capable hosts carry no mark at any step, so an ordinary
request that fits such a host is placed as without this class. The one exception is a fall of the ceiling (a host lost or tainted) while
leases are held: the floor is then restored after at most the longest remaining max wall time of those held leases, and meanwhile no
reservation exists beyond the ceiling. A queued ordinary request is delayed by whole-node work only by the
hosts it holds. Whole-node work: a request holding a reservation is committed within `drain_deadline` or gives up; after at most
`max_reserve_attempts` give-ups its reservation has the max wall time as deadline, and a reserved host is empty by then because nothing
new is placed on it. So the request is committed within `max_reserve_attempts` times `drain_deadline`, plus the max wall time, plus
`prepare_timeout` for each of at most `max_replace` tries, for every setting of `drain_deadline`. A request without a reservation waits for a slot: each
slot is held for at most the whole-node lease's own max wall time plus its drain, so its wait is finite and a function of its queue
position, the ceiling and the limits. These are properties of the injected clock, tested on it (R2).

## 7. Elastic providers (D50)

When the pool's hierarchy (D39) has an entry that can launch a machine meeting the request, **a whole-node request acquires a fresh machine
instead of draining a busy one**: draining idles capacity for up to a max wall time, a launch does not, and D39 already prefers a launch to
waiting for a busy host. The order is the two passes of D39: (1) a quiet available eligible host anywhere; (2) an acquire in the first entry
that can satisfy it under its caps (`max_machines`, `max_step`, `cooldown`, `provider_max_machines`); (3) only when no entry can launch
now, a reservation on a busy host. The acquire is recorded with the request id; when the machine registers it is committed to that request
at once, before any other work can be placed on it (reserved at birth). A launch counts as a whole host against the ceiling from the
acquire, and `max_machines` counts it as for any launch. If a busy host becomes quiet while the machine boots, the request takes the host
and the booting machine is kept and released by the idle rule (D40). A machine that served a whole-node lease is checked at its end and
is then an ordinary elastic host, released by `idle_timeout` (5.10). A registered `default` host is never released. A host that carries
many bookings is drained like any other (section 3); a whole-node lease on a spot host can be lost to a reclaim, the loss case of
section 9 ([hosts.md](hosts.md), sections 5 and 6).

**An elastic pool at its machine cap (D62 to D65, [hosts.md](hosts.md) section 11).** When the pool is at `max_machines` and its queue keeps
growing, the scheduler terminates its smallest elastic machine and requeues that machine's jobs. It never picks a host that holds a
whole-node lease or a reservation (the next smallest is taken), and it checks the ceiling of section 6 by host count: it does not remove a
host if that would push the ceiling below the hosts reserved plus held. An eviction is not a reservation and counts toward neither
`max_draining` nor `keep_for_ordinary`. A whole-node request that no live host is big enough for is demand like any other, and the biggest
type launched may serve it (D50).

## 8. During and at the end

**During.** Nothing else is placed. The consumer may run containers or VMs inside its node (each VM fresh for its run and counted in full,
5.14, 5.15), runs its own actions, or hands the node to a runner it starts. The agent accepts a `Start` for the lease from nobody but the
leasee. A lost or silent agent is Unknown, never Absent (5.6): the host stays `held` and is given to nobody until the lease is released or
observed gone.

**End.** On release, TTL, max wall time or loss of the lease, the scheduler retains a `Stop` until the lease leaves the heartbeat (9.1).
The agent removes what remains (containers, scratch, VMs), runs the checks, announces `restoring` and returns to `serving`. **Only then**
does the scheduler clear the `held` mark and make the host bookable; the next reservation or share may take it after that. A host with a
finding is tainted instead and is not bookable (5.12). The host counts against the ceiling until the mark is cleared.

**Host jobs (D51).** A `host` job may change the machine, so it taints it, and the scheduler records the taint at the grant (5.12). The rule
becomes: **a lease may carry `host_jobs` only if it is a whole-node lease**, in addition to the consumer's, the pool's and the agent's
consents. Justification: a host job can read or damage whatever else lives on the host, and a whole-node lease is the one grant under which
nothing else does; a share that allowed host jobs would taint a host that other consumers are using, and their work would be lost to a
taint they did not cause. The cost is that the host is tainted from the grant: on an elastic machine it is replaced, on a registered host
it takes no further work until an operator reimages it (K37), so a GPU or performance pool should leave host jobs off, as the default
does (5.12).

## 9. kbf's use

kbf requests a whole-node lease for two things, both two-level (9.1): it asks the scheduler for the node and then places its own work on
the host with `Start` and `Stop`; the scheduler never sees an action.

- **Reproducible performance runs.** The performance harness needs the host to itself. kbf's consumer configuration maps its class to
  selectors that pick a pool, for instance a dedicated pool of performance machines or a mixed pool that offers the class:

  ```toml
  [[scheduler.class]]
  kbf_class = "whole_machine"        # kbf's name
  class = "whole-node"               # the generic class
  labels = { tier = "perf" }         # picks the pool
  ```

  kbf holds the node and has two ways to use it, and the scheduler is indifferent between them. (a) *kbf places*: its own loop runs the
  harness's actions on the node one at a time, through `kbf-executor`, as it places actions in any lease; this is the default. (b) *A runner
  owns the node*: kbf starts one runner job in the lease with a limit of the whole node and the runner orchestrates the measurement and
  reports to kbf. In both, kbf keeps the lease, renews it, and releases it when the run ends.
- **GPU testing on the Mac Studio machines.** kbf requests `whole-node` with `gpu_class eq apple-m` (or `gpus > 0`), which matches only the
  GPU pool (D32), and runs GPU tests as jobs of the lease, `contained` where the macOS runtime allows it. A host job there would taint a
  registered machine (D51, K37).
- **The ladder.** The rung "the whole host" of an unseen action (R1, 7.4) is a whole-node request, queued with its reservation, after which
  kbf reruns the action inside it. This is the same class; it is not a node lease.
- **kbf's own node leases** are shares and are the work a reservation drains. A kbf that needs a whole node on a host where it holds node
  leases should release them first; the scheduler does not stop them. kbf counts a whole-node lease in its `max_leased_machines` (its own
  cap) and answers `host_reserved` on a node lease by placing nothing new there and releasing when idle.
- **Loss.** A whole-node lease that ends mid-run (host lost, tainted, instance reclaimed, fenced, or the lease ended by TTL or max wall
  time) loses the run: the platform fault is recorded (8.1) and the scheduler tells the consumer the lease is gone. **The consumer repeats
  the measurement**, in a new whole-node lease it requests itself. The scheduler cannot rerun an action (it never sees one, 9.1) and does
  not try; it keeps no partial result. The consumer discards the partial measurement and never uses it.
- **Measurement is the consumer's.** Noise, CPU frequency and other tenants' traces are measured by the consumer, in its readiness hook
  during `preparing` (section 5, item 5) and in its own harness during the run. The scheduler's readiness is only that the host is quiet in
  its table and its checks; it asserts nothing about the environment's fitness for a measurement.

## 10. Wire and configuration, in one place

Additive fields (R3), none new on the agent beyond the merged design: `HeartbeatReply.exclusive` (lease id) and `Heartbeat.exclusive_ready`
(lease id); `Heartbeat.exclusive_refused` (lease id, reason code, detail cut by the receiver to at most 256 bytes; section 5, item 4); a renewal answer's
`host_reserved`; the reservation records in the lease file (section 3). All are additive: an old reader skips and keeps them, and an old
agent never sends a refusal and so only ever times out. Refusal constants: `exclusive-not-ready` (after `max_replace`, with the last code
and detail), with `no-capable-host` and `no-matching-pool` as above. Pool keys are the table of section 6. A previous pool file loads unchanged: `class = "gpu-host"` stays the
retained alias of section 1 (id `gpu-host`, `min_gpus` 1, whole-node admission), and `max_draining_fraction` of earlier drafts is replaced by `max_draining`.

## 11. Tests and invariants (13.1, 13.2, 13.3)

Invariants, checked at every step of seeded runs of the fake agent and the simulation on the injected clock (R2):

- W1. **Never two leases on a whole-node host.** From commit to clearing, a host with a whole-node lease holds no other lease, no placed run
  and no `Start` of another lease.
- W2. A reserved host takes no new lease, no placed run and no `Start` for a lease that did not exist at the reservation.
- W3. **The token is never minted before the agent reports ready**; the TTL and the max wall time count from the token.
- W4. A request holds at most one reservation; a host carries at most one; a held host is not reservable.
- W5. **Caps are never exceeded:** whole hosts at most the effective ceiling (held leases are not revoked when it falls); reserved-not-held at
  most `max_draining`; each consumer within its limit; no reservation starts unless at least `keep_for_ordinary` hosts of a mixed pool
  would stay unmarked. When the ceiling has fallen under held leases, unmarked hosts may be fewer until those leases end (at most the
  longest remaining max wall time); reservations beyond the ceiling are dropped at once, and the test asserts both forms.
- W6. **The drain is bounded:** every reservation ends, by commit, deadline give-up or cancel, by the deadline on the injected clock; a
  give-up touches no work; a request gives up at most `max_reserve_attempts` times, and its next reservation then carries the max wall
  time as deadline and commits.
- W7. **Nothing is killed for a reservation:** the scheduler stops no lease or run because of one.
- W8. Refused only when no registered host could ever be given whole and no elastic provider could launch one (D35); a busy, lost, cordoned
  or reserved host is capable; a tainted or denied host is not.
- W9. The host is bookable only after the agent's `restoring` and clean checks; a tainted host is never bookable.
- W10. **Starvation bounds** (section 6): with a stream of share requests, a queued whole-node request is committed within its bound; with a
  stream of whole-node requests, ordinary requests keep `keep_for_ordinary` hosts and are placed on them; one consumer cannot hold more
  than its limit; a newer ordinary request does not hold off an older whole-node request (the last-capable rule).
- W11. A `host_jobs` lease on a host that is not whole-node is refused.
- W12. With elastic entries, a launch for a whole-node request is committed to it at registration, and a booting machine is released by the
  idle rule when a quiet host took the request first.
- W13. **Reservations survive a restart.** After a restart over the same store, every reservation whose request is presented again within
  `regain_grace` is the same (host, deadline plus boot grace, both counters, exclusions); none is lost, none is made twice, no host
  carries two; an unclaimed record is dropped and its host returns to `serving`; a restart never resets `max_reserve_attempts` or
  `max_replace`, so the bounds of W6 and W14 hold across restarts.
- W14. **A prepare failure is acted on at once and counted once.** A typed refusal ends the lease without waiting for `prepare_timeout`,
  excludes or taints the host as its row says, and counts one toward `max_replace` and none toward `max_reserve_attempts`; a request
  fails `exclusive-not-ready` after `max_replace` of them and not before; an unknown code acts as `setup-failed`; no scheduler decision
  depends on a measured value.

Cases: a drain of a host with three leases ending at different bounds picks the host with the smallest bound, then the fewest leases, then
the lowest id; a give-up at the deadline re-queues on another host and the first is excluded for `reserve_backoff`; an agent that finds a
VM at `preparing` destroys it and reports a finding; a timeout (a silent agent) gives the lease up, excludes the host for
`reserve_backoff` and the retries stop at `max_replace`; a `noise-over-bound` refusal re-queues the request on another host at once,
without waiting for `prepare_timeout`, counts toward `max_replace` and not `max_reserve_attempts`, and the same request after a drain
give-up counts the other way; `host-unsafe` taints and `unsupported` excludes for good; an unknown code acts as `setup-failed`; a
detail over 256 bytes is cut by the receiver to a character boundary and the heartbeat is accepted with its results, held list and code; a restart in the middle of a drain (three leases left on the reserved host, one give-up already counted)
keeps the host reserved with the same deadline once the request is presented again, and a restart loop does not reset the counters; an
unclaimed record is dropped after `regain_grace`; a lease lost mid-run is reported gone and the scheduler issues no rerun; `gpu-host` configs
and lease files from the merged service load as the retained `gpu-host` alias (id, `min_gpus` 1 and error text unchanged) and its tests
pass unchanged, including two hosts taken with the one bearer token because the per-consumer default is no limit there; a GPU request
under class `action` is refused `no-matching-pool`; with an explicit per-consumer limit of 1 on a GPU pool of two hosts and per-consumer
identities, a consumer's second request waits while another consumer's first is granted; a `drain_deadline` shorter than the max wall time
gives up `max_reserve_attempts` times and then commits.

## 12. Extraction steps

Steps 46, 54, 56, 57 and 58 of [extraction-steps-d.md](extraction-steps-d.md): 46 the `Whole` admission, the commit at quiet, the
readiness gate and the end; 56 the reservation and the drain; 57 the caps and the host-job rule; 58 elastic machines for whole-node
requests; 54 moves the GPU machines to registering into a `whole-node` pool.

## 13. Decisions and risks

D46 to D55 and K37 to K41 are in [decisions.md](decisions.md) and [risks-f.md](risks-f.md).
