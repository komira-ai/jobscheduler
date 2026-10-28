# Simulation invariants and the capacity-provider suite (13.2, 13.3)

Moved from design.md sections 13.2 and 13.3 to keep that file under its size limit. Section numbers refer to design.md.

## Capacity-provider suite (13.2)

- `acquire` is idempotent on the acquire id; an Unknown `acquire` followed by a retry does not launch a second instance;
- `release` reads back; releasing an absent instance is fine; a `release` answered Unknown frees nothing; Absent frees a booking once;
- `list` returns only tagged instances, and a `list` error is not an empty list; tags are present on every created instance (the
  follow-up named in the `js-conformance` crate doc); a quota or capacity error maps to `Refused`;
- the pool never exceeds `max_machines` with acquires in flight, Unknown or tainted hosts counted, and a tainted instance is released and
  replaced only after its release reads back Absent; a tainted test-static host is cordoned and a tainted registered id is refused at hello.
- the `default` kind answers `Refused(capacity)` to `acquire`, terminates nothing on `release`, and is never asked to launch; an elastic
  kind's `verify_instance` matches a hello to a pending acquire once and rejects a hello with a wrong, reused or missing proof.

## Admission and hierarchy cases (providers.md)

- A request no host of the pool (and no elastic entry) could ever serve is refused with `no-capable-host`; the same request is queued
  when a capable host exists and is busy, lost, or cordoned; a tainted, denied or draining host does not make a request capable. Each
  case names the host state it uses.
- A pool whose `max_machines` is filled with tainted or denied hosts, with an enabled elastic entry, refuses a request with
  `no-capable-host` (those slots never free); the same pool filled with busy, lost or elastic hosts queues it. An elastic entry that is
  disabled or has a share of 0 does not count as able to launch. A queued request in a pool that reaches the first state starts
  `regain_grace`, and is refused after it, so no accepted request waits for ever.
- A queued request whose last capable host leaves is still queued until `regain_grace` has passed on the injected clock, is served if a
  capable host registers inside it, and is refused with `no-capable-host` after it; a new submission in the same state is refused at once.
- Placement uses a free capable host of the first entry that has one; an entry is asked to acquire only when no entry has a free capable
  host and nothing in flight can serve; an entry at its share of `max_machines`, disabled, cooling down or braked is passed over, and the
  next one is asked; with every entry exhausted the request waits.
- When a host frees while a later entry's machine is booting, the waiting request is placed on the freed host in the same tick; the
  booting machine is not released, registers, and is released only after `idle_timeout` with nothing to do.
- A registered `default` host is never selected for release. No test sets a minimum, and a file that names `min_machines` or `min` is refused.

## New invariants of the simulation (13.3)

An Unknown observation never frees a booking; an Absent observation frees it exactly once; a pool never launches two instances for one
acquire id; a pool never exceeds `max_machines` (in-flight acquires and tainted hosts counted), never acquires more than `max_step` in a
tick, never exceeds a provider's share of it, holds no machine of an elastic entry without demand or an idle period still running (no
minimum); queued work that fits a freed slot is
placed on that host before any acquire; a tainted host takes no lease and is never untainted by a hello, a heartbeat or a Result, even when
the agent reports no taint after a host job (the scheduler tainted it itself); a tainted host id is refused at admission; a pool stops
replacing after `max_taints_in_a_row`; a host holding a lease is never released, including when a lease commits between selection and
release; a host in `releasing` takes no lease or placed job (run service, ucdk), selection and placement committing under the table lock,
first one wins; a host's offer is never above its total less booked at any step, and a VM id serves one run only, however many runs a
lease holds; a whole-node lease's token is never minted before the agent reports ready; a stale or repeated facts revision never changes
what is stored; a pool with a failing boot stops acquiring after `max_boot_failures`; the reaper never releases an instance whose agent
holds leases; every instance in `list` is a pool host or is being reaped.

Added for GPUs (D32): a request that asks for a GPU is never granted a host whose shape has none, and no acquire is made for it.

Added for the whole-node class (5.17): W1 to W12 of [whole-node.md](whole-node.md), section 11: never two leases on a whole-node host; a
reserved host takes no new work; the token never before ready; one reservation per request and per host; the caps (ceiling, draining,
per consumer, the floor for ordinary work) never exceeded and a held lease never revoked; the drain bounded and nothing stopped for it;
a host not bookable until restored and clean; the starvation bounds; host jobs only in a whole-node lease; reservations and the counters
of a request survive a restart (W13); a typed readiness refusal is acted on at once and counted toward `max_replace`, not
`max_reserve_attempts` (W14).

Added for the service (5.16, [deployment.md](deployment.md)): a boot never issues an id from an earlier term; a second table over the
same lease file fails at the lock; a lease held before a restart is held after it unless its host was observed absent; with the boot
grace set, no held lease expires before the grace has passed on the table's clock, and an absent observation still drops a lease at once.

A kbf case: an out-of-memory kill at the top rung of a half-node lease is rerun in a whole-node lease (7.4).
