# Booking memory from the learned profiles, without killing a job on a wrong estimate

Status: design for review. Nothing on this page is built except its step 2, the memory shadow
(komira-ai/jobscheduler#1254). It follows [placement-profiles-live.md](placement-profiles-live.md), whose
`--placement-curves on` books CPU by curve on named hosts and keeps memory on one flat number in every mode (its
question 4), and whose section 2, "Memory over time, in shadow", projects memory by curve and books nothing. The
owner has since asked that placement use the memory profiles too. This page says how, and what keeps a host safe
when a memory estimate is wrong.
Companions: [sizing.md](sizing.md) (today's flat memory booking), [cpu-over-time.md](cpu-over-time.md) (the
profiles) and [placement-by-load.md](placement-by-load.md) (js-sched's load test, whose flat memory guard this page
replaces on the hosts it names and nowhere else).

Code references are `path:line` on main after komira-ai/jobscheduler#1249. Labels: **READ**, read in the code there.
**MEASURED**, a figure measured on the farm, named where it is used. **INFERRED**, reasoned, not run. **DECIDES**, a
number not yet measured that settles a choice. The memory shadow's counts on the farm measure each one.

## 1. First page

### What is asked
Place jobs by their memory profiles, not only by one flat memory number per job. Keep it simple. Add no new state
and no new race to the start path. Nothing kills, throttles or moves a running job on a wrong estimate.

### Where things stand (READ, MEASURED)
- **Memory is booked flat in every mode.** A job books the 95th percentile of its class's last 32 peaks, x1.2 +
  128 MiB, rounded up to 256 MiB, at least 1 GiB. It is never below the class's held samples or its kill floor
  (`crates/js-sizing/src/summary.rs:525-535`, [sizing.md](sizing.md)).
- **The profile is learned and projected, not booked.** Under `shadow` and `on` the service learns a memory
  profile per class. The memory shadow turns it into a class's memory curve (`MemoryCurve::of`,
  `crates/js-sizing/src/memory_curve.rs:32`): per 2 s slot the load-weighted 90th percentile of the slot's largest
  second, and from 128 s on the 90th percentile of the runs' peaks after 128 s. Each slot carries the flat margin
  and is held to at most flat. A class has no curve until it has scored 16 runs.
  - The shadow projects each host per slot (`crates/js-sched-placer/src/memory_shadow.rs`).
  - It counts each placement `same`, `sooner` or `later`.
  - It compares each heartbeat reading with the projection.
  - Nothing that books or places a job reads it.
- **A job has no memory limit of its own.** Since komira-ai/jobscheduler#1027 the agent's leases (`TrackedMemory`)
  get no `memory.max`, `memory.high` or `memory.swap.max` (`crates/js-contain/src/cgroup.rs:313-315`). They get
  `memory.oom.group`, so a kernel kill takes the whole job (`cgroup.rs:306`). The only memory bound is the booking
  test against `memory_schedulable_percent` (rule B14). A job above its booking runs on. Its class books higher
  next time.
- **A host short of memory is not seen.**
  - The agent tells a kill for host memory from a kill for the lease's own limit (`OomOutcome::BusyNode`,
    `cgroup.rs:413-428`). It still reports the host kill as an ordinary exit
    (`crates/js-agent/src/crun/spec.rs:226-241`).
  - js-sched has a requeue for such a kill, `Failure::NodeMemoryPressure`
    (`crates/js-sched/src/scheduler/memory.rs:798-799`). Nothing produces it today.
  - The heartbeat carries the host's non-file memory (`crates/js-agent/src/usage/host.rs:62-86`) but not its swap in
    use. A job's own swap peak is reported at its end.
  - Nothing in this repository sets up swap on a host.
- **The farm is full by memory booking (MEASURED, one hour of 10,288 jobs read with `js usage`).**
  - Memory booked was 1.45x the peak used: 3,577,691 GiB-s against 2,473,172.
  - Per job (runs of 5 s or more), booked over peak used was p10 1.2, p50 1.7 and p90 9.7.
  - The six Linux hosts (88 cores, 252 GiB each) reached 97% booked memory and 94% booked CPU in their peak minutes.
  - The median wait for a host was 142 s.
  - The long 11 to 12.75 GiB classes hold about half the booked memory-time, at 1.2 to 1.4x their peak. 891 jobs at
    4 cores and 16 GiB hold 10.5%, at 3.0x their peak.
- **Few classes have a shape (MEASURED, komira-ai/jobscheduler#1077, 4 h of check lines).** 38 of 330 classes with
  16 lines (11.5%) had a memory profile whose error was at most 80% of the flat line's.
- **What the recorded peaks say (MEASURED, komira-ai/jobscheduler#1254, one hour of usage records, no memory
  series).**
  - Runs past 128 s hold 76.6% of the booked memory-time.
  - A tail booking from peaks could give back at most 5.3% of all booked memory-time.
  - With classes approximated by their booking pair, a projection from history was too low for 18.4% of the long
    runs judged (9 of 49), once classes that often overran were left out. It was short by 2,418 MiB at the median.
    That figure is INFERRED, and the proxy overstates it.
- **The shadow's counts on the farm do not exist yet.** They need a release with komira-ai/jobscheduler#1254
  running `shadow` for two working days. Each choice below names the count that settles it.

### Likely outcome (INFERRED)
- **The gain is small.** It is the memory that running jobs no longer hold once their peak has passed, and queued
  jobs still book flat.
  - From the tails, the recorded peaks allow at most 5.3% of booked memory-time.
  - The rest would come from the first 128 s of runs, which only the shadow's counts on the farm can show.
- **The 3x bookings of the 4-core, 16 GiB jobs** are a sizing matter for [sizing.md](sizing.md). Fixing them needs
  no curve and risks no kill.
- **So the go bar of section 6 may not be met.** Step 1 is worth building anyway, and step 2 is built. Step 4
  waits on step 3's counts in shadow.

### Decisions needed
| # | Question | Recommend |
|---|---|---|
| 1 | What a running job is charged | The remaining maximum of the shadow's memory curve: the largest of its slot bookings from the job's age on. It never rises and is never above flat. A queued job books flat (section 2) |
| 2 | The margin, and how it is learned | The curve's own margin, x1.2 + 128 MiB on the class's 90th percentile, not tuned per class. A class learns whether it may use its curve, from a held-out check (sections 2, 4) |
| 3 | What protects a host when real memory passes the projection | No per-job limit (as today). A measured floor. `memory_schedulable_percent` 95 as the level where the host stops taking jobs. 32 GiB of encrypted swap with zswap as the cushion. The scheduler never kills, throttles or moves a job (section 3) |
| 4 | Fallback to flat | Five checks. Any failure books flat at the next pass (section 4) |
| 5 | Canary | `--memory-curve-hosts` (file key `server.memory_curve_hosts`), named hosts, a subset of the CPU curve hosts (section 5) |
| 6 | What the lead reads | One table of go and stop numbers (section 6) |
| 7 | Off | Drop `--memory-curve-hosts` (or delete its file key) and restart (section 5) |

### Build steps, each its own pull request and each useful alone
1. **Signals.**
   - The agent reports the host's swap size in its facts and its swap in use in each heartbeat.
   - A job killed for host memory ends with its own outcome. The server maps it to `Failure::NodeMemoryPressure`,
     which requeues it with the same booking, prefers another host and fails it after the farm rerun bound
     (`crates/js-sched/src/scheduler/fault.rs:74-90`). Each kill is counted per host.
   - This makes the flat farm's host kills visible now, curves or not.
2. **The memory shadow.** Built (komira-ai/jobscheduler#1254). Its per-slot counts on the farm set the margin and
   the cushion.
3. **The remaining maximum on named hosts,** behind `--memory-curve-hosts`, with the checks of section 4 and the
   tests of section 7. Under `shadow` it adds the remaining maximum's own `sooner` and too-low counts beside the
   per-slot ones, so the two can be compared on the same days. It runs under `shadow` for at least two working days
   before step 4.
4. **Host preparation, then the canary.** Swap on the named host, the pool's percent at 95, then one host for a
   working day, three hosts, all six.

## 2. What a job is charged

### The remaining maximum of the shadow's curve
The enforced charge reuses the curve the shadow already computes (`MemoryCurve`,
`crates/js-sizing/src/memory_curve.rs`). Its slot booking is the 90th percentile with the flat margin, held to at
most flat. A running job of age `a` is charged the largest of `charge(a, k)` over every slot `k` ahead of it
(`memory_curve.rs:74`). That is the most the curve says it may still hold, wherever in its life that comes.
- **It never rises** as the job ages, because the largest over fewer slots ahead is never larger. So the test made
  now holds at every later moment. The curve's own value at a slot can rise, because the runs that last longest
  may be the heaviest, and taking the largest from here on absorbs that. Past 128 s the curve charges its tail
  booking at any age (`memory_curve.rs:76-78`), which the largest already included.
- **One exception:** a job that reaches a start slot fewer than 4 kept runs reached is charged flat
  (`memory_curve.rs:79-81`). That is a rise, so it counts as a miss in check 3.
- **A queued job is charged its flat booking.** That is what lets the store keep its own memory check (section 5).
  It costs little, because a queued job's curve is held to flat and starts at its first slots (INFERRED).
- **No survival weighting,** as in the shadow: a job still running is charged as if it runs on.

What the profile gains for this. It is learning state, rebuilt from the job records at start, as the profile is
(`crates/js-sched-placer/src/sizes_profile.rs:25-27`). None of it is on the start path.
- **A window of 16 runs for whether the memory series was whole.** The existing window counts the CPU series.
- **A window of 16 runs for the held-out check** (section 4, check 3).
The tail figure the charge needs, each kept run's peak after 128 s, is already kept (komira-ai/jobscheduler#1254).

### Why not the per-slot projection the shadow uses
The shadow charges a job each slot's booking at the slot it will be in. That lets a job whose peak is late, such as
a link at the end of a compile, start beside one whose peak has passed, which is the larger gain. But a job slower
than its class moves its peak onto another job's. For CPU that is a slowdown. For memory it is a kill.
**The shadow's counts bound the remaining maximum from both sides.**
- A running job's charge here is at least its per-slot charge at every slot ahead.
- A queued job's flat booking is at least its curve.
- The residual is the shadow's (section 3).
So this projection is at least the shadow's at every slot. It is too low no more often than the shadow's, and it
places a job sooner no more often.
**DECIDES:** enforce per slot later only if the shadow's own too-low share is under section 6's go bar for the
whole canary.

### The margin, and how it is learned
- The margin is the curve's: x1.2 + 128 MiB on the class's 90th percentile, the same sizing as the flat booking.
  The class's runs are the learned part.
- The margin is not tuned per class, because a learned margin is one more estimate that can be wrong.
- What a class learns is whether it may use its curve at all: section 4's held-out check.
- **DECIDES:** the shortfall, measured minus projected, from the shadow's "a host held more memory than its jobs'
  memory curves projected" lines on the Linux hosts.
  - If its p99 is above 12.6 GiB (5% of a host), the margin becomes x1.3 and the shadow runs again before any host
    is named.
  - The line's measure includes memory that belongs to no job, so this reads high.

## 3. What protects a host when its real memory passes the projection
From the first defence to the last.

1. **The class's own checks** (section 4). A class that misses books flat at the next pass.
2. **The measured floor.** A host's projected memory is `H = S + R`.
   - `S` is the sum of the running jobs' charges.
   - `R` is what the host's non-file memory plus its swap in use shows beyond the sum of those jobs' charges at
     their present slot, `sum_j M_j(0)`. It is the shadow's residual, with swap added, and it is never below 0.
   - A job fits when `H + its flat booking <= the host's schedulable memory`.
   - Swap is added because swapped pages have left the non-file figure.
   - A placed job that has not started yet is charged its age-0 charge, so it counts before it uses anything.
   - A miss stops new jobs within one heartbeat.
   - Readings count while at most 15 s old, as for the shadow (`FRESH_MS`,
     `crates/js-sched-placer/src/memory_shadow.rs:43`). A named host whose last reading is older books flat.
3. **The level where the host stops taking jobs: the host's schedulable memory.**
   - Once measured use reaches it, nothing fits, flat or curve, until use falls.
   - B14 allows one memory setting, `memory_schedulable_percent`, so the level is that setting.
   - **Recommend 95 for the Linux pool,** set before the first host is named. That leaves 12.6 GiB of each host
     unbooked, for the kernel, the agent and the start of a miss.
   - **Cost:** the setting is per pool, so the flat hosts book 5% less. In the measured hour that is 1,435 of
     1,510 GiB, against a peak minute of 1,472 booked (INFERRED: about 37 GiB of jobs wait longer in peak minutes).
     90 would cost 113 GiB in that minute, so this page does not ask for it.
4. **Swap with zswap in front: the cushion.**
   - **Recommend 32 GiB of swap per named host,** encrypted with a key made at each boot, with zswap (zstd) in front.
   - zswap keeps the first pages compressed in memory, so a small miss costs CPU, not disk.
   - Encryption is needed because run files on a tmpfs can be swapped (`crates/js-agent/src/cli.rs:212-213`).
   - Swap counts in `H` and is never room: a host that swaps takes nothing new.
   - **DECIDES:** the size is at least twice the largest shortfall in the shadow's "held more memory than ...
     projected" lines. If that is above 16 GiB, the cushion grows to match. If it is above 32 GiB, the step stops.
   - Swap is set up by the host's own configuration, outside this repository. The lead checks the size in
     `js host list` (build step 1) once, before naming the host. There is no runtime check (B20).
5. **The kernel.** If all of these fail, the kernel kills a job, as it can on a flat host today. The job is requeued
   and counted (build step 1). One such kill on a named host means the lead turns memory curves off (section 6).

**No per-job limit.** Each job keeps today's limit, which is none. A `memory.max` at the charge would turn a wrong
estimate into a kill. A `memory.high` would throttle. B14 forbids both.

**The scheduler never kills, throttles or moves a running job.** A wrong estimate changes only where the next job
goes.

## 4. Fallback to flat
A class books flat at the next pass unless all five hold.
1. **Scored:** 16 scored memory runs (`CHECK_RUNS`), as CPU's check 1.
2. **Shaped:** the memory profile's existing `shaped` test, curve error at most 80% of flat (`FIT_PERMILLE`). A class
   whose memory does not change over its life gains nothing.
3. **Held out:** each of the last 16 runs was compared, when it was taught, with the charge the class booked before
   it. None used more than that charge at any age, and none reached a slot where its charge rose. The count allowed
   is zero (CPU allows 3 in 16), because a memory overrun is a kill risk.
   - This is the per-class check [placement-profiles-live.md](placement-profiles-live.md) section 4 asked for.
   - It uses 16 runs, not the 64 that page named. Every check then reads one window (`CHECK_RUNS`), and a run
     tested before it was learned is a stricter test than one judged among the runs it shaped.
   - It is new because the profile's overrun flag cannot serve memory. That flag compares a peak with the start
     slots only, so every late peak counts (komira-ai/jobscheduler#1254).
4. **Series whole:** 12 of the last 16 runs recorded a whole memory series. Mac runs record none, so they book flat.
5. **Spread not too wide:** the 95th percentile of the kept runs' peaks is at most twice their median.
   **DECIDES:** if the shadow's classes between 1.5 and 2 miss more often than those below 1.5, the bound becomes 1.5.

Also flat:
- a job that names its memory;
- a whole-host job;
- a class with no profile;
- a host not named;
- a job past the wait bound `server.curve_max_wait_seconds`. One wait bound covers both axes.

The stored booking is the flat number in every mode, as for CPU. It is what the store, the agent and a rollback see.

## 5. Where it sits, the canary and the off line

### In the placement pass, not the start path
- **The charge is computed in the pass** that builds the CPU table for each curve host (`curve_room.rs`,
  `begin_curve_walk`). It reads the live jobs' ages, the class profiles and the latest heartbeat. Like the CPU
  table, it is rebuilt each pass, kept in memory and never stored.
- **The store keeps its memory check.** On a named host the placer hands the store a flat memory capacity of
  `schedulable memory + (sum of the running jobs' flat bookings - H)`. The store's own test is the sum of stored
  flat bookings plus the new one, at most that capacity, and that is the same as `H + new flat booking <=
  schedulable memory`.
  - Suppose another server instance committed a job since this pass read the store. That job's flat booking is in
    the store's sum, and it is at least its charge. So the test only gets stricter.
  - Placement is serial on the store's commit head ([multi-instance.md](multi-instance.md)). Two instances cannot
    overbook a host's memory between them, and a rolling update adds no race.
  - This differs from CPU, whose store check is lifted on curve hosts (`curve_room.rs:251-266`). CPU can stay
    lifted, because a CPU overlap costs a slowdown.
- **The pass judges memory itself on a named host.** The build adds a memory-judged flag beside `set_cpu_judged`,
  so js-sched's minute-0 memory check and the booked-memory check are skipped there
  (`crates/js-sched/src/admission/horizon.rs:136-148`). The room's `fits_without_memory` and `memory_capacity`,
  which the shadow added (`crates/js-sched-placer/src/room.rs:131-162`), serve the fit. Today only CPU is
  skipped.
- **Nothing else changes.** The agent, the job's cgroup and the stored booking stay as they are. The start path
  gains no state, no message and no wait.

### The canary
- **`--memory-curve-hosts`, or the file key `server.memory_curve_hosts`,** a list of host ids, read once at
  start. It mirrors `--curve-hosts` and `server.curve_hosts` (komira-ai/jobscheduler#1250), and the job service runs
  on flags. It takes the same checks (`crates/js-lease-server/src/config/placement_curves.rs:75-97`): valid ids, no
  repeats, at least one.
- **It acts only under `on`.** The server refuses at start a named host outside a non-empty `server.curve_hosts`, so
  memory goes on only where CPU curves have already run.
- **An absent key means no host,** unlike `server.curve_hosts`. Widening to all means naming all six.
- **Stages:** the CPU canary host after its own working day passes, then three hosts, then all six. Each stage runs
  a working day, and the lead reads section 6 before the next.
- **Compatibility.** The new heartbeat and facts fields and the new outcome are optional and additive (B8).
  - A host whose agent sends no swap figure is not named.
  - The server is updated before the agents. A server of the release before maps the new outcome as it maps any
    exit it does not know.
  - The build pull request names the test that runs the previous release against the new data, and the reverse.

### The off line
- **Drop `--memory-curve-hosts` from the service's arguments, or delete its file key, and restart** through the
  update hook. Every class books memory flat at the next pass. CPU curves and the profiles are untouched.
  `--placement-curves shadow` turns both off.
- **Nothing stored needs undoing,** because bookings are stored flat.
- **A host may stay full for a while.** A host packed by memory curves may hold more flat bookings than its
  schedulable memory. The flat check refuses it new jobs until enough of them end, which can take as long as its
  longest job: the 12 GiB class averaged 639 s (MEASURED, the same `js usage` hour). No running job is touched.

## 6. What the lead reads
**Before naming a host:**

| Read | Source | Go when |
|---|---|---|
| Placements the remaining maximum would make sooner, Linux hosts | its `sooner` count in shadow (build step 3) over all of `js_memory_curve_placements_total` | at least 5% |
| Readings above the projection | `js_host_memory_projection_low_total` over `js_host_memory_projection_checks_total`, per host | under 1% |
| Shortfall when above: p99, and the largest | the "held more memory than its jobs' memory curves projected" lines, measured minus projected | p99 at most 12.6 GiB; largest at most half the swap |
| Refusals by reason, `booked-memory` against `cpu` and `cpu-curve` | `js_placement_refusals_total` | memory refuses at least as often as CPU |
| Kills for host memory on the flat farm | `/metrics` (build step 1) | the rate before, to compare with |

**During each stage, the named host against one flat host:**

| Read | Source | Stop when |
|---|---|---|
| Kills for host memory | `/metrics` | any one on a named host |
| Swap in use | `/metrics` | above half the swap at any time |
| Readings above the remaining maximum's charges | its too-low count over its checks (build step 3), per host | above 1% |
| Run time per class | job records | p50 more than 5% slower than on the flat host |
| Queue wait and jobs per hour | `js usage` | no better than the flat host after a working day: memory curves buy nothing there, so turn them off |

The build adds to `/metrics`:
- per host: kills for host memory, swap in use and the memory charge (build steps 1 and 3);
- the remaining maximum's own placement verdicts and too-low and check counters, beside the shadow's (step 3);
- a `memory-curve` refusal reason, and a count of memory-curve classes against flat.

## 7. What the build pull requests test
The minimum, in the repository's simulator and unit tests:
- **Past its peak:** a job past its class's peak is charged less than flat, and a queued job fits beside it where
  the flat booking queues. A job before its peak is charged its peak.
- **Never rising:** a class whose longest runs are its heaviest gives a charge that does not rise with age. A job
  that reaches a start slot fewer than 4 runs reached is charged flat, and that run counts as an overrun.
- **Bounded by the shadow:** at every slot the host's `H` is at least the shadow's `P(k)`, and under `shadow` both
  sets of counts are written.
- **Above its profile:** a running job above its charge raises `H` at the next heartbeat. The next job is refused,
  the class books flat at the next pass, and the running job runs to its end.
- **Too little history:** 15 scored runs book flat, 16 may book the curve.
- **Each check at its boundary:** one held-out overrun in 16; 11 against 12 whole series; spread 2.0 against 2.01.
- **Stop level:** measured use at schedulable memory refuses every job, flat or curve.
- **Two instances:** a job committed by one instance after the other read the store is in the store's sum, and the
  second instance's commit is refused when the two together pass schedulable memory.
- **Canary and off:**
  - an unnamed host books flat;
  - a named host outside `server.curve_hosts` is refused at start;
  - with the key absent, placement matches main's placer on the same queue, job for job.
- **Host kill:** a job killed for host memory is requeued with the same booking, avoids that host, fails after the
  farm rerun bound and is counted.

## 8. Considered and not chosen
- **A per-job limit, `memory.high`, or evicting or moving a job when a host runs short.** Each kills, throttles or
  moves a running job on an estimate. B14 forbids a memory limit beyond `memory_schedulable_percent`.
- **Enforcing the shadow's per-slot projection now** (section 2). It is the larger gain, but a timing error costs
  a kill. It may come later, if the shadow's own too-low share stays under the go bar.
- **A learned margin per class.** It is one more estimate that can be wrong. The held-out check does its job.
- **Charging a queued job its age-0 charge.** It gains little, and the store could no longer check memory by flat
  bookings.
- **Lifting the store's memory check, as CPU's is lifted.** Two instances could then overbook a host's memory.
- **A new setting, or a per-host percent, for the stop level.** B14 allows only `memory_schedulable_percent`, and it
  is per pool.
- **Counting swap as room.** A host would book onto disk, and its jobs would slow.
- **A runtime check that a named host has swap.** It is machinery for a config mistake (B20). The lead checks once.
- **`earlyoom` or `systemd-oomd` on the hosts.** They kill earlier than the kernel and pick victims by their own
  rule.
- **Memory curves on `server.curve_hosts` itself.** Memory could not be turned off while CPU curves stay on.
