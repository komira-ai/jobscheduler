# Oversubscription and devices

Companion to [docs/design.md](../design.md), sections 6.3 and 6.4. Section numbers refer to design.md.

## 6.3 Oversubscription as kbf does it
The generic core must not assume memory gates admission. The `Book(Dims)` form carries kbf's rule as data: dimensions not listed in
`Dims` are not compared with free capacity; they are checked once against the pool's total (a request whose memory equals the pool's is
exclusive and runs alone; a request above it does not fit that pool), yet they are still added to the booked total and still count in
`Rank`. Their protection is the agent's job (a hard limit per lease, the pressure backstop, and a rerun on another node on a busy-node kill, which never raises the
booking). Classes that need isolation use `BookAll` (ucdk Job classes, with no reservation); a class that needs the host exclusive and quiet is
whole-node (`Whole`, 5.17).

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

