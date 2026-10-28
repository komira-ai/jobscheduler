# Oversubscription, devices and many jobs on one host

Companion to [docs/design.md](../design.md), sections 6.3, 6.4 and 6.6. Section numbers refer to design.md.

## 6.3 Oversubscription as kbf does it
The generic core must not assume memory gates admission. The `Book(Dims)` form carries kbf's rule as data: dimensions not listed in
`Dims` are not compared with free capacity; they are checked once against the pool's total (a request whose memory equals the pool's is
exclusive and runs alone; a request above it does not fit that pool), yet they are still added to the booked total and still count in
`Rank`. Their protection is the agent's job (a hard limit per lease, the pressure backstop, and a rerun on another node on a busy-node kill, which never raises the
booking). A class that must have a whole free host to itself uses `BookAll` (no reservation; no ucdk Job class does, D56: they book their declared
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

