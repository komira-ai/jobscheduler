# Oversubscription, devices, many jobs on one host and probes

Companion to [docs/design.md](../design.md), sections 6.3, 6.4, 6.6 and 6.7. Section numbers refer to design.md.

## 6.3 Oversubscription as kbf does it
The generic core must not assume memory gates admission. The `Book(Dims)` form carries kbf's rule as data: dimensions not listed in
`Dims` are not compared with free capacity; they are checked once against the pool's total (a request whose memory equals the pool's is
exclusive and runs alone; a request above it does not fit that pool), yet they are still added to the booked total and still count in
`Rank`. Their protection is the agent's job (a hard limit per lease, the pressure backstop, and a rerun on another node on a busy-node kill, which never raises the
booking). A class that must have a whole free host to itself uses `BookAll` (no reservation; no ucdk Job class will, D56, step 59: they book their declared
size and share a host, 6.6); a class that needs the host exclusive and quiet is whole-node (`Whole`, 5.17).

## 6.4 Devices and exclusive work
`crates/kbf-sched/src/devices.rs` is the model for named assignments: pick the first free device by id that satisfies the attributes;
release on result acceptance or give-up; a re-sent report that drops the device does not release it. The generic
`assignments: Vec<{resource, id}>` generalises this to GPU indices, NUMA nodes and device ids. Two limits stay, with their reasons:

- At most one device lease per agent (`DEVICE_LEASES_PER_WORKER = 1`), because sandbox isolation between device leases is unproven. A
  class can raise this when its agent can prove isolation.
- GPUs are whole-device only. Fractional GPUs are out of scope (D10).
- GPUs exist only on the Mac Studio machines of the GPU pool (D32): the shape declares `gpu_class = "apple-m"` and `gpus = 1`, and a GPU
  request is satisfiable only by that pool ([capabilities.md](capabilities.md), GPUs).

A whole-node request reserves exactly one host once queued, which stops small requests from starving it (5.17, whole-node.md; the reservation
is capped per pool and per consumer). Large non-whole requests have no
reservation today (`docs/design/simulation.md`, scenario F1.5, documents this); R1 forbids adding reservation admission, so the design
adds none and the scenario is kept as a known behaviour.

The rule that replaces the CPU and memory part of 6.3 for actions, from measured and predicted load, is in [placement-by-load.md](placement-by-load.md). It is the default placement for actions, and it plans a node to capacity plus a small tunable CPU slack.

## 6.6 Many jobs on one host: the placement rule over hosts a provider brought

A host is sized to hold many jobs (5.18, [hosts.md](hosts.md)). The rule that packs them is the pipeline of 6.1 and the rank of 6.5, not a
second rule:

- **What is booked.** A host's capacity is the vector its agent offers (`cpu_millis`, `memory_bytes`, `gpus`, `vms`; total less booked). A
  job run books its request vector: a container its CPU and memory under `Book(Dims)` (6.3); a VM its whole CPUs, its memory and one `vms`
  slot, in full, from clone to destroy (5.15); a share lease the vector it names; a whole-node lease every dimension (5.17).
- **Fit.** A capable host fits a request when its offer covers the booking on every gated dimension, a slot is free if the booking needs
  one, and its facts meet the request's constraints, `vms`, `virt` and `lifecycle` among them (5.13).
- **Rank.** In a pool that scales, `FirstFit` (6.5): the first fitting host in hierarchy order, then host order, so work packs onto the
  fewest hosts and the rest go idle and are released (5.8, 5.10).
- **Demand.** Queued bookings that fit no existing host, none about to be freed and none `registering` are placed by the same fit and
  rank onto empty hosts of the pool's shape; the empty hosts used are the demand for new machines, bounded by `max_step` and
  `max_machines`. Demand is hosts, never a count of jobs or leases (5.11).
- **Whole-node.** A whole-node lease books the whole of one host; a host with many bookings is drained to give it (D48), and an elastic
  pool acquires a fresh host first (D50).

## 6.7 A probe pinned to one host

An operator who installs or updates js-agent on a host proves it with one job on that host while the host stays cordoned for ordinary
work. That job is a **probe**: a request that names one host (`probe_host` on `POST /v1/leases`, `js lease acquire --probe-host HOST`;
`Request::pin` in `js-sched`). It runs on that host and nowhere else, and it is the only new work a cordoned host takes.

**The rule** is one function, `js_sched::admit_probe`, and both placement engines call it: the scheduler's round for every queued probe,
over the workers it holds, and the lease server's placer (`--placement js-sched`) for every probe lease, over the facts its registry keeps
of the host. It checks, in order, and the first that fails is the refusal, by name:

| Check | Refusal (`js-sched` / lease API code) |
|---|---|
| the host is known and live | `UnknownHost` / `probe-host-unknown` |
| it is serving (not switching its session, quarantined or tainted, under memory pressure, draining by its own word or scaled away) | `NotServing` / `probe-host-not-serving` |
| it is cordoned | `NotCordoned` / `probe-host-not-cordoned` |
| the cordon is a plain one, not a drain (draining, drained or paused) | `Draining` / `probe-host-draining` |
| it serves the probe's lease kind and satisfies its platform | `NoDriver`, `PlatformMismatch` / `probe-host-mismatch` |
| its whole capacity, after the `memory_schedulable_percent` cut, holds the probe's request | `TooLarge` / `probe-host-too-small` |

A refusal is final for that request, and it is given at once: none of these is a wait for a cordon to end, which is what ordinary work on
a cordoned host waits for.

**Room is not part of the rule.** A probe the rule admits is placed as any request is, on its host alone: the host's free room, the load
predicate (placement-by-load.md) and the CPU slack all apply unchanged, and nothing is booked past them. While its host has no room a
probe never goes to another host. In the scheduler it waits in the queue, with no reason shown, until a lease on the host ends. On the
lease server a start finds room or does not, so a probe on a full host is answered `no-capacity`, the same as any start when no host has
room, and the client asks again. A probe holds no host and is held off none: no reservation or held head is ever on a cordoned host, and
whole-node pools do not count it.

**What a probe changes, and what it does not.** The cordon is not touched: it is the same before the probe, while it runs and after it
ends, and ordinary work stays off the host the whole time. A probe is never joined to a twin, nor a twin to it, since another run would
prove nothing about the host. Ordinary placement is unchanged when no probe is submitted. A request with no pin takes exactly the path it
took before.

**Where a probe can be placed.**

- The lease server with `--placement js-sched` places probes. Its cordon is the host's own (`POST /v1/hosts/{id}:cordon`, which the update
  hook takes), so a cordoned host is one whose cordon has not lapsed. The registry has no drain of that cordon, so the lease server never
  answers `probe-host-draining`. The registry only reports what it knows of the host (its booked shape, cordon, phase, taint and whether it
  is being scaled away), and the placer judges it.
- With the default `--placement registry`, a probe is refused `probe-unsupported`, as it is by the static pool, whose hosts are never
  cordoned. The layered provider passes over a child that answers `probe-unsupported`, so a probe reaches the registry. Every other
  probe refusal is final there: the probe is never placed by a child that did not judge its host. A probe of a whole-node class is
  refused `probe-unsupported` too, because its host would be prepared and held as the whole-node queue decides, and no probe rule judges
  that.
- `js-server-core` queues no probe (`NotQueued::Probe`). Its state-store rows do not keep the pin, so a probe taken up after a restart or
  by another frontend would run as ordinary work anywhere. And the cordon a closed intake puts on its nodes is a plain cordon, which a
  probe would pass.

The lease API has one client token, so it has one trust domain: any holder of the token may ask for a probe, as for any lease. A probe
cannot get past anything a lease could not, apart from the cordon of the host it names.

