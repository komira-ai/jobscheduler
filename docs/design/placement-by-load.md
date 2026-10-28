# Placement by measured and predicted load

Companion to [placement.md](placement.md) (6.3, where memory does not gate admission) and [sizing.md](sizing.md). This document replaces the
CPU and memory admission rule for actions; devices, whole-node reservation and the lease kinds are untouched.

## Problem
Admission counts booked CPU: every action books one core whatever threads it runs, so a node takes about as many actions as it has cores
(`Worker::has_room`, `free().fits(resources.without_memory())`), and memory is never summed. Measured mean parallelism per action is 1.4 to 2.1
cores. The 5 to 10 times is the run queue, not the cores per action: about 25 concurrent leases, each in a thread pool of about 89 threads, put
5 to 10 times as many runnable threads as cores on a node. Memory is protected only by the per-lease cgroup limit and the pressure backstop. The goal
is nodes at about 100% of CPU and RAM, with an overshoot of a small factor and never 10 times.

## Decision
There is no pinning, no cpuset and no QoS tier: a container may use any core. Admit an action when the node's measured load, plus the predicted
cost of what the measurement does not yet cover, plus the predicted cost of the new lease, stays within the node's capacity, for CPU and memory
separately. Per-node measurements come from cgroups each heartbeat; per-action-key costs are learned; the memory kill-and-retry ladder is unchanged.

## What goes away in kbf
| Today | Under this design |
|---|---|
| `actions/cpu.max` written by `limit_cpus` (kbf-node `lib.rs`, kbf-driver-container `delegate.rs`) | Not written for a pool that has placement-by-load `on`. A bandwidth limit hides overload (measured CPU can never exceed it), so the proof metric would read zero. Kept in `off` and `shadow`; then measured CPU is capped at the limit and `throttled_usec` is the overload signal. |
| `--lease-placement=numa-node` (`numa.rs`, cpuset on lease cgroups) | Removed. Containers may use any core. |
| `--reserve-cpus` as the `cpu.max` quota | Kept as a **capacity reduction only**: `C_cpu = hardware threads - reserve`. Chosen over measuring the non-action usage because the daemon and storage processes live outside the actions cgroup, in a layout only the operator knows; a static number cannot be made wrong by a failed read. The non-action usage (root `cpu.stat` minus actions) is exported so the reserve can be tuned. The memory side is unchanged: `C_mem` is the Hello memory after protected floors. |
| per-lease `cpu.weight` from the booking | Unchanged (a proportional share under contention, not a limit or a tier). |

## Signals
Additive and optional; an old agent omits them, an old server ignores them.

| Field | Meaning |
|---|---|
| `cpu_usage_usec` | cumulative `usage_usec` of the actions cgroup (`cpu.stat`); descendants removed since are still included |
| `mono_usec` | the agent's monotonic clock at the same read; the only time base used for windows (the server's arrival time is used only for freshness) |
| `memory_working_set_bytes` | `memory.current` minus `inactive_file` from `memory.stat` of the actions cgroup (floored at 0). Chosen over anon plus kernel because it keeps shmem and tmpfs outputs, which are not reclaimable; it still counts hot (active) file pages, which errs toward under-admitting, never toward a kill. Cold page cache, the bulk of a busy node's cache, is excluded. |
| `covered` | the leases that were running for the whole window since the previous reading (by the agent's own start, so a start held back by the pressure backstop or an image pull is not covered) |
| `cpu_throttled_usec`, `cpu_pressure_some_avg10` | telemetry only; never feed admission |

Server side, per worker: a measurement is accepted only for `seq` = last `seq` + 1 within one daemon process (`mono_usec` did not go back); else it
is dropped and the worker is in fallback until the next heartbeat. `measured_cpu` = `delta usage / delta mono` in millicores, integer; admission and
ranking use the larger of the last two windows (a rise is believed at once, a fall after two windows). Values that fail `validate` (implied cores above
4x the hardware threads, memory above 4x capacity, counter going back) count as absent. All arithmetic is integer, saturating or `u128`. A signal older
than three windows is stale. `W` is the heartbeat interval the welcome sets (5 s by default).

## Learned cost per action key
Both costs are the existing `js-sizing` estimate (`Estimate::cpu_millis`, `memory_bytes`), with these changes:

- **CPU:** `round_up_250m(P90(cpu_usage / wall))` over the 32-sample ring, whole-run means, clamped to `[floor, node threads]`. Today a cold key
  (no usable summary: `Fallback::Cold`, `Missing`, `Stale`) has no estimate and the caller books a default; with 1 to 4 samples the figure is the P90 of
  the samples that exist (`Summary::cpu` does not check `N_MIN`).
- **Cold cost:** CPU `cold_cpu`, default 2 cores (measured mean parallelism is 1.4 to 2.1); memory 4 GiB (`Bounds::default_memory`, the first rung).
- **Memory** is `1.2 x max + 128 MiB` rounded to 256 MiB (not a peak; the estimator tabulates its own misses), at least 1 GiB. A retry up the ladder is
  admitted at `max(learned, the rung it was killed at)`, so a spiky key killed at 4 GiB is not counted at its learned 2 GiB again.
- Cost is clamped to capacity, so a node-wide key runs on an otherwise idle node.
- **Sub-core costs.** `CPU_MIN_MILLIS` goes from 1000 to 250 (one 250 m step). A 0.3-core action then costs 0.5 core, not 1, and a node can hold
  `1.1 C / 0.5` such leases (at most the lease cap and the memory term). Keeping 1000 would make short light actions, which finish inside the
  in-flight phase, cost exactly what they cost today.
- **Heterogeneous nodes.** Each sample also records the thread count of the node it ran on (one `u16` per ring slot, 64 bytes per key). A sample that
  used at least half its node is a pool-sized action and is scaled by `target threads / sample threads`; a narrower sample is not scaled. The cost is
  the P90 of the scaled samples, clamped to the target. `C_cpu` is hardware threads, so "100%" means threads busy; no SMT normalisation.
- **Learning under load.** Today a CPU sample is kept only below 100 permille node CPU pressure, which a node at 100% load rarely meets, and an unknown
  pressure (Mac, no PSI) drops it. New rule: a sample taken at or above that pressure, or with none, is kept **only if it exceeds the key's current CPU
  estimate**. A starved run understates its demand, so a kept value is never above the truth: the estimate can only rise under load and falls on the
  quiet runs and the existing staleness. Counted per key and farm-wide: samples kept, dropped under pressure, and keys with no CPU sample.

## Admission
For a worker with capacity `C` (CPU threads minus reserve; memory after floors), each lease `i` has the cost `(p_i, m_i)` fixed at admission and held
until its result (any outcome: end, kill, requeue). `U` is the set of admitted leases not in the latest accepted `covered` (uncovered), at most about
`2W` after the agent starts them. VM leases are outside the actions cgroup: their booked CPU and memory, `V`, are added to every sum.

    CPU:     measured_cpu + sum(p_i, uncovered) + V_cpu + p_new  <=  C_cpu * (1 + s_cpu)
    memory:  max( sum(m_i, all), working_set + sum(m_i, uncovered) ) + V_mem + m_new  <=  C_mem * (1 + s_mem)

Defaults `s_cpu = 0.10`, `s_mem = 0.00`. Memory is the maximum of the measured working set and the sum of the predicted costs of every running lease,
held to the lease's end; **predicted memory never decays**, so a lease that allocates late is still counted at its prediction. CPU does decay to the
measurement (covered leases are in `measured_cpu`), because CPU is compressible and memory is not. The sums are updated after each placement, so one
round cannot place more than the headroom. `request.memory <= capacity.memory` and the lease cap stay. Whole-host, whole-machine and exclusive leases keep
their rules.

**Fallback.** A worker with a stale, absent, dropped or implausible signal, no cgroup (Macs with `MacFootprint`, native and VM nodes, cgroup v1), or an old
daemon, is admitted by the same predicate with `measured_cpu` replaced by `sum(p_i, all)` and `working_set` omitted: the sum of the predicted costs of
its leases, no decay. It never falls back to booked-only. Such a worker is as safe as its predictions are good, and the cold cap below limits the rest.

## The overshoot bound
Let `M` be the CPU of covered leases at the last reading, `U_w` and `U_c` the predicted cost of uncovered warm and cold leases, so `M + U_w + U_c <=
(1+s_cpu) C`. Let `k` be actual over predicted CPU of a warm lease over a window (the mean is learned, within-run peaks are not, so `k` includes
phase changes), `r` how much a covered lease's CPU can rise over its measured use in one window, `A` the CPU a cold lease can use, and `n_c` the cold
leases uncovered at once. Then the demand in any window after admission is

    P_cpu  <=  r*M + k*U_w + n_c*A   <=   max(r, k) * (1 + s_cpu) * C  +  n_c * A

Cold cap: at most `cold_per_window` leases with a cold CPU cost are admitted per worker per window, default `ceil(C_cpu / 32)` (setting
`--placement-cold-per-window`); a lease stays uncovered for at most `2W`, so `n_c = 2 * cold_per_window`.

Defaults, with the shadow exit criteria below (`k <= 2`, `r <= 2` at p99, `A <= 4` cores at p99): on 88 threads `n_c = 6`, so
`P_cpu <= 2 * 1.1 * 88 + 6 * 4 = 218 cores = 2.5 C`; on 16 threads `n_c = 2`, `P_cpu <= 35 + 8 = 43 = 2.7 C`. Not 10 times, and
transient: the next heartbeat shows the rise, admission stops, and a key with understated cost relearns by the rule above.
Memory: a lease cannot exceed its cgroup limit `1.5 x booking + 512 MiB`, so a cold lease (4 GiB) has `k_m <= 1.63` and
`P_mem <= (1 + s_mem) * C_mem * max(1, k_m)`; a warm key's miss is the estimator's tail (target `k_m <= 1.3` at p99). The per-lease limit and the node
backstop kill beyond that, as today. What bounds `k` is not the mechanism but its measured value, hence the histograms below and the exit criteria.
Persisting above `(1 + s_cpu) C` needs leases whose real use exceeds their prediction, and the learning rule raises those predictions.

## Starvation
No tiers; queue order is kept. A **pool** is the set of workers a request can run on (platform, drivers, devices, avoid set), as `Servable` computes it.
The **head** is the first queued action, whole-machine, exclusive and VM requests excluded (they keep the existing whole-node reservation, and a worker
that holds one is never chosen), that has no worker with room. It reserves the worker in its pool with the smallest binding ratio
`max((L_cpu + p) / C_cpu, (L_mem + m) / C_mem)` (`L` the left side of the predicate without the new cost), sticky while that ratio is within 0.1 of the
best. A reserved worker admits no later request (a request that fits elsewhere goes elsewhere; one that fits only there waits, as for a whole-node
reservation). One reservation per pool: a second blocked head waits until the first starts, then becomes the head. A reservation on a worker whose
signal goes stale stays and uses the fallback figures. Bound: the head starts within the longest remaining lease at reservation, plus `2W`, plus one
round. Work ahead of the head is never held back.

## Lying or stuck node, restart
- **Plausibility.** The server accumulates, per worker, the `cpu_usage_us` of the leases whose results it has received, and compares it with the
  growth of `cpu_usage_usec` over the same span: growth below half the lease sum (once that is at least 60 core-seconds), or a node with no lease
  reporting more than half of `C_cpu` for three windows, marks the signal implausible and the worker uses the fallback until it agrees again. This
  catches a wrong cgroup, a stuck counter and a broken reader; an agent that lies in both places is out of scope.
- **Scheduler restart or new leader.** Recovered leases count their full cost (recomputed from the current estimator) as uncovered; the first heartbeat
  after re-registration has no previous reading, so the worker is in fallback for it and measured from the second (about `2W`). Never booked-only.
- **Lease end** removes its cost at the result; a short action therefore costs its prediction for its whole life, which is why the sub-core floor matters.

## Plumbing
| Piece | Where |
|---|---|
| Read `cpu.stat`, `memory.current`, `memory.stat`, `cpu.pressure` of the actions cgroup | new `node_load.rs` beside `cgroup_usage.rs` in kbf-driver-container, over the existing `CgroupFs` trait: `read_actions_load(mount, actions)` |
| Hand it to the daemon | new defaulted method on the kbf-daemon `Runtime` trait, `node_load(&self) -> Option<NodeLoad>`; native, VM and Mac runtimes keep the default `None` |
| Put it in the heartbeat | kbf-daemon `daemon.rs`, where each `Heartbeat` is built: reads `node_load` and the lease table (for `covered`) once per interval |
| kbf protobuf | `Heartbeat.optional NodeLoad node_load = 6`; `message NodeLoad { optional uint64 cpu_usage_usec = 1; optional uint64 mono_usec = 2; optional uint64 memory_working_set_bytes = 3; optional uint64 cpu_throttled_usec = 4; optional uint32 cpu_pressure_some_avg10_centi = 5; repeated LeaseId covered = 6; }`, plus `optional uint32 cpu_peak_window_millis` on `CgroupUsage` (next free tag). Additive; a fixtures proto and goldens move in the same change. |
| js wire | `js_agent_wire::Heartbeat.load: Option<NodeLoad>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, the shape of `taint`), with `cpu_usage_usec`, `mono_usec`, `memory_working_set_bytes`, `covered`; `validate` bounds as above |
| js scheduler | `js_sched::Event::Heartbeat` gains `load: Option<NodeLoad>`; the predicate and shares live in one pure module (`load.rs`) with shared test vectors, because kbf-sched is a copy |
| Cost | `js-sizing`: floor 250 m, scaled samples, the learning rule, `Observed` says why a sample was dropped |
| Flags | `--placement-load off\|shadow\|on` and `--placement-load-pools` (pools that are `on`; the rest stay at their mode) on kbf-server and as a `Scheduler` setter plus `js-lease-server::Config` field; `--placement-cold-per-window`, `--placement-s-cpu`, `--placement-s-mem`. `shadow` or `on` with `--learned-sizes=off` is refused at start-up, since the CPU estimate exists only with learned sizes. |

## Ranking
`load.rs` ranks by the largest of CPU share, memory share and lease share, now `L_cpu / C_cpu` and `D_mem / C_mem` from the predicate's own left sides
(fresh or fallback), exact rationals of integers, the lease count the tie-break. The pressure and throttle figures are telemetry only.

## Telemetry
Per worker each heartbeat: measured CPU and memory over capacity, uncovered cost, sum of predicted cost, throttled time, run-queue proxy
(`cpu.pressure`), non-action CPU, plausibility state. Per finished lease, **in `shadow` and `on`**: histograms of `k` for CPU (`mean cores / p_i`),
of `r` (`peak window / mean`, from the daemon's per-heartbeat read of the lease cgroup) and of `k_m` (`memory.peak / m_i`). Per key: cost, sample count;
farm-wide: cold and unsampled keys, cold leases admitted. The summary reports the distribution of measured CPU over capacity, the share of windows
above `1 + s_cpu`, and the run queue.

## Rollout
`off` (default); `shadow` (today's rule decides; the server logs the new predicate's verdict beside the real one, and the histograms); `on`, per pool,
as the canary. Because shadow keeps the old rule it cannot show measured load under the new one; it shows `k`, `r`, `k_m`, signal freshness and the
verdict differences.

| Step | Numeric exit criteria |
|---|---|
| shadow to canary pool | signal fresh on at least 99% of heartbeats of container workers; p99 `k <= 2`, p99 `r <= 2`, p99 cold-run mean cores `<= 4`, p99 warm `k_m <= 1.3`; keys without a CPU sample at most 5% of started leases |
| canary to wide | over a canary period of at least 7 days of traffic: p99 measured CPU / `C` at most 1.25 and never above the bound; ladder retries and node-pressure kills not above the control pools; queue wait p95 not more than 10% above control; mean measured CPU / `C` above control |

## Simulation tests
Logical ticks only (js-sim, js-sched sims), `W` expressed in ticks; no wall-clock assertion. Bounds are checked against the model's ground-truth
demand, not the scheduler's own figures. Each must go red without the mechanism named.

| Sim | Scenario and assertion | Red without |
|---|---|---|
| a | 88-thread node, wide-pool low-duty actions, single-threaded long ones, plus a light short population (0.3 core, under 2 windows); with a contention model (slowdown = demand / cores): demand `<= (1 + s_cpu) C` for constant-duty actions | measured term (booked rule), or the 250 m floor (light population stays at today's concurrency) |
| b | A stream of wide actions and a node-wide head: head starts within the stated tick bound; a head blocked in pool A does not delay pool B; two heads in one pool: second waits for the first | the reservation |
| c | Memory ramps late (allocates at 60% of the run) and mispredicts (3% tail runs); page cache fills 80% of RAM: the sum of ground-truth memory `<= (1 + s_mem) C max(1, k_m)`, and the cache does not block admission | memory held to lease end; the working-set basis |
| d | Stale, absent, cgroup-less (Mac-like), partial-field, one stale among fresh, going stale and returning: predicted-sum admission, demand within the bound; off and shadow traces equal the existing golden | the predicted-sum fallback |
| e | Throughput and p99 wait against booked-only with the contention model: not worse by more than 5% on (a), at least 10% higher on the light population | contention model absent (claim cannot fail); floor |
| f | A burst of equal submissions in one round, then across two rounds, over several workers, before any heartbeat: admitted `<=` headroom, spread by rank | per-placement update of the sums |
| g | Cold burst: 40 unlearned keys at once: uncovered cold leases per worker `<= 2 * cold_per_window`, demand within `P_cpu` | the cold cap |
| h | Phase change (idle for 4 windows, then 16 cores): demand within the bound, and no placement after the heartbeat that shows the rise | believing a rise at once |
| i | A key understated 4x: demand `<= 4.4 C` at first; after the learned samples (taken under pressure) the cost is within 1.25 x true and demand `<= 1.1 C` | the learning-under-load rule |
| j | Scheduler restart with leases in flight: no placement beyond the bound across the restart; recovered leases are counted | counting recovered leases |
| k | Stuck-low signal (reports 0 with leases running): flagged within the stated number of windows, then predicted-sum, demand within the bound | the plausibility check |
| l | Mixed 16- and 88-thread nodes, a pool-sized key learned on one and placed on the other: demand within the bound on both | the thread-count scaling |
| m | Shadow: placements identical to `off`; logged verdicts equal the first-round verdicts of an `on` run; histograms count every lease | shadow isolation |
| n | Ladder retry: killed at 4 GiB, retried at 16 GiB, completes; the retry is counted at the rung; the learned estimate rises without thrash | counting the retry at the rung |

## Out
Pinning, cpusets, NUMA placement, QoS tiers, per-action reservations, a CPU limit on the actions cgroup (for `on` pools), and any client environment
variable. The cgroup limit of a lease stays the memory ladder's.

## Order
kbf first: `node_load.rs` and the `Runtime` method, the proto field and the daemon heartbeat, the `js-sizing` changes, the predicate behind the flag in
`shadow`, telemetry, the simulations; then a canary pool with `cpu.max` and `numa-node` removed. Then the same predicate in `js-sched`, with the
heartbeat fields in `js-agent-wire`.
