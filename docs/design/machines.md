# Machine reuse, caps and integrity

Companion to [docs/design.md](../design.md), sections 5.4 and 5.8. Section numbers refer to design.md; 5.10 to 5.12 are defined here.

## 5.10 Machine reuse

A machine is the expensive thing and a job is the cheap one, so a machine serves many jobs in a row before it is given up.

**After every job, in this order, `js-agent`:**

1. stops and removes the job's container and removes its scratch directory (the per-job directory under the agent's scratch root);
2. runs the integrity checks of 5.12;
3. only if cleanup and checks passed, frees the slot: the unit leaves the heartbeat set, the booked resources return to the host, and the
   agent emits a capacity change. If cleanup failed or a check found something, the slot is not freed and the host is tainted (5.12).

`Result` is sent as soon as the container has exited and is repeated until acknowledged (5.4); it does not wait for cleanup. The slot is
freed only after cleanup, so the next job never sees the last one's files or processes.

**Placement reacts at once.** A freed slot is an input to the placement loop, not something found at the next poll. Where the scheduler
places requests itself (the run service for ucdk, 11), the same tick that sees the capacity change places the oldest queued request that
fits the freed slot, on this host, before any other pool is asked for a machine. Where a consumer places work inside its lease (kbf, 9.1)
its own loop does the same. In both cases queued work that fits existing capacity is placed before the scaler acquires anything (5.8):
demand that fits is never demand for a new machine.

**A machine is released only after it has been idle.** A host is idle from the moment it has no lease and no running job. After
`idle_timeout` of continuous idleness, while the pool holds more than `min_machines`, the scaler releases it (terminates it, for a cloud
provider; returns it to the inventory, for a static one). A job or lease arriving before then restarts nothing: the host simply serves
it. Reuse across consumers is allowed because the checks of 5.12 run after every job and the scratch is gone; a host that cannot show that
is tainted instead.

**Reconciled timeouts.** `idle_timeout` is the one release delay; it is the delay of the former release rule and of the reaper's idle
release (12.1), not a second clock. `cooldown` is the one re-acquire delay: it follows a `Refused`, a boot failure and a release, and
nothing else. `boot_timeout` bounds a launch that never says hello. `max_step` (5.11) bounds growth per tick. No other timer scales a
pool.

## 5.11 Caps: overload protection, not cost guardrails

Every pool has `max_machines`, a count the provider never exceeds. The count includes every machine the pool owns in any state:
serving, `registering`, in-flight `acquire` (an intent recorded but not yet answered, or answered Unknown), `releasing` and tainted. The
check is made under the table lock when the intent is recorded, so two scaler ticks cannot both pass it. A restart counts what `list`
returns plus the open intents. A pool at `max_machines` leaves the demand queued (8.2).

`min_machines` is the floor the scaler keeps while the pool has demand, and defaults to 0.

**Growth is bounded and follows real demand.** Each tick the scaler acquires at most `max_step` machines (default 2). It acquires only for
queued demand that does not fit existing capacity: leases in `acquiring` that no free host, no host about to be freed and no
`registering` instance can serve, counted in machines' worth of the pool's shape after packing, not in requests. A pool never grows to
"see what happens", and a burst of 500 requests that fit two idle machines acquires nothing.

**Why: overload protection.** The caps protect the scheduler, the provider's API and the network from a runaway loop or a flood of
requests, and keep a small mistake small. They are not cost guardrails: jobscheduler adds no budget, spend ceiling or tenant quota, and
cloud quotas remain the platform's (N4). A cap that is hit is visible as queued demand and an alert, not as an error.

**10,000 targets never means 10,000 machines.** A consumer that leases capacity places its work inside it (9.1): kbf packs actions into
the machine leases it holds, so the scheduler sees demand for machines' worth of resources and never an action. The number of machines
follows the sum of the resources the queued work needs, not the number of actions, and is cut off by `max_machines`.

**A consumer may cap itself.** A consumer's own configuration may set `max_leased_machines`, the most machine leases it holds at once
(kbf: in its scheduler section, see [configuration.md](configuration.md)); beyond it, the consumer queues its own work. The scheduler
neither reads nor enforces it; it is the consumer's own overload protection and is tested on the consumer's side.

## 5.12 Integrity: contained and host jobs

**Isolation.** `StartSpec` (5.2) carries `isolation`, either `contained` (the default: a container with the limits and mounts of the
message) or `host` (the job may change the machine: install packages, load a module, write outside the agent's roots). An unspecified
value is `contained`. A new value an old agent does not know is refused by it, never run (R3: the field is additive; an old agent
refuses a `host` it cannot parse and the lease is given up as never started).

**Who may request a host job.** Three consents are needed, and any one missing refuses the `Start` before a container exists:

- the consumer's own configuration allows it (kbf: `allow_host_jobs`, default false) and its lease request states `host_jobs = true`;
- the pool allows it (`allow_host_jobs`, default false; configuration.md), checked when the lease is granted; the lease token minted for
  that lease then carries a `host_jobs` claim, and only a pool that allows it ever mints one;
- the agent's own local configuration allows it (`allow_host_jobs`, default false), checked on `Start`.

The default is contained only. A pool that allows host jobs should be a pool of throwaway machines, since every host job costs its machine.

**After a host job the machine is tainted.** On completion (any outcome: success, failure, kill, timeout), the agent marks the host
tainted before it frees anything.

**Post-job checks.** After every job, contained or host, the agent runs cheap checks against a baseline it recorded when the host was
last known clean (at agent start, and after each job that passed). This list is a minimum; a check may be added later (additively, R3),
and none is removed without a decision:

- *stray processes*: any process started during the job that is outside the job's cgroup or still alive after the container is gone
  (including a daemonised child), and any process running as the job's user after the job ends;
- *host changes*: a file created or changed outside the allowed roots (the agent's scratch, cache and output roots), compared by a bounded
  listing and digest of a configured watch list of directories; the set of mounts (`/proc/self/mountinfo`) against the baseline; the user
  and group database against the baseline; the loaded kernel modules (`/proc/modules`) against the baseline;
- *cleanup failure*: the container or its scratch directory could not be removed, a cgroup could not be emptied, or the slot could not be
  freed.

Any finding taints the host, whatever the job declared. A check that cannot run (an unreadable file) is a finding; it is never skipped.

**Taint is sticky and reported by the agent.** The agent writes a taint marker under its state root before it reports, so a restart does
not clear it, and it sends the taint in its `Heartbeat` (reason code, the lease id of the job, the check that found it) until the
scheduler acknowledges it. The scheduler records the taint durably against the host id (an additive field of the lease file, R3) and
announces the phase `tainted` (5.3). A tainted host takes no new lease or job. Its running leases finish or are drained: the consumer is
told, drains and releases (9.1). **The scheduler never clears a taint on the agent's word:** a later hello or heartbeat without the
taint changes nothing; only the provider path below, or an operator, ends it.

**Replacement, by provider kind.**

| Kind | Tainted host |
|---|---|
| `aws` and later launching kinds | Once its leases are gone, `release` it (terminate, read back Absent), then acquire a replacement under the usual caps. A tainted machine counts toward `max_machines` until it reads back Absent, so at the cap the terminate comes first. The instance id is never reused. |
| `static`, listed | Cordoned. It stays listed and takes no work until an operator reimages it and runs `js-cli host clear-taint`, which is an audited operator action. |
| `static`, self-registered | The host id enters the tainted set and any hello from it is refused with a constant code, even with a valid join credential, until an operator reimages the machine and clears the id. Clearing also drops the binding of the id to its first credential (5.8). |

A tainted host id cannot rejoin: it is checked at admission before the credential. The tainted set survives a scheduler restart (it is in
the lease file), and an operator's clear is the only way out.

**Replacement is bounded.** A replacement is an acquire like any other: it follows `max_step`, `cooldown` and `max_machines`, and a pool
whose hosts keep tainting (a bad job class) raises an alert after `max_taints_in_a_row` replacements without a clean job between them,
and stops replacing until the owner clears it. This is the same failure brake as `max_boot_failures`, not a cost limit.
