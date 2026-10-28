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

The flat rule (Signals, Learned cost, Admission, the overshoot bound, Starvation) is self-contained and complete without profiles. The Time profiles section is a layer on top of it:
it changes how a lease's cost is expressed (a curve instead of a number) and how the predicate is evaluated (at each future minute), and every case it does not cover falls back to the flat
rule. Read the document in that order.

## Which placer this changes
**js-sched is the implementation.** The predicate, the cost record, the sums, the ranking and the starvation reservation are written once, in `js-sched` (`Worker::has_room`, which today
checks `free().fits(resources.without_memory())`, memory only against capacity, and the lease cap), as a pure module `load.rs` with shared test vectors. kbf-sched takes the same module through
the existing byte-identical copy rule; nothing is implemented twice. The kbf work is plumbing only: the node reader (`node_load.rs`), the `NodeLoad` heartbeat field, the
`Runtime` method and the flags.

What stays as it is, because it is a different admission scope and neither reads a measurement:
- `js-registry` `Runs::admit` (the `/runs` interim API) sums the declared `limits.cpu_millis` and `memory_bytes` of a lease's live runs against the lease shape. It is a per-lease budget *inside* an already
  placed lease, not a host placer; it is unchanged by `--placement-load`.
- `js-pool-static` books a whole host to one lease. It is unchanged.
One host belongs to exactly one placer, so the two cannot disagree about it: a host the lease-table path books is never evaluated by the load predicate.

**Relation to `--placement`.** `--placement-load` acts only under `--placement sched`. With `--placement lease-table` it is refused at start-up (not ignored), and the flag golden of the lease-server lists it.

## The cost record in js-sched
Each admitted lease carries, in `Worker`'s per-lease map, a `LeaseCost { p_millis, m_bytes, limit_bytes, rung, curves, started_mono }`: the predicted CPU cost, the predicted memory cost, the cgroup limit and the ladder rung as
separate fields (the request's `Resources.memory_bytes` stays the cgroup limit, as `has_room` already treats it, so nothing that reads it changes), the profile curves used at admission (at most 64 buckets of two
`u32`, 512 bytes; empty for a flat lease), and the agent-reported start that defines `covered`. The covered and uncovered sets are derived from `NodeLoad.covered` and `started_mono` and are not stored. `Event::Heartbeat` gains
`load: Option<NodeLoad>`. **The cost is persisted with the lease** (in the lease record that replay reads) and is the value used after a restart: a recovered lease is never re-estimated from the current estimator, so the sums
before and after a restart agree. This is data in the pure crate; the estimator is never read inside the scheduler's decision, only at admission by the caller that produces the cost.

## What goes away in kbf
| Today | Under this design |
|---|---|
| `actions/cpu.max` written by `limit_cpus` (kbf-node `lib.rs`, kbf-driver-container `delegate.rs`) | Not written when placement-by-load is `on` (the default). A bandwidth limit hides overload (measured CPU can never exceed it), so the proof metric would read zero. Kept only for a pool held at `off` or `shadow` by the operator; then measured CPU is capped at the limit and `throttled_usec` is the overload signal. The pool mode lives on the server but kbf-node writes the file at start-up, so the two are kept consistent by a node flag and a check (see Plumbing): `--actions-cpu-max=on\|off` (default `off`, no limit, matching the server default), and the heartbeat reports whether a limit is set. |
| `--lease-placement=numa-node` (`numa.rs`, cpuset on lease cgroups) | Removed. Containers may use any core. |
| `--reserve-cpus` as the `cpu.max` quota | Kept as a **capacity reduction only**: `C_cpu = hardware threads - reserve`. Chosen over measuring the non-action usage because the daemon and storage processes live outside the actions cgroup, in a layout only the operator knows; a static number cannot be made wrong by a failed read. The non-action usage (root `cpu.stat` minus actions) is exported so the reserve can be tuned. The memory side is unchanged: `C_mem` is the Hello memory after protected floors. |
| per-lease `cpu.weight` from the booking | Unchanged (a proportional share under contention, not a limit or a tier). |
| protection of the daemon and storage processes, which `cpu.max` gave indirectly (the actions cgroup could never take the whole node) | Replaced by weight, set where the siblings actually are. kbf-driver-container moves the daemon into a `supervisor/` cgroup with `actions/` beside it, both under the unit's delegated cgroup; a unit `CPUWeight` is relative to other units and weighting the parent that holds both changes nothing between them. So the daemon writes `supervisor/cpu.weight` = 1000 next to its existing `supervisor/memory.min` write, and `actions/` stays at 100: under contention the supervisor gets up to ten times the share of the whole actions cgroup, so a heartbeat is not starved and the lease self-fence does not trip from CPU starvation. The storage service is a separate unit, a sibling of the node unit, so its unit `CPUWeight=1000` does work. Idle, both use almost nothing, so the weight costs the actions nothing. The value is not chosen but measured: the daemon's `cpu.pressure` and self-fence trips are observation targets. |

## Signals
Additive and optional; an old agent omits them, an old server ignores them.

| Field | Meaning |
|---|---|
| `cpu_usage_usec` | cumulative `usage_usec` of the actions cgroup (`cpu.stat`); descendants removed since are still included |
| `mono_usec` | the agent's monotonic clock at the same read; the only time base used for windows (the scheduler's injected `FarmTime` is used only for freshness; production never reads wall time, as the pure scheduler does not) |
| `memory_working_set_bytes`, `memory_active_file_bytes` | `memory.current` minus `inactive_file` from `memory.stat` of the actions cgroup (floored at 0), and `active_file`. The server uses `ws = working_set - max(0, active_file - a * C_mem)`: the file-page part is capped at the share `a` of the node's memory (default 0.25, `--placement-ws-file-share`), so retained hot page cache can never make an idle node read as full. Without the cap an idle node whose leases have ended keeps their pages active until reclaim, which only happens under memory pressure, so `working_set >= C_mem` would refuse every action and no pressure would ever reclaim. At most `a * C_mem` of cache counts; anon, shmem and kernel memory always count in full. Chosen over anon plus kernel because it keeps shmem and tmpfs outputs, which are not reclaimable; it still counts hot (active) file pages, which errs toward under-admitting, never toward a kill. Cold page cache, the bulk of a busy node's cache, is excluded. |
| `covered` | the leases that were running for the whole window since the previous reading (by the agent's own start, so a start held back by the pressure backstop or an image pull is not covered) |
| `actions_cpu_limited` | whether the actions cgroup has a `cpu.max` other than `max`, read from the cgroup; a worker in an `on` pool that reports `true` is admitted by the fallback (below) and counted, so a node that was not restarted with the flag is safe, not silently wrong |
| `cpu_throttled_usec`, `cpu_pressure_some_total_usec` (cumulative; a rate is taken over the same window as the CPU) | telemetry only; never feed admission |

Server side, per worker: a measurement is accepted only for `seq` = last `seq` + 1 within one daemon process (`mono_usec` did not go back); else it
is dropped and the worker is in fallback until the next heartbeat. `measured_cpu` = `delta usage / delta mono` in millicores, integer; admission and
ranking use the larger of the last two windows (a rise is believed at once, a fall after two windows). Values that fail `validate` (implied cores above
4x the hardware threads, memory above 4x capacity, counter going back) count as absent. All arithmetic is integer, saturating or `u128`. A signal older
than three windows is stale. `W` is the heartbeat interval the welcome sets (5 s by default).

## Learned cost per action key
Both costs are the existing `js-sizing` estimate (`Estimate::cpu_millis`, `memory_bytes`), with these changes:

- **P90** is nearest-rank over the samples that exist: sort ascending (ties by ring slot, oldest first), take rank `ceil(0.9 n)` (1-indexed), so one to four samples give the maximum. The ring is ordered oldest-first and a new
  sample overwrites the oldest slot. The same definition is used everywhere the document says P90 (cost, profile minutes), and the shared vectors of `load.rs` include tiny rings, ties and a full ring.
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
  quiet runs and the existing staleness. The "current estimate" of a key with no estimate is its cold cost (2 cores): such a key is learned under load
  only from a run that used more than 2 cores. The ratchet is therefore one-way while a node is saturated: an overestimated or cold key never falls
  there, because the lower samples are the ones dropped. That errs toward under-filling (the key is counted above its truth), never toward
  overload; it corrects on the first quiet run, and the observation target on mean measured CPU and the count of keys with no CPU sample
  would show it if it persisted. Counted per key and farm-wide: samples kept, dropped under pressure, and keys with no CPU sample.

## Admission
This section is the flat form of the predicate: it is what a cold key, a key demoted by the shape guard, a retry, and a worker in fallback are charged. By default the cost of a key with enough runs is a curve over its lifetime and the predicate is checked at every future minute (Time profiles below); both forms have the same shape.

For a worker with capacity `C` (CPU threads minus reserve; memory after floors), each lease `i` has the cost `(p_i, m_i)` fixed at admission and held
until its result (any outcome: end, kill, requeue). `U` is the set of admitted leases not in the latest accepted `covered` (uncovered), at most about
`2W` after the agent starts them. VM leases are outside the actions cgroup: their booked CPU and memory, `V`, are added to every sum.

    CPU:     max( measured_cpu, f * sum(p_i, covered) ) + sum(p_i, uncovered) + V_cpu + p_new  <=  C_cpu * (1 + s_cpu)
    memory:  max( sum(m_i, all), ws + sum(m_i, uncovered) ) + V_mem + m_new  <=  C_mem * (1 + s_mem)

Defaults `s_cpu = 0.10`, `s_mem = 0.00`. Memory is the maximum of the measured working set and the sum of the predicted costs of every running lease,
held to the lease's end; **predicted memory never decays**, so a lease that allocates late is still counted at its prediction. CPU decays to the
measurement only down to a floor: the covered leases are charged at least `f` times the sum of their predicted cores (default `f = 0.75`, flag
`--placement-cpu-floor`), and the measurement can only raise the figure above it. Without the floor a covered lease in an idle phase (fetching inputs,
waiting) measures about 0, so each window would refill the whole headroom while earlier leases sleep, and several batches could wake together. `f`
is set from the cost model: a prediction is the P90 of per-run mean cores, a typical run sits near three quarters of it, so at `f = 0.75` the floor
does not bind for leases in their normal phase and measured load keeps its job of correcting under-predictions and releasing the headroom of
over-predicted keys; only the idle-prelude case is held. A smaller `f` admits more and loosens the bound by `1/f`; `f = 1` makes the CPU side the pure
predicted sum raised by the measurement (the fallback figure). The histogram of `k` shows where `f` can sit. The sums are updated after each placement, so one
round cannot place more than the headroom. `request.memory <= capacity.memory` and the lease cap stay. Whole-host, whole-machine and exclusive leases keep
their rules.

**Fallback.** A worker with a stale, absent, dropped or implausible signal, no cgroup (Macs with `MacFootprint`, native and VM nodes, cgroup v1), or an old
daemon, is admitted by the same predicate with `measured_cpu` replaced by `sum(p_i, all)` and `working_set` omitted: the sum of the predicted costs of
its leases, no decay. The cost floor in the fallback stays 1000 m for a worker with no measurement at all (Mac, native, VM): nothing there measures a light population, so only predictions protect it. It never falls back to booked-only. Such a worker is as safe as its predictions are good, and the cold cap below limits the rest.

## The overshoot bound
Let `M` be the CPU of covered leases at the last reading, `P_c = sum(p_i, covered)`, `U_w` and `U_c` the predicted cost of uncovered warm and cold
leases, so `max(M, f P_c) + U_w + U_c <= (1+s_cpu) C`. Let `k` be the **whole-run mean** cores over predicted cores of a warm lease (`mean cores / p_i`, the figure the histogram below records; the mean is learned,
so `k` includes the lease's own variation between runs), `r` the factor by which a lease's peak window exceeds its whole-run mean (`peak window / mean`, also histogrammed), so a lease's demand in one window is at most `r k p_i`; `A` the CPU a
cold lease can use, and `n_c` the cold leases uncovered at once. The covered leases demand at most `r M` when the measurement is the larger term, and at
most `r k P_c` when the floor is (any covered lease, idle now, can wake to its peak window); with `f P_c <= (1+s_cpu) C` the second is at most
`r k (1+s_cpu) C / f`. Then the demand in any window after admission is

    P_cpu  <=  (r k / f) * (1 + s_cpu) * C  +  n_c * A

(the uncovered warm term `r k U_w` is inside it, since `f P_c + U_w <= (1+s_cpu) C` and `f <= 1`; `r M` is inside it too, since `r k / f >= r` once `k >= f`, and below that the floor is the larger term). The floor is what makes this a bound: stated
without it, the covered term `r M` is about 0 when covered leases are idle and the covered leases are bounded by nothing but the admission history.

Cold cap: at most `cold_per_window` leases with a cold CPU cost are admitted per worker per window, default `ceil(C_cpu / 32)` (setting
`--placement-cold-per-window`); a lease stays uncovered for at most `2W`, so `n_c = 2 * cold_per_window`.

**Cold ramp.** The cap limits how fast unlearned keys enter. On an idle 88-thread node (`cold_per_window = 3`, `W = 5 s`, cold cost 2 cores so about
48 cold leases fit in `1.1 C`) the node fills in `ceil(48 / 3) = 16` windows, about 80 s, and a backlog of unlearned keys drains at 36 starts a minute
per node; a mass-cold event (a toolchain bump, a cleared estimator) with a thousand distinct keys therefore takes about half an hour on one node and
proportionally less on a pool, and only for actions shorter than the ramp (longer ones fill the node anyway). Each key is cold once, after its first
run it has a figure. To drain a mass-cold event faster an operator raises `--placement-cold-per-window` for the pool and restarts the scheduler with it;
the bound moves with it by `n_c * A` (at 12 per window: `n_c = 24`, so `516 + 96 = 612 cores = 6.9 C` on 88 threads, against 540 = 6.1 C at the default) and sim o asserts the bound with the new `n_c`.

Defaults (`f = 0.75`, `s_cpu = 0.10`), with the shadow exit criteria below (`k <= 2`, `r <= 2` at p99, `A <= 4` cores at p99), so `r k / f = 5.33`:
on 88 threads `n_c = 6`, so `P_cpu <= 5.33 * 1.1 * 88 + 6 * 4 = 516 + 24 = 540 cores = 6.1 C`; on 16 threads `n_c = 2`,
`P_cpu <= 5.33 * 1.1 * 16 + 8 = 94 + 8 = 102 cores = 6.4 C`. (With `f = 0.5`: `r k / f = 8`, 798 cores = 9.1 C and 149 cores = 9.3 C, which is why the range stops at 0.5 and the default is higher; with `f = 1`: 411 cores = 4.7 C and 78 cores = 4.9 C.) About 6 C, not 10 times. It is a worst case at the p99 criteria, not the expected overshoot; CPU is compressible, so it slows work and does
not fail it. The bound lasts until the leases admitted at `r k` times their cost finish, which can be minutes to hours: the next heartbeat shows the rise
and stops further admission, and a key with understated cost relearns by the rule above, but that protects later leases only. A lease is uncovered for
`2W` plus the heartbeat delivery delay.
Memory: a lease cannot exceed its cgroup limit `1.5 x booking + 512 MiB`, so a cold lease (4 GiB) has `k_m <= 1.63` and
`P_mem <= (1 + s_mem) * C_mem * max(1, k_m)`; a warm key's miss is the estimator's tail (target `k_m <= 1.3` at p99). The per-lease limit and the node
backstop kill beyond that, as today. What bounds `k` is not the mechanism but its measured value, hence the histograms below and the exit criteria.
Persisting above `(1 + s_cpu) C` needs leases whose real use exceeds their prediction, and the learning rule raises those predictions.

## Time profiles
The rule above charges each lease one number for its whole life. Many actions are not flat: a fetch phase, then a burst, then an upload. Two actions whose
busy minutes do not overlap can share a node that two flat peaks cannot. With profiles on, a lease's cost is a curve over minutes since its start, and admission
checks the node's predicted load at every future minute within a horizon, not only the peak. Profiles are the scheduling basis for CPU and memory from the first deploy, with an operator off switch only: `--placement-profile on|off` and
`--placement-profile-memory on|off`, both default `on`. The per-minute series is always collected, whichever the switches say, so history exists before it is used; with a switch `off` the
flat rule above applies to that resource. A key with fewer than 3 usable runs is charged flat until it has runs. Observation (the histograms, the verdict logging of `shadow`) never gates use.

**1. Sampling.** The daemon already reads each running lease's cgroup every `W` (for `r`). It now bins those reads by minutes since the lease's own start
(the agent's clock, the same actual start `covered` uses, so an image pull or a held start does not shift the curve): per closed minute, the mean cores (`delta cpu.stat usage_usec / delta mono`), the peak single-window
cores, and the maximum working set (`memory.current` minus `inactive_file` of the lease cgroup). The minutes are read while the lease runs, not only at its end.
They reach the server in the heartbeat: `NodeLoad.minutes` (field 10) and `repeated LeaseClock { LeaseId lease = 1; uint64 started_mono_usec = 2; }` (`lease_clocks`, field 9, the agent's start of each running lease; the server's elapsed minutes `e_i` are `(latest accepted mono_usec - started_mono_usec) / 60 s`, rounded down, and 0 for a lease not yet reported), `repeated LeaseMinute { LeaseId lease = 1; uint32 minute = 2; uint32 cpu_mean_millis = 3;
uint32 cpu_peak_window_millis = 4; uint64 ws_max_bytes = 5; }` (about 24 bytes). At most one record per running lease per minute, and at most 64 records
in one heartbeat, oldest minute first and then by lease id: twelve heartbeats a minute carry 768 records, above the lease count of a node, so a healthy node never
queues. After an outage the daemon keeps unsent minutes for 10 minutes and drains them 64 a heartbeat (256 leases take about 3.5 minutes to catch up); older minutes
are dropped and counted (`minutes_dropped`), and a run missing more than a fifth of its minutes does not enter the ring. A record is idempotent by `(lease, minute)`, so a resend after a
reconnect is harmless. The final partial minute of a lease rides on its result.
Per node, nothing new is sent: the server derives the node's per-minute series from the cumulative `cpu_usage_usec` and the working set already in `NodeLoad`.

**2. Retention.** In the scheduler's state store, two tables, both numbers only:
- `key_profile`, per action key (the existing key digest, per platform class): a ring of the last 32 runs' per-minute series (cores and working set, up to
  64 minutes a run; bucket 63 is the maximum of the per-minute means from minute 63 on, conservative), and the fitted profile. Only runs that ended on their own enter the ring: a run killed
by the memory ladder or the pressure backstop, cancelled or requeued is truncated and would bias the curve low, so it is excluded (the ladder's rerun enters). At most about 25 KB for a key with long runs, a few hundred bytes for
  a key whose runs last a minute or two. The whole table is bounded by `--profile-store-bytes` (default 256 MiB), evicting the least recently used key (an evicted key is charged flat until it is relearned), and ages
  with the same staleness rule as the cost estimate. A row is written when a run ends, not per minute.
- `node_minute`, per node: minute, mean cores, working-set maximum, lease count; kept 14 days (14 x 1440 rows of 32 bytes, about 0.65 MB per node), pruned daily. It is telemetry, never an admission input: the server buffers a node's rows and writes them in one batch every 5 minutes, so a scheduler crash loses at most 5 minutes of it.
Per-lease raw minutes are not kept outside the ring. Nothing else is stored: no argument vector, path, environment, user or output, only the key digest, the node
identifier the scheduler already holds, and counts. The telemetry tables are public-safe by construction.

**Profile shape: absolute minutes since start, not normalised to run length.** Admission aligns a running lease by its elapsed time and does not know when it
will end; a normalised curve would need a run-length forecast, and where run lengths vary a normalised curve smears the busy phase across the run. In absolute
minutes the variation in run length simply shows as the spread of runs reaching each minute. The fit, per minute `t`: the P90 across the ring's runs that reached `t`
of mean cores, and of working set (with the memory margin of the flat rule, `1.2 x max + 128 MiB`). Under node pressure the flat rule's keep-if-higher applies per minute: a
minute from a run taken at or above 100 permille CPU pressure, or with none, enters the fit only where it exceeds the current fit at that minute (the key's flat cost for a key
with no curve), else it counts as missing, so a starved run cannot flatten the curve. The values are then **dilated** by `delta = 2` minutes (each value is the
maximum of the fitted values within `t +- delta`) so a busy phase that starts a little early or late is still covered. Dilation costs co-admissions: two busy phases are co-admitted only with an idle gap of at least `2 delta + 1 = 5` minutes between them, and shadow counts the verdicts that `delta = 0` would have admitted and `delta = 2` refused (`profile_dilation_missed`). A minute reached by fewer than a quarter of the
runs holds the previous value (the tail); a lease that outlives the curve is charged the tail. A run under a minute is one bucket, which is the flat cost.
`p_bar` below is the key's flat cost (the P90 of whole-run means, as above).

**3. Use in admission.** A lease's charge at minute `t` since its start is `c_i(t) = max(curve_i(t), g * p_bar_i)`, with `g = f = 0.75`
(`--placement-profile-floor`, range 0.5 to `f`; lowering it is a later decision from the histograms): the profile can lower a lease's charge in its idle minutes, never below `g` times its flat cost. Cold keys, keys
with fewer than 3 usable runs, keys demoted by the shape guard, and every worker in fallback have no curve: they are charged flat (`p_bar` and the floor `f`),
exactly as before: a covered flat lease is charged `f * p_bar` and an uncovered one `p_bar`, and a profile lease is floored at the same `g * p_bar` with `g = f`, so mixed sums are consistent. For a candidate worker, with `e_i` the elapsed minutes of each running lease and `H` the horizon (default 30 minutes), admit when for every
`tau` in `0..H`, and once more at `tau = H+` (below):

    CPU:     F_tau + sum(c_i(e_i + tau), uncovered) + V_cpu + c_new(tau)  <=  C_cpu * (1 + s_cpu),    F_0 = max(M, sum(c_i(e_i), covered)), F_tau = sum(c_i(e_i + tau), covered)
    CPU, H+: every lease charged its remaining maximum of c_i from minute e_i + H on (the tail included), summed with V_cpu and the new lease's remaining maximum  <=  C_cpu * (1 + s_cpu)
    memory:  max( sum(R_i(tau), all), working_set_0 + sum(R_i(tau), uncovered) ) + V_mem + R_new(tau)  <=  C_mem * (1 + s_mem)

At `tau = 0` the measurement still enters (it can only raise the figure); at later minutes only the curves do. An uncovered lease (the in-flight rule) is
charged the maximum of its curve over its first two minutes, so a lease that has just started is never counted at its quiet start. `R_i(tau)` is the
**remaining maximum** of lease `i`'s memory curve from minute `e_i + tau` on, including the tail, and never less than its current working set: memory is held until the
lease's peak is behind it, then released, never before. Cold and flat leases hold their cost to the end as before, and a lease retried up the ladder is charged flat at its rung.

**Memory safety is the ladder.** Memory is not compressible and the remaining maximum drops the flat rule's guarantee that predicted memory never decays: after its curve's peak a lease that
allocates again is charged only its working set. What bounds it is the kill-and-retry ladder, unchanged: the booking and the lease's cgroup limit (`1.5 x booking + 512 MiB`) stay at the flat cost, so a lease
whose memory runs past its curve rises to the limit and is killed individually, then retried at the next rung, charged flat at that rung. The key learns from it: the killed run does not enter the CPU
ring (its CPU curve is truncated and would bias low), but its per-minute working set up to the kill, including `memory.peak` at the kill, enters the key's **memory** ring as a *censored*
run: its minutes count at the values observed and the kill minute and the minutes after it count at no less than `memory.peak`, and a censored minute can only raise the P90, never lower it. The retry, if it ends on
its own, enters both rings. **Memory misfit** is a lease-minute whose working set exceeds its remaining maximum `R_i`, or a kill by the cgroup limit; it is counted per key and fleet-wide. The automatic per-key fallback is
the shape guard, for CPU and memory alike: two of the last 3 runs with a misfit demote that resource's curve to the flat hold for the next 8 runs (counted). A key therefore causes at most two ladder kills from its curve
before it is charged flat, and one after the first kill, because the censored run has already raised its curve.

**Memory worst case.** In the window before a misfit lease is killed the node holds the sum of curves exceeded by that lease's excess: `P_mem <= (1 + s_mem) C_mem + sum over misfit leases (L_i - R_i)`, where `L_i`
is the lease's cgroup limit and `R_i` its charge; with `s_mem = 0` and a flat-cost booking `b_i`, each misfit lease adds at most `1.5 b_i + 512 MiB - R_i` (a cold 4 GiB booking with a 1 GiB tail charge: up to
5.5 GiB). The number of simultaneous misfits is what the bound depends on, and is not capped by a formula: misfits are rare per key (the fleet target is at most 0.1% of lease-minutes, and each key is kicked to flat after two), but a
shared cause (a changed input that makes many keys grow late together) can misfit many at once. Their excess is then removed by the per-lease kill or the node pressure backstop, as today, and the number of retries per
lease is at most the rungs of the ladder. The worst case is therefore a bounded excess per misfit lease for the time to the kill, not a sustained sum.
**The bound with profiles.** Define `k_c` as the lease-minute's actual mean cores over its charge `c_i(t)` (histogrammed per lease-minute; a minute counts only when the actual exceeds
the charge by at least 250 m, so a tiny charge cannot dominate) and `q` as the peak window of that minute over the minute's mean. A profile lease's demand in a window is at most `q k_c c_i`; a flat lease's is at most
`r k p_bar_i` with a charge of at least `f p_bar_i` (the bound above). The sum of the charges is within `(1 + s_cpu) C` at every minute, so

    P_cpu  <=  max( r k / f , q k_c ) * (1 + s_cpu) * C  +  n_c * A

With the criteria (`r, k <= 2` at p99, `q <= 2`, `k_c <= 1.5` at p99, `A <= 4`) and `g = f = 0.75`: the flat term is 5.33 and the profile term 3, so the figures are those of the flat rule,
**540 cores = 6.1 C on 88 threads and 102 cores = 6.4 C on 16**; a pool whose demand is only profile leases is bounded by `3 * 1.1 * 88 + 24 = 314 cores = 3.6 C` (`3 * 17.6 + 8 = 61` cores = 3.8 C on 16).
The CPU worst case with profiles on at the defaults (`g = f = 0.75`) is the figure above, 6.1 C. CPU is compressible, so exceeding it slows work and does not fail it, and the demand is transient (the next heartbeat
shows the rise and stops admission; the overrunning leases finish). The profile term is a statement at the criteria, not a hard cap: a profile that is wrong (a busy phase where the curve said idle) has a large `k_c` for its minutes, and the hard limit there is only the floor,
`r_b k / g` with `r_b` the key's own peak ratio (3 to 4 for a bursty key), about 8 to 11 for one lease type, before the guard acts. That is why the shape guard and the exit criterion exist and why sim q
asserts a wrong profile. The **shape guard** (per key): 2 of the last 3 runs with a minute at `k_c > 2.0` demote the key to the flat cost for its next 8 runs, counted. It is looser than the fleet criterion (`k_c <= 1.5` at p99), which
is a statement about the whole population, so that a single noisy minute does not demote a key. Lowering `g` below `f` (admitting more at a larger worst case, `r k / g`) is a later decision from the histograms
and not part of shipping; the off switch keeps the flat rule one setting away.

## Starvation
A host marked for a whole-node request (reserved or draining for a whole-machine, exclusive or VM lease) is excluded **before** the load predicate and before ranking: its load falls as it drains, which would otherwise rank it first and let small actions refill it, so the whole-node request would miss its deadline. The existing mark is the gate; the predicate never sees such a host.

No tiers; queue order is kept. A **pool** is the set of workers a request can run on (platform, drivers, devices, avoid set), as `Servable` computes it.
The **head** is the first queued action, whole-machine, exclusive and VM requests excluded (they keep the existing whole-node reservation, and a worker
that holds one is never chosen), that has no worker with room. It reserves the worker in its pool with the smallest binding ratio
`max((L_cpu + p) / C_cpu, (L_mem + m) / C_mem)` (`L` the left side of the predicate without the new cost), sticky while that ratio is within 0.1 of the
best. A reserved worker admits no later request (a request that fits elsewhere goes elsewhere; one that fits only there waits, as for a whole-node
reservation). One reservation per pool: a second blocked head waits until the first starts, then becomes the head. A reservation on a worker whose
signal goes stale stays and uses the fallback figures. Bound: the head starts within the longest remaining lease at reservation, plus `2W`, plus one
round. Work ahead of the head is never held back.

## Lying or stuck node, restart
- **Plausibility.** Lease CPU arrives only when a result does, so a long lease would lag any comparison (its hours of CPU would land in one short
  span). The check therefore uses only leases that were admitted and ended inside the span, and compares their summed `cpu_usage_us` with the growth of
  `cpu_usage_usec` over the same span (which also holds the other leases, so the growth can only exceed the sum on a healthy node). The span
  extends back window by window until the sum reaches 60 core-seconds, at most one hour; below that the check is *unchecked*, not failed, and the
  unchecked time is reported. Growth below half the sum marks the signal implausible, and the worker uses the fallback until it agrees again;
  a node with no lease reporting more than half of `C_cpu` for three windows is marked the same way. This catches a wrong cgroup, a stuck counter and a
  broken reader on any node with turnover; a node running only leases longer than the span is unchecked and says so, and an agent that lies in both
  places is out of scope.
- **Scheduler restart or new leader.** Recovered leases count their persisted cost (never recomputed from the current estimator) as uncovered; the first heartbeat
  after re-registration has no previous reading, so the worker is in fallback for it and measured from the second (about `2W`). Never booked-only.
- **Lease end** removes its cost at the result; a short action therefore costs its prediction for its whole life, which is why the sub-core floor matters.

## Plumbing
| Piece | Where |
|---|---|
| Who produces `NodeLoad` | Two kinds of host produce it: kbf-daemon hosts (this table), and `js-agent` hosts through the cgroup collection slice in `js-agent` (owned separately; it fills the same `js_agent_wire` `NodeLoad`). A host whose agent does not yet produce it is in the fallback, never booked-only. |
| Read `cpu.stat`, `memory.current`, `memory.stat`, `cpu.pressure` of the actions cgroup | new `node_load.rs` beside `cgroup_usage.rs` in kbf-driver-container, over the existing `CgroupFs` trait: `read_actions_load(mount, actions)` |
| Hand it to the daemon | new defaulted method on the kbf-daemon `Runtime` trait, `node_load(&self) -> Option<NodeLoad>`; native, VM and Mac runtimes keep the default `None` |
| Put it in the heartbeat | kbf-daemon `daemon.rs`, where each `Heartbeat` is built: reads `node_load` and the lease table (for `covered`) once per interval |
| kbf protobuf | `Heartbeat.optional NodeLoad node_load = 6`; `message NodeLoad { optional uint64 seq = 1; optional uint64 mono_usec = 2; optional uint64 cpu_usage_usec = 3; optional uint64 memory_working_set_bytes = 4; repeated LeaseId covered = 5; optional bool actions_cpu_limited = 6; optional uint64 cpu_throttled_usec = 7; optional uint64 cpu_pressure_some_total_usec = 8; repeated LeaseClock lease_clocks = 9; repeated LeaseMinute minutes = 10; optional uint64 memory_active_file_bytes = 11; }` (fields 9 and 10 are the profiles slice; see Time profiles for `LeaseClock` and `LeaseMinute`), plus `optional uint32 cpu_peak_window_millis` on `CgroupUsage` (next free tag). Additive; a fixtures proto and goldens move in the same change. |
| js wire | `js_agent_wire::Heartbeat.load: Option<NodeLoad>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, the shape of `taint`), with `cpu_usage_usec`, `mono_usec`, `memory_working_set_bytes`, `covered`; `validate` bounds as above |
| js scheduler | `js_sched::Event::Heartbeat` gains `load: Option<NodeLoad>`; the predicate, the cost record and the shares live in one pure module (`load.rs`) written in `js-sched`, with shared test vectors, which kbf-sched takes through the byte-identical copy |
| Profiles | `js-sizing`: the per-key ring, the dilated P90 fit, the shape guard; the scheduler's per-worker load array over the horizon; the `key_profile` and `node_minute` tables in the state store |
| Cost | `js-sizing`: floor 250 m, scaled samples, the learning rule, `Observed` says why a sample was dropped |
| Flags | `--placement-load on\|shadow\|off` (default `on`) and `--placement-load-off-pools` (pools the operator holds at `off`) on kbf-server and as a `Scheduler` setter plus `js-lease-server::Config` field; `--placement-cold-per-window`, `--placement-cpu-floor` (`f`, default 0.75, range 0.5 to 1), `--placement-profile on|off` and `--placement-profile-memory on|off` (both default `on`, operator off switches), `--placement-profile-floor` (`g`, default 0.75 = `f`), `--profile-store-bytes`, `--placement-s-cpu`, `--placement-s-mem`, `--placement-ws-file-share` (`a`, default 0.25). On kbf-node: `--actions-cpu-max=on\|off` (default `on`, today's behaviour); `off` makes `limit_cpus` write `max` and skip the limit, and the operator sets it on the nodes of a pool when that pool goes `on` (the node starts first, then the pool mode flips). The server does not trust the pairing: `actions_cpu_limited` in the heartbeat puts a worker of an `on` pool that still has a limit in the fallback and raises a counter, and a node with `off` in a pool that is not `on` is merely under-limited, reported the same way. |
| Daemon protection | the daemon writes `supervisor/cpu.weight` = 1000 (flag `--supervisor-cpu-weight`, default 1000) beside its existing `supervisor/memory.min` write in kbf-driver-container `delegate.rs`; `actions/` stays 100. The storage unit gets `CPUWeight=1000` in its packaged systemd unit (a sibling unit, so the unit weight applies). Not a kbf-server flag. |
| Learned sizes | `--placement-load` `shadow` or an explicit `on` with `--learned-sizes=off` is refused at start-up, since the CPU estimate exists only with learned sizes; with `--learned-sizes=off` the default resolves to `off`. |

## Ranking
`load.rs` ranks by the largest of CPU share, memory share and lease share, now `L_cpu / C_cpu` and `D_mem / C_mem` from the predicate's own left sides
(fresh or fallback), exact rationals of integers, the lease count the tie-break. The pressure and throttle figures are telemetry only.

## Telemetry
Per worker each heartbeat: measured CPU and memory over capacity, uncovered cost, sum of predicted cost, throttled time, run-queue proxy
(`cpu.pressure`), non-action CPU, plausibility state. Per finished lease, **in `shadow` and `on`**: histograms of `k` for CPU (`mean cores / p_i`, the same `k` as the bound), the share of node-windows where the floor binds (`f * P_c > M`),
of `r` (`peak window / mean`, from the daemon's per-heartbeat read of the lease cgroup) and of `k_m` (`memory.peak / m_i`). Per key: cost, sample count;
farm-wide: cold and unsampled keys, cold leases admitted; profile counters `profile_dilation_missed`, `minutes_dropped`, shape-guard demotions and memory misfits. `C_cpu` is hardware threads: 100% means threads busy, not cores, so on SMT hardware a node at 100% is further from full than it reads. The summary reports the distribution of measured CPU over capacity, the share of windows
above `1 + s_cpu`, and the run queue.

## Rollout
`on` is the default, with operator off switches: `--placement-load off` (per cluster, or per pool with `--placement-load-off-pools`), `--placement-profile off` and `--placement-profile-memory off`. Profiles are used
from the first deploy; the per-minute series is collected from the first heartbeat, and a key has a curve after 3 usable runs (until then it is charged flat). `shadow` remains as an observation mode for an operator who
wants the new predicate's verdict logged beside the old rule's decision; it is not a gate. The `--placement-load` setting is kept as an on/off switch rather than a staged rollout because
it is separate from profiles: it also changes what kbf-node writes (`cpu.max`, NUMA) and what admission trusts, and an operator needs a way back that does not depend on a redeploy.

The numbers below are observation targets with alarms, not gates: each is exported and an alarm fires when it is missed, and the operator's response is the off switch or a flag, not a blocked release.

| Observation | Target (alarm when missed) |
|---|---|
| signal | fresh on at least 99% of heartbeats of container workers |
| cost model | p99 `k <= 2`, p99 `r <= 2`, p99 cold-run mean cores `<= 4`, p99 warm `k_m <= 1.3`; the mean of `k` per node weighted by `p_i` times time at least `f`; the share of node-windows where the floor binds not above 10%; keys without a CPU sample at most 5% of started leases |
| CPU profiles | p99 `k_c <= 1.5` over lease-minutes of profile keys; p99 `q <= 2`; at most 1% of lease-minutes above 2 x charge; shape-guard demotions at most 5% of profile keys; `minutes_dropped` under 1% of minutes; `profile_dilation_missed` reported |
| memory profiles | memory misfit (working set above the remaining maximum, or a kill by the cgroup limit) in at most 0.1% of lease-minutes; ladder kills attributable to a curve not above the flat rule's; no key with more than two curve kills before demotion |
| node | daemon `cpu.pressure` and lease self-fence trips not above their pre-deploy level (this measures the supervisor weight); p99 measured CPU / `C` at most 1.25 and never above the bound; node-pressure kills not above the pre-deploy level; queue wait p95 not more than 10% above it; mean measured CPU / `C` above it |

## Simulation tests
Logical ticks only (js-sim, js-sched sims), `W` expressed in ticks; no wall-clock assertion. Bounds are checked against the model's ground-truth
demand, not the scheduler's own figures. The last column names the mechanism the assertion exercises: the assertion fails when that mechanism is removed.

| Sim | Scenario and assertion | Exercises |
|---|---|---|
| a | 88-thread node, wide-pool low-duty actions, single-threaded long ones, plus a light short population (0.3 core, under 2 windows); with a contention model (slowdown = demand / cores): demand `<= (1 + s_cpu) C` for constant-duty actions | measured term (booked rule), or the 250 m floor (light population stays at today's concurrency) |
| b | A stream of wide actions and a node-wide head: head starts within the stated tick bound; a head blocked in pool A does not delay pool B; two heads in one pool: second waits for the first | the reservation |
| c | Memory ramps late (allocates at 60% of the run) and mispredicts (3% tail runs); page cache fills 80% of RAM: the sum of ground-truth memory `<= (1 + s_mem) C max(1, k_m)`, and the cache does not block admission. Livelock case: an idle node with no leases and hot page cache at 100% or more of `C_mem`: a new action is still admitted and the working set the rule uses is at most `a * C_mem` of cache | memory held to lease end; the working-set basis |
| d | Stale, absent, cgroup-less (Mac-like), partial-field, one stale among fresh, going stale and returning: predicted-sum admission, demand within the bound; off and shadow traces equal the existing golden | the predicted-sum fallback |
| e | Throughput and p99 wait against booked-only with the contention model: not worse by more than 5% on (a), at least 10% higher on the light population | contention model absent (claim cannot fail); floor |
| f | A burst of equal submissions in one round, then across two rounds, over several workers, before any heartbeat: admitted `<=` headroom, spread by rank | per-placement update of the sums |
| g | Cold burst: 40 unlearned keys at once: uncovered cold leases per worker `<= 2 * cold_per_window`, demand within `P_cpu` | the cold cap |
| h | Phase change (idle for 4 windows, then 16 cores): demand within the bound, and no placement after the heartbeat that shows the rise. Idle prelude: batches of leases are admitted window after window while every earlier lease is idle (measures about 0), the node is filled to the predicate, and all leases then wake at their peak (`r = 2`, `k = 2`): demand `<= (r k / f) (1 + s_cpu) C + n_c A`, and the covered charge never falls below `f * sum p_i` while they idle | believing a rise at once; the covered-CPU floor |
| i | A key understated 4x: demand `<= 4.4 C` at first; after the learned samples (taken under pressure) the cost is within 1.25 x true and demand `<= 1.1 C` | the learning-under-load rule |
| j | Scheduler restart with leases in flight: no placement beyond the bound across the restart; recovered leases are counted at their persisted cost, and the sums before and after the restart are equal even when the estimator changed in between | counting recovered leases |
| k | Stuck-low signal (reports 0 with leases running): flagged within the stated number of windows, then predicted-sum, demand within the bound. Long leases (40 windows) beside short ones on a healthy signal: never flagged; a node with only long leases is unchecked and reported, not flagged | the plausibility check; counting only leases that ended in the span |
| l | Mixed 16- and 88-thread nodes, a pool-sized key learned on one and placed on the other: demand within the bound on both | the thread-count scaling |
| m | Shadow: placements identical to `off`; logged verdicts equal the first-round verdicts of an `on` run; histograms count every lease | shadow isolation |
| n | Ladder retry: killed at 4 GiB, retried at 16 GiB, completes; the retry is counted at the rung; the learned estimate rises without thrash | counting the retry at the rung |
| s | Reserved host: a host marked for a whole-node request while a stream of small actions arrives and its measured load is the lowest: no action is placed on it, the whole-node request starts within the reservation bound | the reserved-host exclusion before the predicate |
| t | `--placement-load on` with `--placement lease-table` is refused at start-up; under `sched` the lease cost read after `recover` equals the cost at admission | the flag relation; the persisted cost |
| o | Mass-cold event: 1000 unlearned keys queued for an empty 88-thread node: 48 cold leases running after exactly 16 windows, never more than 3 cold admitted per window, uncovered cold `<= n_c`; with `cold_per_window = 12` the node fills in 4 windows and demand stays within the bound with the new `n_c` (612 cores on 88 threads at the criteria) | the cold cap and its flag |
| p | Two keys with non-overlapping busy phases on one 88-thread node: A busy (60 cores) in minutes 0 to 2 and quiet after, B quiet for 8 minutes then busy (60 cores) in minutes 8 to 10 (an idle gap of 5 minutes, `2 delta + 1`); both have profiles learned from earlier runs. Under the profile rule B is admitted beside A; under the flat peak rule (`--placement-profile=off`, 60 + 60 > 1.1 C) it is not. With B's busy phase at minute 6 (a gap of 3) the dilation makes it refuse, and shadow counts it in `profile_dilation_missed`. Two busy phases at minute 40 (beyond `H`) are refused by the `H+` check. Ground-truth node CPU stays within the bound at every tick | the profile rule (B waits without it); the dilation; the `H+` check |
| q | A wrong profile: B's busy phase actually falls at minutes 0 to 2 (a shift of 6 minutes, beyond the dilation) so both are busy at once: ground-truth demand at every tick `<=` the wrong-profile limit `(r_b k / g)(1 + s_cpu) C + n_c A` with `r_b` the key's own peak ratio; after two such runs the key is demoted to flat and an identical pair is no longer co-admitted; a shift of 1 minute is absorbed by the dilation and never exceeds `(1 + s_cpu) C` | the floor `g`; the shape guard; the dilation |
| r | Memory curve (the default): a key peaks at minute 3 then falls, then allocates again past its remaining maximum: the lease is killed at its cgroup limit and retried at the next rung charged flat; the killed run's observed peak enters the key's memory ring as a censored run and raises its curve, and the CPU ring does not take it; after a second misfit in 3 runs the key is charged flat for 8 runs; node memory never exceeds `(1 + s_mem) C_mem` plus the misfit leases' `L_i - R_i`; a key peaks at minute 3 then falls; a second lease is admitted only after the first passes its peak, never while its current working set exceeds its remaining maximum; a lease whose working set exceeds the curve is charged its current working set | the remaining-maximum rule with the current-working-set floor; the censored sample; the per-key demotion |

## Out
Pinning, cpusets, NUMA placement, QoS tiers, per-action reservations, a CPU limit on the actions cgroup (for `on` pools), and any client environment
variable. The cgroup limit of a lease stays the memory ladder's.

## Order
The sampling and the tables come first in the order below, since a key has a curve only after 3 runs and history should accumulate from the first deploy.

`js-sched` first: `load.rs` (the predicate, the cost record, the shares) with the shared vectors, the persisted cost, the simulations, and the `--placement-load` flag under `--placement sched`; with the `NodeLoad` fields in `js-agent-wire` and the `js-sizing` changes. Then the plumbing in kbf (`node_load.rs`, the `Runtime` method, the proto field, the daemon heartbeat, the node flags) and the same module through the byte-identical copy in kbf-sched, with `cpu.max` and `numa-node` removed; and the `js-agent` collection slice from its owner.
