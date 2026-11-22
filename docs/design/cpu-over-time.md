# Scheduling on CPU use over time

Status: design for review. Nothing here is built except the replay (`crates/js-cpu-replay`). Companion to
[sizing.md](sizing.md) (how a class's CPU is booked) and [placement-by-load.md](placement-by-load.md) (the
load rule and the time profiles designed for leases, never wired to jobs).

Labels used throughout:
- **READ**: read in the code.
- **REPLAY**: printed by `js-cpu-replay` from a trace committed in this pull request. `tests/farm.rs` pins the
  figures: the summary of the three-day export (`tests/fixtures/export.trace.gz`), and the report of the window
  with no CPU limit (`tests/fixtures/farm.trace.gz` gives `tests/fixtures/farm.report.md`).
- **MEASURED**: counted from the three-day export or from the farm's metrics, without a test pinning it.
- **ASSUMED**: a choice or belief not yet tested.
- **UNKNOWN**: not known; the build step that measures it is named.

## 1. First page

### The problem, in five lines
1. A job's CPU booking is the median of its class's last 32 runs' mean CPU (CPU time over wall time,
   komira-ai/jobscheduler#1026). A class is a job type, for example the compile of one target. MEASURED in the first
   8 minutes after the change, against the hour before:
   - jobs per host: 3 to 4 before, 30 to 36 after;
   - hosts: about 20% busy before, about 99% after;
   - jobs finished per minute: 58 before, 128 after;
   - median queue wait: 15 min before, 4 min after;
   - median run time: 32 s before, 65 s after;
   - load average on an 88-thread host: 13 to 26 before, 250 to 330 after.
2. A booking is one number. A class can average a few cores and burst to 20 for a few seconds. The sum of bookings
   can then sit at 1.1x of a host's cores while the real demand in some seconds is several times that.
3. A host cannot be measured above its cores (READ: `NodeLoad::busy_millicores_since` is CPU time over wall time),
   so the placer's load rule cannot see overload. Once a host is full it reads 88 cores and admits until the
   bookings alone reach 96.8 (READ: `WorkerLoad::left`, `max(measured, covered) + uncovered`). Only CPU pressure
   would show it, and the agent does not send pressure for a host today.
4. The agent reads every job's CPU counter once a second, keeps the highest five-second rate, and throws the rest
   away (READ: `crates/js-agent/src/crun/peak.rs`, `PeakWindow`). The server therefore cannot tell a steady 2 cores
   from 1 core for 60 s followed by 20 cores for 3 s.
5. The owner's design: record CPU per second, learn each class's shape over its run, and place jobs so that a busy
   phase lands on other jobs' quiet phases. It aims at 1.0x to 1.1x of the cores at every moment, not only on
   average.

### What the data says first (section 9 has the detail)
- The jobs carry far more CPU than the history booking them saw.
  - Over the three days, runs of 30 s or more carry 96.2% of the CPU (REPLAY).
  - Over the three days their mean CPU had a median of 1.0 core (p25 0.7, p75 1.2) (REPLAY). Most of those runs
    were held by a `cpu.max` of their booking.
  - In the six hours with no limit the same runs had a median of 4.2 cores (p25 2.8, p75 5.0) (REPLAY).
  - Bookings learned from held runs are about four times too low for the runs now free to burst. That alone puts
    30 to 36 jobs on a host asking for well over 88 cores, before any burst. The level error may be most of
    today's overload; the shape is the second question.
- On those six hours replayed, today's rule at a limit of 1.0 (instead of 1.1) gets most of what a perfect per-job
  curve buys, with jobs spread flat over their runs (REPLAY, 4x arrivals). The flat bracket is the only one the
  file allows today: it records no peaks. The rows of the job store hold a five-second peak, and the
  recording-only change gives the per-second series.

  | rule | slowdown | host-time over the cores | jobs per hour |
  |---|---|---|---|
  | limit 1.1 (today) | 19.7% | 51% | 9,192 |
  | limit 1.0 | 3.3% | 16% | 9,152 |
  | perfect curve, limit 1.0 | 0% | 0% | 9,211 |
- The replay's own check is not yet met (section 10). Replayed with the recorded bookings, its waits are 2.3x
  shorter than the recorded ones (median 575 s against 1,322 s after the first hour, REPLAY). Absolute waits below
  are optimistic; the comparisons between rules on the same arrivals are what to read.

### The design, in ten lines
1. The agent keeps each job's per-second CPU rate and sends at most 64 values (2 bytes each) with the job's
   result. It also sends its own measured run length, because the server's stamps are wrong for short jobs:
   - 11.3% of jobs show 10 ms or less (MEASURED);
   - a run of a few seconds can show more CPU than 88 cores give in its recorded time (REPLAY: the replay holds
     such runs to 24 cores, the most any run of 10 s or more averaged, 23.3).
2. The agent also records, per host, busy cores and CPU pressure (the kernel's "share of time some task waited for
   a core") for every second. They travel as small arrays inside the existing 5-second heartbeat.
3. For each class the server keeps a profile: the expected CPU at each age of a job (seconds since it started), for
   ages 0 to 128 s in 2-second slots, built from the last 32 runs, plus a flat level after that.
4. The profile's level is scaled to today's median of means, so the p50 policy is kept: the shape comes from the
   curves, the level from the median.
5. For each host the placer adds up, for every 2-second slot of the next 128 s, what its jobs are expected to use at
   their current ages.
6. A job fits a host if, at every slot, that sum plus the job's own profile stays at or under cores x slack (slack
   default 1.1, the existing setting). With all-flat profiles this is today's rule.
7. A job that does not fit stays queued and is retried on the existing triggers (heartbeat change, submit, 5-second
   tick). As running jobs age past their bursts, hosts open up. Queue order stays binding.
8. The measured host CPU enters as a correction that fades in 10 s, so a wrong prediction is noticed within one
   heartbeat. Nothing is killed or throttled.
9. A class uses its curve only while the curve predicts its recent runs better than a flat line does; otherwise it
   is flat, as today.
10. A job is never held on a forecast for more than a set time (default 30 s); after that today's rule alone
    decides.

### What changes where
| Part | Change |
|---|---|
| Agent | A per-job recorder beside `PeakWindow` (no new file read); host busy and pressure per second; its own run length. New flags, off by default. |
| Wire | One optional field on the job result (about 70 B typical, 174 B at most), and per-second host arrays in the heartbeat. Old peers ignore them (precedent: `cpu_peak`). |
| Store | The job record keeps the condensed series (about 40 MB for 7 days of jobs, estimate). `GET /v1/usage` returns it. |
| Placer | A profile per class (about 4.5 KB), a 65-slot table per host, one extra test in the fit check. Memory, disk, GPUs and the queue rules are unchanged. |

### Build steps, each behind a flag and each useful alone
1. **Record.** Per-second series, host busy and pressure, agent run length. No placement change. Alone it answers
   "are jobs phased or flat?" and fixes short-job timing.
2. **Learn, in shadow.** The server builds profiles and the per-class self-check, and logs how well each profile
   predicted each run.
3. **Decide in shadow.** At every placement, log what the curve rule would have done beside what the flat rule did,
   and the predicted against the actual host load. **The replay gate is here** (section 10).
4. **Enforce**, on a named list of hosts first, then all six. Off returns to today's rule at once.
5. Later, only if measurement says so (section 11): a horizon of minutes, credit for jobs about to end, delay on
   purpose.

### Decisions needed
1. **Approve step 1 now.** Recommend yes. It is cheap (no new file read per job), reversible, and the only way to
   learn whether jobs are phased. Cost of no: any later design is guesswork, and short-job timing stays wrong.
2. **Success measure.** Recommend run time over uncontended run time (median and p90), time in system (queue wait
   plus run time), and the share of host-seconds above cores x 1.1. Throughput must not fall by more than 2%. Cost
   of choosing jobs per hour instead: it barely moves at 99% busy (REPLAY: every rule with learned bookings finishes
   within 1% of the same jobs per hour at 4x but one, the limit 0.9 at 7.7% fewer), so nothing would ever pass.
3. **Keep today's flat ceiling beside the curve rule** (the curve rule can only hold a job, never admit more).
   Recommend yes for step 4.
4. **Hold bound: 30 s** (setting). A job can be held at most that long on the forecast. Recommend 30 s.
5. **Gate on the replay, and stop if shape does not pay.** Recommend yes, with the pass mark in section 10. The
   replay now exists and must also beat today's rule at a limit of 1.0, which on flat data gets most of the gain.

## 2. Why this shape

Three designs were compared. All three start with the same recording; they differ in what placement does with it.
- **Direct curves** (laying each class's mean-by-age profile over the host's other jobs): closest to the owner's
  idea; conditioning on age is exact; with flat profiles it equals today's rule. This is the base of this document.
- **Statistical bound** (expected level plus a spread term): needs a variance from 32 runs and an independence
  assumption that jobs placed in the same second break. Deferred. REPLAY: a reserve of twice the root of the summed
  class variances over today's rule cut slowdown from 19.7% to 14.4% at 4x, less than the limit of 1.0 did.
- **Minimal** (host pressure gate plus a 30-second start charge): its strongest point is that measured busy cores
  cannot show overload (line 3 above). That point is adopted: pressure is recorded from step 1.

The outside practice that was looked at (Borg, Autopilot, Kubernetes VPA, Azure Resource Central, Mesos, Bazel,
`make -l`) uses a per-class envelope and a feedback loop on measured host state. No system was found that schedules
short jobs on a per-job curve. That is why the design is gated on data, and why host-level feedback is kept.

How far this goes toward the owner's idea: it covers the first 128 seconds of a job, at 2-second resolution, on every
host. Section 11 says what measurement decides the rest.

## 3. What is recorded, and how

**Per job, on the agent.** READ: `CrunRuntime::read_status` (`crun/mod.rs`) reads the job's `cpu.stat` on every
poll (worker step every 1 s), and `PeakWindow::observe` keeps samples at least 1 s apart. The rates are discarded. A
new recorder beside `PeakWindow` receives the same `(monotonic time, usage_usec)` pairs.
- Rates come from differences over real elapsed time. A gap of k seconds spreads its CPU evenly over k seconds.
- Per second the rate is a 16-bit number in 1/16 core (error under 32 millicores; range 4,096 cores).
- At the end of the run it is condensed: the smallest bucket width among 1, 2, 4, 8, 16 and 32 s that fits the run
  in at most 64 buckets; each bucket is the mean rate. The first 2,048 s are covered, and anything later is one
  "rest of run" mean. Runs of 64 s or less keep full 1-second resolution.
- Also sent: the agent's own run length, and a flag when the series is partial (an adopted run after an agent
  restart, never learned from).
- **Guard:** the server ignores a series whose total CPU differs from the exact `cpu_usage_usec` by more than 5%.

**Per host, on the agent.** The heartbeat carries the cumulative CPU time of the actions control group every 5 s
(READ). New: five per-second values of busy cores and five of `cpu.pressure` "some" in each heartbeat. Whether the
actions group exposes pressure is **UNKNOWN**; one read-only check on one host answers it.

**How it travels.** One optional field on the job result, beside `JobUsage` (which is `Copy`, READ). A receiver
ignores unknown fields; precedent `cpu_peak`, with a test that the previous release reads a result that carries it.

**On the server.** The job record codec (`js-jobs`) and `GET /v1/usage` gain the optional field. The record is kept
7 days, which is also how profiles are rebuilt after a restart.

## 4. What the server learns per class

Class and fallbacks are unchanged (READ: `sizes.rs` `key_under`; class, fallback class, client, cold 22 cores). Per
class, the ring of the last 32 successful runs' series, each resampled to 64 slots of 2 s (ages 0 to 128 s), slots
after a run's end counting 0. From it:
- `p[s]`, the average rate in slot s;
- `n[s]`, the runs still alive at slot s;
- `tail`, the average rate after 128 s over runs that got there (the flat booking if fewer than 4 did);
- `lambda`, the median of the runs' means (today's booking) over the average of them, clamped to 0.5 to 2.

A run under 5 s teaches nothing, as today.

**Self-check.** When a run ends, compare how well the profile predicted its per-slot CPU with a flat line at its
mean. A moving score over 16 runs keeps the curve only while its error is at most 80% of the flat error.

**Cold start and few runs.** No runs: cold, flat 22 cores, as today. Fewer than 8 runs, or fewer than 4 alive at a
given age: the flat booking at that age. About 4.5 KB per class, at most 4,096 profiled classes.

## 5. The placement rule

Per job j on host h, its age slot `i` = floor(age / 2 s); a job not yet seen running is treated as started. Expected
CPU at slot k ahead (cores):

```
if class flat, or N < 8, or n[i] < 4:        E_j(k) = f_j            (today's flat booking)
else with s = i + k:
    s >= 64:                                  E_j(k) = tail
    else:                                     E_j(k) = lambda * p[s] * N / n[i]
then blur: average with neighbours k-1, k+1; cap each slot at host_share (22 cores).
```
Host load and fit, with `cap = cores x slack`:
```
L_h(k) = sum_j E_j(k) + B_h * max(0, 1 - k/5)
B_h    = max(0, measured_busy_h - sum over covered jobs of E_j(0))          // 0 when the reading is stale
fits(n, h)  <=>  for every slot k in 0..64:  L_h(k) + E_n(k) <= cap
```
`measured_busy_h` is the existing reading (READ: `WorkerLoad::measured`, the larger of the last two windows, stale
after 15 s). "Covered" means running at the previous reading. At slot 0 this is today's load rule; with all profiles
flat every slot holds the same number. That equality is the first test. Among hosts where the job fits, the existing
scorer ranks them. The two flat checks that exist (`Room::fits`, and `Capacity::has_room` in `place_claimed`) stay,
so the curve rule can only hold a job that today would be placed. After the 30 s hold bound the job skips the curve
test. Memory, disk and GPUs stay flat bookings.

## 6. Worked example (illustrative numbers, not measured)

The host has 88 threads and slack 1.1, so the cap is 96.8 cores.
- **Resident:** 20 tests, each flat at 2 cores for 60 s (40 cores).
- **Class C:** a 30 s compile at 1 core for ages 0 to 20 s, 20 cores for 20 to 24 s, then 1 core to 30 s. Its mean,
  and so its flat booking, is 3.5 cores.
- **Submitted:** sixteen compiles in the same second.

- **Today's rule:** 40 + 16 x 3.5 = 96 ≤ 96.8, so all 16 are placed at once. At ages 20 to 24 s all 16 burst: 360
  cores, 3.7x the host, while the bookings said 0.99x.
- **Curve rule:** two compiles fit per pass (80 cores at their burst). Each 5-second pass admits two more, whose
  bursts fall after the earlier ones'. All sixteen are placed within about 40 s, and demand peaks near 94.
- **Cost:** the last compile starts up to 35 s later. Whether the stretch saved exceeds the wait added is what the
  replay measures. If the compile is flat in reality, the self-check sets the class flat and all 16 start at once.

## 7. When the prediction is wrong
- **A job runs longer than every recorded run:** it is charged its flat booking until it ends.
- **A job bursts at another time or harder:** the measured busy rises above the expectations, and `B_h` charges the
  difference for 10 s. The profile learns it after the run ends.
- **A class whose curve misfits:** the self-check turns it flat within about 16 runs.
- **Overload busy cores cannot show:** pressure can. Step 1 records it, and a pressure gate is built in shadow only.

**No oscillation, no starvation** (argued, not shown):
- The only action is holding the next job.
- New placements enter the table at once, and the error is recomputed each pass.
- Each slot is capped at 22 cores, below 96.8, so an empty host fits any job.
- Queue order is binding, and after 30 s the curve test is skipped.

The replay shows the same for the rules it runs: every arm finishes every job, and a job no idle host would take
runs alone on an empty host (REPLAY: none did).

## 8. Unknowns that change the design
1. **Learning from held and contended runs.** READ: a run with pressure of 10% of its wall time or more is kept only
   if it exceeds the class's estimate (`sizes.rs`, `js-sizing` `CpuSample`). Runs held by `cpu.max` before the agent
   gave CPU as a weight (komira-ai/jobscheduler#999) had a mean that is a lower bound, and they sit in today's rings
   (section 9, first point). New free runs raise a class once more than half of its last 32 kept runs are free ones;
   under contention they run slower, so their means are biased low too. Step 1 reports the share of clean runs per
   class.
2. **Start-time lag.** The server's idea of a job's age lags the true age by the heartbeat and the container start.
   REPLAY gap: the replay of recorded bookings waits 2.3x less than recorded. The time from placement to start
   holds a booking but is not in the usage export, and is the first suspect; the job store's rows hold it.
3. **Whether jobs are phased at all.** Step 1 answers it. The three-day mean of 1.0 core for runs of 30 s or more
   (REPLAY) does not say jobs are steady: it is mostly held runs.
4. The class count. The replay used booking pairs as stand-in classes, which mix unlike jobs. The store's rows carry
   the labels the real key is made from, and `js-cpu-replay convert --rows` reads them (section 10).

## 9. Where the data disagrees with the draft this document began as
- **The draft said** jobs of 30 s or more had a mean CPU of 1.4 to 2.5 cores, "fits steady use". That figure is
  from runs that were mostly held at their booking.
  - REPLAY over the three days: median 1.0 core (p25 0.7, p75 1.2).
  - REPLAY over the six hours after the limit went: median 4.2 cores (p25 2.8, p75 5.0), from 6,932 runs.
  - The worked example's 3.5-core compile is closer to the free runs than to the history.
- **Bookings are low by about four times for free runs.** Consequence: the 3x load after the median change needs no
  burst at all to explain it. Test: with the rows, compare each live class's booking with its free runs' median. If
  most of the gap is level, step 1 still pays (it measures shape and pressure). But the first gain is the rings
  filling with free runs, which the 1.1 limit and the p50 rule do by themselves.
- **The draft said** 36% of jobs run under 5 s and carry 0.3% of the CPU. MEASURED here: 41% and 0.23%. Jobs of 30 s
  or more carry 96.2% (REPLAY), against 94.6% in the draft. Neither changes the design.
- **The draft said** the existing data cannot replay anything. It replays levels: the rules on identical arrivals,
  with each job's CPU flat over its run. Section 10 has the numbers. It cannot replay shapes until step 1 records
  them.
- **Today's rule at a limit of 1.0 is the alternative to beat.** On flat data it reaches 3.3% slowdown against
  today's 19.7%, at 0.4% fewer jobs per hour. A curve rule earns its machinery only by beating that on recorded
  shapes. (The owner keeps 1.1 for now; this is a measurement, not a proposal.)

## 10. How it is proven

**The replay** (`crates/js-cpu-replay`, kept Rust, no clock or random source).
- **Input.** An export of `GET /v1/usage`, and optionally the job store's rows, becomes an anonymised trace:
  - no job id, host name, label, image or command;
  - times relative to the first arrival;
  - classes as dense indices.
- **Placement.** The trace is replayed on six hosts of 88 threads and 251 GiB:
  - the job service's walk, in arrival order, stopping at the first job no host takes;
  - the placer's sizing, mirrored from `JobSizes` over `js-sizing`;
  - its score (keys 1, 3 and 5 of `js-sched` `Scorer`);
  - host readings every 5 s, taken as the larger of the last two;
  - the memory rule, unchanged.
- **Contention model** (ASSUMED). When a host's jobs ask for more than its cores, each gets cores/demand of what it
  asks and runs longer by that factor. Cache effects and context switching are ignored.
- **Each job's CPU over its run** is bracketed by two shapes. The "blip" is five seconds at its peak and the rest at
  the level that keeps its mean. The "burst" is its peak while its CPU lasts, idle otherwise. Without a recorded
  peak both are flat.
- **Arms** (`report::ARMS`):
  - today's rule with the recorded bookings;
  - today's rule with learned bookings at limits 1.1, 1.0 and 0.9;
  - a variance reserve;
  - measured load alone at 1.0 and 1.1;
  - a perfect per-job curve at 1.0 and 1.1, the upper bound of any curve rule.
- **Speeds.** Each arm runs at the recorded arrival speed and at 4x, which reaches saturation.

**The window.** The jobs that started from 16:00 UTC on 2026-11-22, the first hour in which no job ran above its
booking (REPLAY trace: 14,140 jobs, 5.9 h, 19.8% of the farm's cores on average). Before then every job ran under a
`cpu.max` of its booking, so its CPU is a lower bound.

**First check, on the replay itself: not yet met.**
- Replayed with the recorded bookings, the median wait is 505 s against a recorded 1,232 s; after the first hour,
  575 s against 1,322 s (REPLAY).
- The time from placement to start is not in the usage export (section 8, point 2). The rows carry it, and the
  replay holds a job's booking through it (`start_ms`).
- A replayed job is placed at the next one-second tick, which adds up to a second to each wait.
- Until the check holds within 20%, read the differences between arms, not the absolute waits.

**Results on that window, jobs flat over their runs** (REPLAY, `farm.report.md`, 4x arrivals). Slowdown is the time
lost to contention over the jobs' own time.

| rule | jobs/h | wait p50 / p99 s | jobs per host | host-time over cores | slowdown |
|---|---|---|---|---|---|
| today, learned bookings, 1.1 | 9,192 | 369 / 570 | 24.5 | 50.8% | 19.7% |
| today, 1.0 | 9,152 | 526 / 849 | 21.1 | 15.8% | 3.3% |
| today, 0.9 | 8,481 | 656 / 1,228 | 19.3 | 6.1% | 2.3% |
| reserve z=2, 1.1 | 9,189 | 386 / 629 | 23.4 | 43.5% | 14.4% |
| measured load only, 1.0 | 9,158 | 507 / 800 | 21.7 | 19.5% | 6.4% |
| measured load only, 1.1 | 9,192 | 301 / 510 | 32.2 | 63.0% | 57.6% |
| perfect curve, 1.0 | 9,211 | 434 / 689 | 20.5 | 0% | 0% |
| perfect curve, 1.1 | 9,217 | 380 / 624 | 21.6 | 61.4% | 4.9% |

At the recorded speed every learned rule waits a median of 1 s, and today's rule loses 15.7% to contention against
0% for the perfect curve. Three points to read from the table:
- Measured load alone at 1.1 is the worst arm: the measurement lags and cannot read above the cores.
- The perfect curve's gain over today's rule at 1.0 is small with flat jobs; bursts are what could make it large.
- Today's rule in the replay holds 24.5 jobs per host against 30 to 36 live. Its bookings are learned from the
  window's own free runs, not from the held history, which agrees with section 9.

**What the replay needs next, in order:**
1. The job store's rows for the window (`convert --rows`), for real classes, five-second peaks, contention and start
   times. Then the burst bracket is filled in, and the first check is run again.
2. The per-second series from step 1. Then the curve rule itself, with learned profiles, is an arm; its per-class
   curves would be learned from the first two days and replayed on a held-out third.

**Pass mark** (the owner sets it):
- The curve rule beats today's rule on the share above 1.1x and on stretch, by more than the spread between two
  held-out days.
- It also beats today's rule at a limit of 1.0, the cheap alternative.
- Throughput is no more than 2% lower, and queue-wait p90 and p99 are no worse.
- The perfect curve clearly beats today's rule.
- If today's rule at 1.0 gets most of the gain, that is the cheaper change.

**Kept Rust tests asserting behaviour** (injected clock, no sleeps), for the curve rule when it is built:
- all-flat profiles give exactly the flat rule's decisions;
- a burst-first and a quiet-first job are admitted together where two burst-first jobs are not;
- a job older than every recorded run is charged flat;
- an empty host fits any job, and the flat rule admits a job held 30 s;
- a series whose area is off by more than 5% is ignored;
- `B_h` is never negative and fades;
- the previous release reads a result that carries the series, and the reverse.

## 11. Full version, and the measurements that decide
- **A horizon of minutes.** Decided by the share of CPU time, in jobs over 60 s, spent after 128 s in bursts at least
  twice the job's mean and lasting 3 s or more. If it is under about 20% (ASSUMED threshold), 128 s is enough.
- **Credit for jobs about to end.** Decided by whether the step 3 shadow shows the flat sum over-counting hosts.
- **Delay on purpose.** Decided by the perfect-curve arm against the 5-second-tick version of the curve rule.
- **A spread term.** Decided by step 3's calibration.

## 12. Mac hosts
READ: the Mac runtime has no `usage` method, so a Mac job's result carries no CPU time at all today. Mac classes have
no series and stay flat under the existing fallbacks until the Linux replay shows a gain.

## 13. Settings (flags and files; no environment variables)
| Name | Default | Meaning |
|---|---|---|
| agent `--cpu-series` | off, then on after both sides ship | record and send the per-job series |
| agent `--host-second-samples` | off, then on | per-second host busy and pressure in the heartbeat |
| server `--placement-curves` | `off` (`off`, `shadow`, `on`) | learn and decide with curves |
| `--curve-hosts` | all when `on` | file listing the hosts that use the rule |
| `--curve-horizon-seconds` | 128 | look-ahead |
| `--curve-slot-seconds` | 2 | slot width |
| `--curve-min-runs` | 8 | runs needed before a shape is used |
| `--curve-fit-ratio` | 0.8 | profile error over flat error to keep a curve |
| `--curve-bias-fade-seconds` | 10 | fade of the measured correction |
| `--curve-max-hold-seconds` | 30 | longest a job is held on the forecast |
| existing CPU slack | 1.1 | cap = cores x slack (unchanged) |

Off equals today's behaviour exactly.

## 14. Considered and not chosen
- **A burst reserve per host from the classes' spread.** REPLAY: less gain than today's rule at a limit of 1.0. Kept
  as a later setting on recorded data.
- **Measured host load as the only rule.** REPLAY: at 1.1 the worst arm, at 1.0 behind today's rule at 1.0. The
  measurement lags placements and cannot read above the cores.
- **A pressure gate and a 30-second start charge only.** The lightest design. Not chosen as the base because it
  drops the owner's idea before there is data; its gate is built in shadow.
- **Sampled recording.** It starves the rings of rare classes.
- **The existing minute-slot admission in `js-sched`** (`admission/horizon.rs`). It is unused by jobs and keyed by
  lease clocks.
- **A one-second heartbeat.** It multiplies placement passes.
- **CPU quotas or cpusets.** These contradict the owner's ruling.
- **Memory curves.** No per-job memory limit, by ruling.
