# Simulation invariants and the capacity-provider suite (13.2, 13.3)

Moved from design.md sections 13.2 and 13.3 to keep that file under its size limit. Section numbers refer to design.md.

## Capacity-provider suite (13.2)

- `acquire` is idempotent on the acquire id; an Unknown `acquire` followed by a retry does not launch a second instance;
- `release` reads back; releasing an absent instance is fine; a `release` answered Unknown frees nothing; Absent frees a booking once;
- `list` returns only tagged instances, and a `list` error is not an empty list; tags are present on every created instance (the
  follow-up named in the `js-conformance` crate doc); a quota or capacity error maps to `Refused`;
- the pool never exceeds `max_machines` with acquires in flight, Unknown or tainted hosts counted, and a tainted instance is released and
  replaced only after its release reads back Absent; a tainted static host is cordoned and a tainted registered id is refused at hello.

## New invariants of the simulation (13.3)

An Unknown observation never frees a booking; an Absent observation frees it exactly once; a pool never launches two instances for one
acquire id; a pool never exceeds `max_machines` (in-flight acquires and tainted hosts counted), never acquires more than `max_step` in a
tick, and keeps `min_machines` hosts whether or not there is demand (never released at or below it); queued work that fits a freed slot is
placed on that host before any acquire; a tainted host takes no lease and is never untainted by a hello, a heartbeat or a Result, even when
the agent reports no taint after a host job (the scheduler tainted it itself); a tainted host id is refused at admission; a pool stops
replacing after `max_taints_in_a_row`; a host holding a lease is never released, including when a lease commits between selection and
release; a host in `releasing` takes no lease or placed job (run service, ucdk), selection and placement committing under the table lock,
first one wins; a host's offer is never above its total less booked at any step, and a VM id serves one run only, however many runs a
lease holds; a whole-node lease's token is never minted before the agent reports ready; a stale or repeated facts revision never changes
what is stored; a pool with a failing boot stops acquiring after `max_boot_failures`; the reaper never releases an instance whose agent
holds leases; every instance in `list` is a pool host or is being reaped.

Added for GPUs (D32): a request that asks for a GPU is never granted a host whose shape has none, and no acquire is made for it.

Added for the service (5.16, [deployment.md](deployment.md)): a boot never issues an id from an earlier term; a second table over the
same lease file fails at the lock; a lease held before a restart is held after it unless its host was observed absent; with the boot
grace set, no held lease expires before the grace has passed on the table's clock, and an absent observation still drops a lease at once.

A kbf case: an out-of-memory kill at the top rung of a half-node lease is rerun in a whole-node lease (7.4).
