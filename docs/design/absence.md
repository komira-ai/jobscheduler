# Failure and absence semantics

Moved from design.md section 5.6. Section numbers refer to design.md.

## 5.6 Failure and absence semantics
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
  `Present` at the provider is a partition and the agent self-fences. Silence while the instance is `Absent` means the host is gone. For
  a `default` (or test `static`) host silence is Unknown, never Absent (5.5), and a run-on lease is given up only when the host is proven stopped (5.8).
- **A host leaves a pool only after `release` reads back Absent.** An instance gone underneath (reclaim, an operator terminating it) is
  `Absent` on `observe` or missing from `list`, and is treated as host-lost: its machine leases are given up and the consumer reruns its
  placements (8.1).
- **Provider outage.** While a capacity provider is unreachable, its pool can neither grow nor shrink, and existing leases stay. While an
  agent's stream is down, no new leases are placed on its host and its leases keep their resources booked. This is the store-outage rule
  of `crates/kbf-sched/src/scheduler/outage.rs` applied per provider: expiry timers restart with a grace window on recovery.
- **Cordon and drain.** Operator `Cordon` and `Drain` apply to a host and are scheduler state: the scheduler holds them in memory, and a
  core over a state store writes each as the host's `cordon/` row and puts it back at its start (`crates/js-store/src/cordon.rs`). The lease
  server's own-host cordon (`POST /v1/hosts/{id}:cordon`, called by a host for itself) is a lease that lapses by itself; it is kept in the
  binding file (`<state file>.agents`) and survives a restart for as long as it has left. A wait caused
  only by a cordon is never refused as unservable, and a cordoned host is not released on idle. Agent-announced draining is a phase (5.3).
- **Poison and duplicates.** A lease lost `LOSS_REQUEUES` (3) times refuses the operation (4.1); the value becomes a per-class retry policy
  value (8.1). A loss the scheduler caused on purpose, the eviction of a machine at the pool's cap (hosts.md section 11, D63), takes the
  same lost-host path but is recorded `evicted` and does not count toward it; it applies only to work still running when the drain grace
  ended, and those are counted separately, by `max_evictions` (3). A second result for an accepted request is answered DUPLICATE and ignored; a result for a lease that is not the committed
  lease is STALE.
- **A VM that disappears under a run** (the hypervisor process exits, the clone is gone) is a failed run, not an absent lease: the agent
  still heartbeats, the run is reported `failed`, and the consumer reruns it against the platform-fault budget (8.1). A VM is never
  re-adopted after an agent restart (5.14).
