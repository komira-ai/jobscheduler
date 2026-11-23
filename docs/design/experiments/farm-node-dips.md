# Why the Linux farm nodes dip to 1-35% CPU while builds run (2026-11-23, 01h-13h UTC)

Status: investigation record. It states what was measured and what it shows. It changes nothing on the farm and decides nothing; the follow-ups are in section 6.

## The short answer

The six Linux nodes (88 cores, 251 GiB each) averaged 65-73% CPU over 01h-13h UTC. sysstat's 10-minute samples show each node falling to 1-35% busy for one or two samples at a time, which read as an hourly sawtooth. Two separate things cause the idle CPU, and neither is on the nodes:

1. **The deep dips are gaps in demand, and all six nodes dip together.** In the nine deep-dip windows the job service's queue of Linux jobs was empty, or held a short burst for at most a few minutes (section 2), and each job submitted was otherwise placed within about a second. The farm's only demand is komira's CI, and during each dip every build in flight was in a critical-path tail: one to three long Mojo compile or link actions (`komira_bench_runner_main`, `copy_runner`, `bench_q6_phases`), with nothing behind them to run in parallel. Several merge-queue builds share these tails and end together. The dips are not periodic; they come every 45 to 140 minutes, whenever the builds in flight reach their tails at the same time.
2. **When work does wait, memory bookings refuse it, not CPU.** In 552 of 720 minutes Linux jobs waited in the queue, and those minutes hold 59% of the fleet's idle CPU. In 323 of them every node was booked at 220 GiB or more, against about 250 GiB that the pool books, while only 14-45 of its 88 cores were booked. A job books on average 3.2 times its own measured peak (2.0 times when weighted by how long each job runs). Physical memory was never short (sysstat's lowest MemAvailable was 97 GiB).

No node-local source lines up with the dips: no timer, cron job, OOM kill, tierstore sweep, hostkeeper update or js-agent restart falls in all of them (section 4).

## 1. Sources

All times are UTC on 2026-11-23. Everything was read, nothing changed.

| source | what was read | grain |
|---|---|---|
| sysstat on each node | `sadf -d -U sa23 -- -u` and `-q` | 10 min (the collector's interval, its finest) |
| the job store | every job row (submitted, placed, ended, host, labels) and allocation row (host, booking, created, freed), decoded with `js_jobs::rows` (behind the crate's `test-support` feature) | the store's clock (Unix time; checked against the agents' `placed_at_ms`, which agree to 1 ms) |
| js-agent on each node | `job: Started` (booking) and `job: result` (booked, measured peak, CPU time), restarts and handovers | 1 s |
| kbf's frontend | the service log | per event |
| komira CI | each run's jobs and steps, and buck2's `Waiting on ...` lines in the build logs | 7 s |
| node-local logs | systemd timers and units, cron, kernel and OOM, tierstore and tierstore-fs, hostkeeper, weed, postgres | per event |

The job service's own log for the window was gone (its container had been replaced), so the queue was rebuilt from the job store: a job counts as queued from its `submitted_at` to its `placed_at`.

## 2. When the nodes dipped, and whether together

sysstat, the 10-minute samples at or under 35% busy (the time is the sample's interval):

| node | mean 01h-13h | samples at or under 35% busy |
|---|---|---|
| 1 | 73% | 03:40-03:50 (32%), 06:19-06:30 (31%) |
| 2 | 65% | 02:40-02:50 (18%), 03:40-04:10 (9-25%), 06:20-06:30 (4%), 07:50-08:10 (26-32%), 08:50-09:10 (4-25%), 10:40-10:50 (35%), 11:30-11:40 (24%) |
| 3 | 68% | 03:40-04:10 (21-34%), 06:10-06:30 (18-32%), 08:50-09:10 (12-27%) |
| 4 | 66% | 03:19-03:30 (28%), 03:39-04:10 (5-27%), 06:20-06:30 (5%), 07:50-08:00 (8%), 09:00-09:10 (23%), 12:20-12:30 (31%) |
| 5 | 66% | 02:30-02:50 (16-19%), 03:20-04:10 (6-29%), 06:20-06:30 (10%), 08:50-09:10 (19-30%), 11:30-11:40 (1%) |
| 6 | 66% | 02:30-02:40 (32%), 03:20-04:10 (9-34%), 06:10-06:30 (4-31%), 09:00-09:10 (4%), 11:29-11:40 (23%), 12:20-12:30 (34%) |

The allocation rows give the same picture per minute, and every node falls in the same minutes. For example, from 06:20 to 06:22 nodes 2 to 6 held no allocation at all and node 1 held 4 to 19 small ones. From 08:59 to 09:02 nodes 2 to 5 held none.

The fleet-wide windows where the booked CPU fell under 35% of the 528 cores, with the Linux queue in each:

| window | Linux jobs queued, most at once | what the builds were doing |
|---|---|---|
| 02:37-02:45 | 0 | two merge-queue builds alone on `komira_bench_runner_main` `mojo_build`, ended 02:39:41-43; one build cancelled 02:43:27; one parked on `test_run_sql_joins` with 17 others |
| 03:33-03:45 | 0 | one build left, on a `mojo_build_test` and then a `copy_runner` `mojo_build`; the others near 100% cached |
| 03:50-04:08 | 0 (143 Mac jobs waited for a Mac) | one Linux build; the queue held only Mac jobs |
| 06:07-06:28 | 0 to 57, briefly | every build on 1-3 long Mojo compiles (`copy_runner`, then `komira_bench_runner_main` alone 06:19-06:27) |
| 07:45-07:54 | 0 | two builds on one `mojo_build` each, one ended 07:52, two just started and mostly cached |
| 08:54-09:11 | 0 to 1 | four builds on 1-4 actions (`bench_runner`, `bench_q6_phases`) ending 09:01-09:10; two merge-queue builds starting 09:02 and 09:05 |
| 10:45-10:51 | 0 | five builds in tails of 1 to 40 actions; one new build at 10:49:52 |
| 11:30-11:33 | 0 | four merge-queue builds in tails, one down to a single test action |
| 12:18-12:27 | 0 to 1 | four merge-queue builds on 1-3 actions since about 12:08, three ending together at 12:19:34 |

One such buck2 line, from a PR build during the 06:07-06:28 dip:

```
[2026-11-23T06:20:11.906+00:00] Waiting on komira//src/bench_runner:komira_bench_runner_main (...) -- mojo_build [re_execute], and 1 other actions
```

It printed the same line every 7 s from 06:19 to 06:27.

## 3. Work that waited while CPU was free: the memory bookings

Minutes with Linux work queued and booked CPU under 35% (02:47-02:50, 03:30-03:32, 08:36-08:47, 10:26-10:36, 12:50-12:51) show nodes full on memory. At 08:38 the six nodes were booked 238-250 GiB each with 14.5-21.2 cores booked each, and 298 jobs waited. The longest wait in that window was 630 s.

A job that names no size is booked what its class's history says (`docs/design/sizing.md` 7.7): `1.2 x` the largest peak of its class's recent runs, plus 128 MiB (`crates/js-sizing/src/summary.rs`). kbf names the class (`size-class`) by the build target and build-or-test only. The kind of action goes in `size-fallback`, which is used only while the class has no runs. So every action of a target books its heaviest action's memory. From js-agent's result lines (booked against measured peak), joined to the jobs' labels:

| class | kind | runs | peak p50 | peak max | booked mean |
|---|---|---|---|---|---|
| `komira.src.sql.sql.build` | `mojo_compile` | 2507 | 3.3 GiB | 12.8 GiB | 13.3 GiB |
| `komira.src.sql.sql.build` | `run.gate` | 656 | 0.0 GiB | 0.1 GiB | 12.4 GiB |
| `komira.src.sql.sql.build` | `kcov cov_run` | 407 | 0.4 GiB | 0.6 GiB | 15.1 GiB |
| `komira.src.optimizer.optimizer.build` | `mojo_compile` | 2990 | 2.8 GiB | 10.6 GiB | 14.1 GiB |
| `komira.src.optimizer.optimizer.build` | `run.gate` | 322 | 0.0 GiB | 0.1 GiB | 14.0 GiB |
| `komira_decimal_compare_conformance` | `run.gate` | 856 | 0.1 GiB | 0.1 GiB | 15.5 GiB |

Over the day, the 70,712 runs with both figures booked 15.0k GiB-hours against 7.5k GiB-hours of their own peaks, 2.0 times. Counted per job instead (each job's booking against its own peak, not weighted by run time), the bookings sum to 3.2 times the peaks, because short jobs are the most over-booked. Gate actions booked 25 times their peak and coverage runs 32 times. `mojo_compile` holds 97% of the booked memory-time at 2.0 times its peak, because one target's compiles differ: the median is a quarter of the largest.

The other 229 queued minutes, where some node had memory room, were not studied here.

## 4. Node-local sources: none lines up

Checked on all six nodes against the nine windows of section 2:

- **Timers and cron.** No unit or cron job starts on all six nodes inside the dips. `apt-daily`, `apt-daily-upgrade`, `man-db` and `motd-news` start at different times per node. The hourly `cron.hourly` runs at :17 and sysstat at every 10 minutes, in every hour alike.
- **OOM.** No `systemd-oomd` lines. The kernel's memory-cgroup OOM kills of job processes fall only at 04:14-04:21, plus one node at 04:39, after the 03:50-04:08 window ended.
- **tierstore.** The put-directory sweep logs once a minute, every minute, on every node, at the same rate in and out of the dips. No eviction, GC or reconciler bursts. Its only burst ("the fill socket is not answering") came with its update restart at 04:46-04:59.
- **hostkeeper.** Its hourly `maintenance: looked` lines come at about :19 and do not match. It restarted on all six nodes at 07:44-07:46, at the start of one of the nine windows (07:45-07:54); nothing was cordoned and the agents' job counts did not change around it.
- **js-agent.** It restarted in place on all six nodes at 04:37, 05:29, 07:18, 08:10, 08:59, 12:03 and 12:41 (the update hook hands its jobs over without a drain). Only 08:59 falls inside a dip, and that dip started at 08:54.

## 5. Decisions taken in reading the data

- The queue was rebuilt from the job store, not taken from the job service's log, which no longer held the window. A job is queued from `submitted_at` to `placed_at`; a job that never placed counts until it ended.
- Linux demand is the queue of jobs that were placed on, or are waiting for, the six Linux nodes. The Mac pool's queue is kept apart: from 03:50 to 04:08 its 143 jobs waited for a Mac and say nothing about the Linux nodes.
- A dip is a 10-minute sysstat sample at or under 35% busy per node, or a minute in which the fleet's booked CPU was under 35% of its cores.

## 6. What would change

- **Demand gaps (section 2).** These are an operation, not a defect: the farm runs out of CI work when the builds in flight all reach their serial tails. What would fill them is more concurrent independent builds (merge-queue groups that do not all end together), shorter critical-path tails in komira's build (the `komira_bench_runner_main` and `bench_q6_phases` link and test steps each held a build alone for 5-10 minutes), or lower-priority background work queued behind CI.
- **Memory bookings (section 3).** This is in code: kbf should name the class by target and kind, so a 0.1 GiB gate action stops booking its target's 13 GiB compile. The estimate from the same data is that this frees about 7% of booked memory-time. The rest is `mojo_compile` actions of one target with different sizes, and a finer key (each action's output inside the target) is the next step to measure. Booking from the largest recent peak stays: a booking is not a limit, and lowering it is a separate decision.
