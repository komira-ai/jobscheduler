# Scheduling on CPU use over time

Status: design for review. Nothing here is built except the replay (`crates/js-cpu-replay`). Companion to
[sizing.md](sizing.md) (how a class's CPU is booked) and [placement-by-load.md](placement-by-load.md) (the
load rule and the time profiles designed for leases, never wired to jobs).

Labels: **READ**, read in the code. **REPLAY**, printed by `js-cpu-replay` from a trace in this pull request and
pinned by `tests/farm.rs` (the three-day `export.trace.gz`, and `farm.trace.gz` giving `farm.report.md`, the hours
with no CPU limit). **MEASURED**, counted from the export or the farm's metrics, with no test pinning it.
**ASSUMED**, a choice not yet tested. **UNKNOWN**, with the build step that measures it.

## 1. First page

### The problem, in five lines
1. A job's CPU booking is the median of its class's last 32 runs' mean CPU (CPU time over wall time,
   komira-ai/jobscheduler#1026). A class is a job type, for example the compile of one target. MEASURED in the first
   8 minutes after the change, against the hour before: jobs per host 3 to 4 became 30 to 36; hosts about 20% busy
   became about 99%; jobs finished per minute 58 became 128; median queue wait 15 min became 4 min; median run time
   32 s became 65 s; load average on an 88-thread host 13 to 26 became 250 to 330.

   That was a queue of waiting jobs released at once. MEASURED 25 minutes after the change, with the backlog
   drained: load 78 to 113 on 88 threads, 14 to 20 jobs per host, and jobs waiting for a CPU 11 to 15% of the time
   (the kernel's pressure "some" over 60 s). The overshoot is mostly at bursts of arrivals, not a steady 3x.
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
- **Size of the prize, against the owner's 20%.** At steady load, jobs waited for a CPU 11 to 15% of the time
  (MEASURED). On the six hours replayed with jobs flat over their runs, a perfect per-job curve cuts mean time in
  system (wait plus run) from 65.1 s to 59.5 s, 8.6% (REPLAY, recorded arrival speed). At saturation, the replay's
  sharing loses no CPU to overcommit, so there the rules that overcommit most finish soonest. The rest of the 20% can
  only come from what the model leaves out: cache and switching cost at 3x, and bursts the flat data hides. Step 1
  measures both (section 10).
- The replay's own check is not yet met (section 10). Replayed with the recorded bookings, its waits are 2.3x
  shorter than the recorded ones (median 575 s against 1,322 s after the first hour, REPLAY). Absolute waits below
  are optimistic; the comparisons between rules on the same arrivals are what to read.

### The design, in ten lines
1. The agent keeps each job's per-second CPU rate as a series of 128 points whose granularity doubles, by the
   mean, each time it fills (section 3), and sends it with the job's result. It also sends its own measured run length, because the server's stamps are wrong for short jobs:
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
   heartbeat. Nothing is ever killed, throttled or moved because an estimate was wrong: a host that turns out
   overcommitted runs overcommitted until its jobs finish (the owner's ruling; heavy overcommit is an accepted
   fallback, not an emergency). Only a job that has not started is ever held.
9. A class uses its curve only while the curve predicts its recent runs better than a flat line does; otherwise it
   is flat, as today.
10. A job is never held on a forecast for more than a set time (default 30 s); after that today's rule alone
    decides.

### What changes where
| Part | Change |
|---|---|
| Agent | A per-job recorder beside `PeakWindow` (no new file read); host busy and pressure per second; its own run length. New flags, off by default. |
| Wire | One optional field on the job result (258 B, or 514 B for a job over 128 s if the variant of section 3 is chosen), and per-second host arrays in the heartbeat. Old peers ignore them (precedent: `cpu_peak`). |
| Store | The job record keeps the series (about 160 MB for 7 days of jobs at 128 a minute, estimate). `GET /v1/usage` returns it. |
| Placer | A profile per class (about 4.5 KB), a 65-slot table per host, one extra test in the fit check. Memory, disk, GPUs and the queue rules are unchanged. |

### Build steps, each behind a flag and each useful alone
1. **Record.** Per-second series, host busy and pressure, agent run length; no placement change. Alone it says
   whether jobs are phased, sizes the prize (decision 2) and fixes short-job timing.
2. **Learn, in shadow.** Per-class distributions and the self-check, logged against each run.
3. **Decide in shadow.** Log what the curve rule would have done beside the flat rule. **The replay gate is here.**
4. **Enforce**, on named hosts first, then all six. Off returns to today's rule at once.
5. Later, only if measured worth it (section 11): a horizon of minutes, credit for jobs about to end.

### Decisions needed
1. **Approve step 1 now.** Recommend yes. It is cheap (no new file read per job), reversible, and the only way to
   learn whether jobs are phased. Cost of no: any later design is guesswork, and short-job timing stays wrong.
2. **Success measure: total time**, queue wait plus run time summed over a build's actions (the replay's "time in
   system"). Pass mark: 10% less total time than today's rule on recorded series (half the owner's expected 20%),
   throughput within 2%. The machinery of steps 2 to 4 is worth weeks only at that size; if step 1's data bounds the
   prize below it, stop at step 1.
3. **Keep today's flat ceiling beside the curve rule** (the curve rule can only hold a job, never admit more).
   Recommend yes for step 4. Since the overshoot comes with bursts of arrivals, the curve rule's value is in
   spreading a burst of starts. At steady load the flat ceiling already holds pressure near 11 to 15%.
4. **Hold bound: 30 s** (setting). A job can be held at most that long on the forecast. Recommend 30 s. A burst of
   hundreds of arrivals is released over several passes, so the bound must cover the spreading of one burst:
   about 30 s in the worked example (section 6). A shorter bound gives the burst back to today's rule.
5. **Gate on the replay, and stop if shape does not pay.** Recommend yes, with the pass mark of decision 2. On flat
   data a lower limit is not the cheap answer: at 1.0 it cuts slowdown but adds more wait than it saves (section 10).

## 2. Why this shape

Three designs were compared. All three start with the same recording; they differ in what placement does with it.
- **Direct curves** (laying each class's mean-by-age profile over the host's other jobs): closest to the owner's
  idea; conditioning on age is exact; with flat profiles it equals today's rule. This is the base of this document.
- **Statistical bound** (expected level plus a spread term): it assumes jobs are independent, which jobs placed in
  the same second are not. Deferred. REPLAY: 2% less total time than today's rule at the recorded speed, 6% more at
  4x.
- **Minimal** (host pressure gate plus a 30-second start charge): its strongest point is that measured busy cores
  cannot show overload (line 3 above). That point is adopted: pressure is recorded from step 1.

### What others do
Read from the papers' and projects' own text, except where marked "abstract only" or "unverified".
- **For long recurring jobs, a curve over time is established practice.**
  - **Morpheus** (Microsoft, OSDI 2016): over 60% of jobs in the busiest clusters recur. A linear program fits one
    "skyline" (resources at each step, "typically one minute") to about a month of runs, and reserves it for every
    later run; a run that falls behind gets extra resources a minute at a time.
  - **Rayon** (Microsoft, SoCC 2014; in Apache Hadoop 2.6): production jobs reserve resources that vary over time; a
    planner places the reservations ahead, and capacity a reservation leaves idle goes to best-effort jobs.
  - Both deal in jobs of minutes to hours, at a minute's resolution, reserved before the job runs.
- **For short tasks, a level from history plus feedback on the host.**
  - **Borg** (EuroSys 2015, section 5.5): a task's reservation "is set equal to the resource request (the limit);
    after 300 s, to allow for startup transients, it decays slowly towards the actual usage plus a safety margin",
    and "is rapidly increased if the usage exceeds it". Non-prod work is placed on reservations. When they are
    wrong, Borg will "kill or throttle nonprod tasks, never prod ones". About 20% of a median cell's work runs in
    reclaimed resources.
  - **Autopilot** (EuroSys 2020, section 3.2):
    - samples: 1 s, kept as a histogram of about 400 buckets per 5-minute window;
    - decay: weights 2^(-age / half-life), "tuned to long-running jobs: we use a 12 hour half life for the CPU
      signal";
    - batch CPU: the weighted mean, "since if a job tolerates CPU throttling, the most efficient limit for the
      infrastructure is the job's average load";
    - serving CPU: the 90th or 95th percentile of load-weighted usage;
    - then a 10 to 15% margin, and the maximum over the last hour.
  - **What carries over to jobs of seconds:** the per-second sample, a decayed distribution per class, the mean for
    batch CPU, and load weighting.
  - **What does not:**
    - Borg's 300 s startup grace: 92.5% of our jobs end within 128 s.
    - Autopilot's 12 h half-life and hour of smoothing: our classes run many times an hour.
    - Their remedy for a wrong estimate (throttle or kill): the owner rules it out, so here a wrong estimate only
      overcommits a host.
  - **Kubernetes VPA, Azure Resource Central:** per-class histograms from history, no per-task curve (documentation
    and abstracts only; their percentile targets are unverified).
  - **Mesos oversubscription:** it stops offering revocable resources when the host is loaded, then evicts.
  - **`make -l`:** a lagging load average stalls or oscillates.
  - No system was found that schedules jobs of seconds on a per-job curve; that is a negative search, not proof.
- **How usage over time is stored.**
  - **RRDtool:** a fixed-size file of round-robin archives at several resolutions. Each consolidates its points by
    AVERAGE, MIN, MAX or LAST, and an operator keeps an AVERAGE and a MAX archive side by side to see both level and
    spikes.
  - **Google's cluster traces:** one record per task per 5-minute period. The 2011 trace adds one randomly picked
    1-second sample of CPU. The 2019 trace adds a 21-bucket histogram of CPU per period, biased to high
    percentiles; its paper measures slack against the peak in each 5-minute sample.
- **What it says here.**
  - The owner's doubling series is RRDtool's AVERAGE archive with one resolution that coarsens as the job ages.
  - Keeping the five-second peak beside it is the MAX archive.
  - For long jobs the design follows Morpheus and Rayon.
  - For jobs of seconds it goes beyond published practice, which is why it is gated on the replay, and why the
    host-level feedback (`B_h`, pressure) is kept.

How far this goes toward the owner's idea: it covers the first 128 seconds of a job, at 2-second resolution, on every
host. Section 11 says what measurement decides the rest.

## 3. What is recorded, and how

**Per job, on the agent.** READ: `CrunRuntime::read_status` (`crun/mod.rs`) reads the job's `cpu.stat` on every
poll (worker step every 1 s), and `PeakWindow::observe` keeps samples at least 1 s apart. The rates are discarded. A
new recorder beside `PeakWindow` receives the same `(monotonic time, usage_usec)` pairs.
- Rates come from differences over real elapsed time. A gap of k seconds spreads its CPU evenly over k seconds.
- A point is a 16-bit number in 1/16 core (error under 32 millicores; range 4,096 cores).
- **The series (the owner's scheme).** One series per job of at most 128 points, and one unsigned integer, its
  granularity in seconds per point. It starts at 1 s a point. When all 128 points are full, adjacent points are
  combined in pairs by their **mean** (never the max, so long runs are not booked at their spikes) and the
  granularity doubles: 128 s at 1 s, 256 s at 2 s, 512 s at 4 s, and so on. The size is fixed at 128 x 2 B plus the
  granularity; no part of a long run is flattened to one number. The five-second peak stays as its own number.
- **Consequences.** A class's profile combines runs stored at different granularities: each run is spread evenly
  over its points before it is resampled to the profile's 2 s slots, so the coarsest run sets how much detail a
  slot really has. For a long job a burst shorter than a point is averaged away on purpose; the host's measured
  reading (`B_h`, section 5) is what catches it.
- **Open question for the owner: keep the first 128 s at 1 s for every job**, and use the doubling only for the part
  after it (a second series of 128 points with its own granularity, sent only by a job that ran longer than 128 s).
  - **Why it matters:** the start of a job is what the placer forecasts when it admits it (its look-ahead is 128 s).
    Under the doubling alone, the start of every job longer than 128 s is smeared to 2 s or more a point.
  - **What the data says** (MEASURED on the committed six-hour trace, `farm.trace.gz`): 92.5% of jobs, with 66.6%
    of the CPU, end within 128 s and are the same under both schemes. The other 7.5% carry 33.4% of the CPU, and
    their starts would be stored at 2 s (12.1% of the CPU), 4 s (10.9%), 8 s (9.3%) or 16 s (1.1%) a point.
  - **Cost:** 258 B a job under the doubling alone; with the variant 514 B for a long job and 258 B for the rest,
    about 277 B on average, 7% more.
  - **Recommendation: the variant.** It keeps the forecast at admission exact for a third of the CPU at 7% more
    bytes, and the profile's first 128 s never mixes granularities.
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
Class and fallbacks are unchanged (READ: `sizes.rs` `key_under`: class, fallback class, client, cold 22 cores).

**The estimate is a distribution, not a number.**
- **Samples.** Every point of a run's series (section 3) is a sample of its class's per-second CPU. A point of
  granularity g counts g times.
- **Decay.** A run's samples are weighted 2^(-age / 6 h) by the run's end, with at most the last 64 runs.
  - 6 h, not Autopilot's 12 h, because a class here runs tens of times an hour and changes with a commit. Over a
    working day a run's weight falls to 1/4.
  - REPLAY: as a whole-run booking, a decayed mean with a 1 h and with a 12 h half-life gave the same total time
    within 0.4% (64.5 s and 64.8 s), so the choice is not sensitive.
- **Kept per class:**
  - a histogram of all its samples, in 64 log-spaced buckets from 1/16 core to 88 cores;
  - for each 2 s slot of ages 0 to 128 s, the decayed mean and the decayed load-weighted 90th percentile;
  - after 128 s, the decayed mean of its points.

**What placement takes from it, and for what:**
- **The level:** the decayed mean of the samples (Autopilot's rule for batch CPU). It is the charge of a job beyond
  its first 128 s, and of a class with no shape. REPLAY: as a whole-run booking it matches today's median in total
  time (64.8 s against 65.1 s).
- **The start:** for each 2 s slot of the first 128 s, the load-weighted 90th percentile. This is what a new job is
  checked against at admission, the only moment placement acts, and so the only place a high quantile pays.
  REPLAY: used for a whole run, the load-weighted 90th percentile packs far less (2.1% slowdown, against 15.7%) but
  adds 2.5% total time, so it is kept to the start.
- **The tail:** the decayed mean after 128 s. A burst shorter than a point is averaged away, and the host reading
  (`B_h`) catches it.
- **Today's median stays the flat ceiling** (decision 3). The distribution feeds only the curve rule.

**A wrong estimate.** It is never acted on against a running job. It overcommits its host until the jobs end, and it
teaches the class: a run slowed by contention may only raise it, as today.

**Self-check, cold start.** A class keeps its shape only while its slots predict its runs better than a flat line
(error at most 80% of the flat one, over 16 runs). With no runs it is cold (flat 22 cores, as today); with fewer than
8 runs, it is flat.

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
An 88-thread host (cap 96.8) runs 20 flat tests at 2 cores (40 cores). Sixteen 30 s compiles arrive in the same
second, each 1 core for ages 0 to 20 s, 20 cores for 20 to 24 s, then 1 core, so each books its mean of 3.5 cores.
- **Today's rule** places all 16 at once (40 + 56 = 96). At ages 20 to 24 s they all burst: 360 cores, 3.7x the host.
- **The curve rule** places two per 5-second pass, their bursts falling one after another. All 16 are placed within
  about 40 s and demand peaks near 94. The last starts up to 35 s later; the replay measures whether that pays. If
  the compile is flat in reality, the self-check sets the class flat and all 16 start at once.

## 7. When the prediction is wrong
Nothing running is ever killed, throttled or moved: the host runs overcommitted until its jobs end, which the owner
accepts. What corrects is the next placement:
- **A job runs longer than every recorded run, or bursts at another time:** it is charged flat; the measured busy
  above the expectations enters `B_h` for 10 s; the profile learns it when the run ends.
- **A class whose curve misfits:** the self-check turns it flat within about 16 runs.
- **No oscillation, no starvation** (argued, not shown): only the next job is held; an empty host fits any job;
  queue order binds; after 30 s the curve test is skipped. In the replay every arm finishes every job.

## 8. Unknowns that change the design
1. **Learning from held and contended runs.** A run with pressure of 10% of its wall time or more only raises its
   class (READ: `sizes.rs`, `js-sizing` `CpuSample`). Runs held by `cpu.max` before komira-ai/jobscheduler#999 sit in
   today's rings with means that are lower bounds (section 9). Step 1 reports the share of clean runs per class.
2. **Start-time lag.** The time from placement to start holds a booking but is not in the usage export; it is the
   first suspect for the replay's 2.3x gap in waits (REPLAY), and the job store's rows hold it.
3. **Whether jobs are phased at all.** Step 1 answers it. The three-day 1.0 core (REPLAY) is mostly held runs.
4. **The class count.** The replay's stand-in classes are booking pairs; `convert --rows` reads the real keys.

## 9. Where the data disagrees with the draft this document began as
- **The draft said** jobs of 30 s or more averaged 1.4 to 2.5 cores, "fits steady use". REPLAY: a median of 1.0 core
  (p25 0.7, p75 1.2) over the three days, mostly runs held at their booking. In the six hours with no limit it was
  4.2 cores (p25 2.8, p75 5.0, 6,932 runs).
  - So bookings are about four times low for free runs, and the 3x load after the median change needs no burst to
    explain it.
  - Test with the rows: compare each live class's booking with its free runs' median.
- **The draft said** 36% of jobs run under 5 s with 0.3% of the CPU. MEASURED here: 41% and 0.23%. Jobs of 30 s or
  more carry 96.2% (REPLAY), not 94.6%.
- **The draft said** the existing data cannot replay anything. It replays levels (section 10); shapes need step 1.

## 10. How it is proven

**The replay** (`crates/js-cpu-replay`, kept Rust, no clock or random source).
- It turns an export of `GET /v1/usage`, and optionally the store's rows, into an anonymised trace.
- It replays the trace on six hosts of 88 threads and 251 GiB, with:
  - the job service's walk, in arrival order;
  - the placer's sizing (mirrored from `JobSizes` over `js-sizing`) and its score (keys 1, 3, 5);
  - host readings every 5 s;
  - the memory rule.
- **Contention** (ASSUMED): a host asked for more than its cores gives each job cores/demand of what it asks, and the
  job runs longer by that factor. Nothing is lost to cache or switching.
- **Shapes:** each job's CPU is bracketed by a "blip" (five seconds at its peak) and a "burst" (its peak while its
  CPU lasts). Without a recorded peak both are flat.
- **Arms:** listed in the table below. Each runs at the recorded arrival speed and at 4x.

**The window.** The jobs that started from 16:00 UTC on 2026-11-22, the first hour in which no job ran above its
booking (REPLAY trace: 14,140 jobs, 5.9 h, 19.8% of the farm's cores on average). Before then every job ran under a
`cpu.max` of its booking, so its CPU is a lower bound.

**First check, on the replay itself: not yet met.** Replayed with the recorded bookings, the median wait is 575 s
against 1,322 s recorded after the first hour (REPLAY). The time from placement to start is not in the usage export
(section 8); the rows carry it. A replayed job also waits up to a second for its tick. Until the check holds within
20%, read the differences between rules, not absolute waits.

**Results on that window, jobs flat over their runs** (REPLAY, `farm.report.md`).
- Time in system is mean wait plus run, in seconds.
- Slowdown is the time lost to contention, over the jobs' own time.
- 1x is the recorded arrival speed; 4x reaches saturation.

| rule | time in system 1x | 4x | slowdown 1x | 4x | jobs/h 4x |
|---|---|---|---|---|---|
| today: median, 1.1 | 65.1 | 351.3 | 15.7% | 19.7% | 9,192 |
| median, 1.0 | 66.9 | 494.6 | 10.4% | 3.3% | 9,152 |
| median, 0.9 | 68.7 | 740.4 | 5.5% | 2.3% | 8,481 |
| decayed mean (12 h), 1.1 | 64.8 | 339.5 | 13.1% | 21.6% | 9,195 |
| decayed mean (1 h), 1.1 | 64.5 | 337.6 | 12.5% | 22.0% | 9,187 |
| load-weighted p90 (1 h), 1.1 | 66.7 | 563.8 | 2.1% | 4.5% | 8,878 |
| reserve z=2, 1.1 | 63.7 | 372.3 | 10.4% | 14.4% | 9,189 |
| measured load only, 1.0 | 66.1 | 473.0 | 17.8% | 6.4% | 9,158 |
| measured load only, 1.1 | 66.5 | 318.3 | 28.0% | 57.6% | 9,192 |
| perfect curve, 1.0 | 59.5 | 387.8 | 0% | 0% | 9,211 |
| perfect curve, 1.1 | 58.7 | 347.9 | 1.7% | 4.9% | 9,217 |

What to read from the table:
- **At the recorded speed only the perfect curve clearly wins**: 8.6% less total time than today. Lower limits and
  higher quantiles cut slowdown but add more wait than they save.
- **At saturation the model rewards overcommit.** Its sharing loses no CPU, so measured load at 1.1 finishes
  soonest despite 57.6% slowdown. That is the owner's view of heavy overcommit, and it is also the model's limit:
  any cost of overcommit beyond slowdown (cache, switching, memory bandwidth) is not in it.
- **Fewer jobs per host than live.** Today's rule in the replay holds 24.5 jobs per host against 30 to 36 live,
  because its bookings come from free runs only (section 9).

**Next:** the job store's rows (`convert --rows`: real classes, peaks, contention, start times), then step 1's
series, with curves learned on two days and replayed on a held-out third.

**Pass mark** (decision 2): on held-out recorded days the curve rule takes 10% or more off total time against today's
rule, by more than the spread between two days, with throughput within 2%; and the perfect curve does better still.

**Kept Rust tests asserting behaviour** (injected clock, no sleeps), for the curve rule when it is built: flat profiles
give the flat rule's decisions; a burst-first and a quiet-first job fit where two burst-first jobs do not; a job older
than every recorded run is charged flat; an empty host fits any job, and a job held 30 s is admitted by the flat rule;
a series off by more than 5% is ignored; `B_h` is never negative and fades; each release reads the other's results.

## 11. Full version, and the measurements that decide
- **A horizon of minutes.** Decided by the share of CPU time, in jobs over 60 s, spent after 128 s in bursts at least
  twice the job's mean and lasting 3 s or more. If it is under about 20% (ASSUMED threshold), 128 s is enough.
- **Credit for jobs about to end.** Decided by whether the step 3 shadow shows the flat sum over-counting hosts.
- **Delay on purpose** (a job not yet started), decided by the perfect-curve arm; **a spread term**, by step 3.

## 12. Mac hosts, settings
- **Mac hosts:** the Mac runtime has no `usage` method (READ), so Mac classes stay flat until Linux shows a gain.
- **Settings** (flags, each off or at today's value by default): agent `--cpu-series`, `--host-second-samples`;
  server `--placement-curves off|shadow|on`, `--curve-hosts`, `--curve-horizon-seconds 128`, `--curve-slot-seconds
  2`, `--curve-min-runs 8`, `--curve-fit-ratio 0.8`, `--curve-bias-fade-seconds 10`, `--curve-max-hold-seconds 30`
  (held jobs are only ones not yet started); the CPU slack stays 1.1. Off equals today's behaviour exactly.

## 14. Considered and not chosen
- **A reserve from the classes' spread:** 2% less total time than today's rule at the recorded speed, 6% more at 4x
  (REPLAY).
- **Measured host load alone:** it lags and cannot read above the cores.
- **A pressure gate and a start charge only:** the lightest design; it drops the owner's idea before there is data.
  Its gate is built in shadow.
- **Others:** sampled recording (starves rare classes); `js-sched`'s minute slots (unused by jobs); a one-second
  heartbeat (more passes); CPU quotas or cpusets and memory curves (against the owner's rulings).
- **Killing, throttling or moving a running job on a wrong estimate:** ruled out by the owner.
