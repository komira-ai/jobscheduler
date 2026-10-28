# Placement by measured and predicted load

Companion to [placement.md](placement.md) (6.3, where memory does not gate admission) and [sizing.md](sizing.md). This document replaces the
CPU and memory admission rule for actions; devices, whole-node reservation and the lease kinds are untouched.

## Problem
Admission counts booked CPU: every action books one core whatever threads it runs, so a node takes about as many actions as it has cores
(`Worker::has_room`, `free().fits(resources.without_memory())`), and memory is never summed. Measured mean parallelism per action is about 1.4
to 2.1, because thread pools are sized to the whole node. The node is therefore several times oversubscribed on CPU, and memory is protected only by
the per-lease cgroup limit and the pressure backstop. The goal is nodes at about 100% of CPU and RAM, not 5 to 10 times.

## Decision
There is no pinning, no cpuset and no QoS tier: a container may use any core. What placement needs is data and correct accounting. Admit an
action when the node's measured load plus the predicted cost of what is not yet measured plus the predicted cost of the new lease stays within the
node's capacity, for CPU and for memory separately.

## Signals
Two additive optional fields on `js_agent_wire::Heartbeat` (an old agent omits them; an old server ignores them):

- `busy_cpu_millis`: cores in use, from `cpu.stat` `usage_usec` of the cgroup that holds all actions, as the delta over the window since the
  previous heartbeat divided by the window. Smoothed on the agent by an exponential average with weight 1/2 per window; the heartbeat also
  carries the raw last-window value. Admission uses the larger of the two (a rise is believed at once, a fall only as it persists); ranking
  uses the smoothed one.
- `memory_in_use_bytes`: `memory.current` of the same cgroup.
- `measured_at` window end, in farm time as the server stamps the heartbeat's arrival.

Staleness: a worker whose last signal is older than three heartbeat windows, or that never sent one, is admitted by today's booked rule. The rule
fails open to the old behaviour, never to a refusal.

## Learned cost per action key
- CPU cores: the P90 of `cpu_usage / wall` over the key's recent runs. `js_sizing::Summary::cpu` already does this (32-sample ring, P90 by nearest
  rank, 250 m steps, clamped between 1000 m and the node's CPUs); runs on a node under CPU pressure are not sampled
  (`CPU_PRESSURE_MAX_PERMILLE`), because a starved run understates its need. This design uses that figure as is, including its one-core floor
  (conservative for actions that use less than a core).
- Peak memory: the existing estimator's figure, unchanged.
- State is the existing bounded `Summary` (about 700 bytes per key, 65 536 keys); nothing is added per key.
- Cold start (fewer than `N_MIN` runs, or an unknown key): CPU cost one core; memory cost the first ladder step (4 GiB).
- The memory ladder (4, 16, 32 GiB, whole host) and OOM retries are unchanged. The learned peak informs admission only; the cgroup limit a lease runs
  under stays the ladder's and the key floor's. A retry up the ladder is admitted at the new rung's cost.
- A cost is clamped to the node's capacity, so a key that learns a node-wide pool runs on an otherwise idle node rather than never.

## In-flight accounting
A lease started since the last measurement is not in the measured figure. The server keeps, per worker, each lease's start time (the `Start` it
sent) and its predicted cost. A lease counts its full predicted cost until the first heartbeat window that began at least one window after its start
has been received (the measurement then covers it); in the next window its counted cost falls linearly to zero. While it counts, the measured figure
may include part of it, so the node is briefly over-counted and under-admits for at most two windows; that is the price of making a burst unable to
over-admit. Admission is evaluated per placement round with the in-flight sum updated after each placement, so one round cannot place more than the
headroom either.

## Admission predicate
For an action on a worker with capacity `C` (cores, bytes), with `measured`, `inflight` and the new `cost`:

    measured_cpu + inflight_cpu + cost_cpu <= C_cpu * (1 + cpu_slack)
    measured_mem + inflight_mem + cost_mem <= C_mem * (1 + mem_slack)

Server settings, defaults `cpu_slack = 0.10`, `mem_slack = 0.00`: a small CPU overshoot only slows actions; a memory overshoot kills them, and
the target is "a small overshoot is fine, 10 times is not". The existing lease cap and `request.memory <= capacity.memory` checks stay.
Whole-host and whole-machine leases are exempt from the predicate and keep their rule (the node holds no lease). VM leases keep booking memory.

Starvation: queue order is kept and there are no tiers. The first queued action, in queue order, that fits on no worker holds a reservation on the
worker with the most headroom for it (sticky while it stays the best): later actions are admitted there only if the head's cost still fits after
them. The reservation is one per pool and lasts while the action is queued, the same shape as the whole-node reservation, so a stream of small
actions cannot keep a large one out, and a reservation never holds back work ahead of the head. Everything behind the head is placed on other
workers by the same predicate.

## Ranking among workers
`load.rs` ranks by the largest of CPU share, memory share and lease share. The CPU and memory shares become `(measured + inflight) / capacity`
from the signal when it is fresh, and booked over capacity otherwise; the lease count remains the tie-break. Shares stay exact rationals (no floats) so
simulator replay is deterministic.

## Telemetry
Per worker, each heartbeat: measured CPU and memory against capacity, in-flight predicted cost, admitted predicted cost (sum over leases), and a
run-queue proxy (`cpu.pressure` `some avg10` of the actions cgroup, when the kernel has it). Per key: learned CPU cost, learned peak memory, sample
count. These are logged on the server's existing per-action usage line and as metrics; a farm summary reports the farm-wide measured CPU and memory
over capacity. "About 100%, not 10 times" is shown by the distribution of measured CPU over capacity, by the share of heartbeats above
`1 + cpu_slack`, and by the run-queue proxy staying low.

## Rollout
A server flag `--placement-load off|shadow|on`, `off` by default, the same shape as the learned-sizes flag. In `shadow` today's rule decides and the
server logs, for every placement round, the would-admit and would-block verdicts of the new predicate beside the real one, so the two can be
compared on real traffic before `on`.

## Simulation tests
Logical ticks only (js-sim, js-sched sims); no assertion on wall time. A model action has a thread count, a duty cycle and a duration in ticks.

a. 88-thread node, a mix of wide-pool low-utilization actions and single-threaded long ones: under the new rule measured CPU is at most
   `C * (1 + cpu_slack)` at every tick; under booked-only admission the same workload exceeds it by several times at some tick.
b. The single-threaded long actions start within a bounded number of ticks of reaching the head of the queue, while wide actions keep arriving.
c. Memory: the sum of peaks of running leases is at most memory capacity at every tick; an action killed at its limit retries up the ladder and
   completes.
d. Signals stale or absent: placement is identical to the booked rule (trace equal to the existing golden for the same inputs).
e. Throughput: completed work per tick is not below booked-only admission by more than 5% on scenario (a), and is higher where booked admission
   left cores idle.
f. A burst of equal submissions in one round, before any heartbeat, admits no more than the headroom allows.

## Out
Pinning, cpusets, NUMA placement, QoS tiers, per-action reservations, and any client environment variable. The cgroup limit stays the memory
ladder's; nothing sets a CPU limit.

## Order
kbf first: the daemon's node-level signal (today it reads cgroup usage per lease at the end), the estimator's CPU cost in the server, admission
behind the flag, telemetry, then the simulations. Then the same rule in `js-sched`, with the heartbeat fields in `js-agent-wire`.
