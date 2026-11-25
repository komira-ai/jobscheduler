# Booking from the learned profiles: the live mode after `--placement-curves shadow`

Status: design for review. Nothing here is built. It follows [cpu-over-time.md](cpu-over-time.md), whose steps 1
(record) and 2 (learn in shadow, komira-ai/jobscheduler#1057) are built, and fixes what its steps 3 (decide in
shadow) and 4 (enforce) book, gate and roll out. Companions: [sizing.md](sizing.md) (how a class is booked today)
and [placement-by-load.md](placement-by-load.md) (the load predicate and the pack limit).

Every code reference is `path:line` on main at komira-ai/jobscheduler#1076 (`ffa439e29`). Labels: **READ**, read
in the code at that commit. **MEASURED**, a figure an earlier document or pull request measured, named where it is
used. **ASSUMED**, a choice not yet tested. **INFERRED**, reasoned from the code, not run.

## 1. First page

### What is asked
The scheduler should predict each job's CPU and memory from what its class used before and what the host uses
now, and place on that prediction. Placement already packs a host to half of its bookings and then places on the
least full host (komira-ai/jobscheduler#1076). In shadow, the job service learns a CPU and a memory profile per
class, logs each run against it and books nothing (#1057). This document says what the live mode books, how it
combines with the host's readings, what shadow must show before a class is booked from its profile, and how the
mode is turned on and back off.

### Where things stand (READ unless marked)
- **Booking today is one number per class and dimension.** It is decided at each pass and never stored on the job
  (`crates/js-sched-placer/src/sizes.rs:59-62`).
  - **CPU** is the median of the last 32 runs' mean CPU, rounded up to a quarter core. A class whose five-second
    peaks are a heavy tail books the 90th percentile of those peaks instead: at least 5 peaks, and their p90 more
    than twice the median (`crates/js-sizing/src/summary.rs:40-45`, `sizing.md` 7.10). The booking is held to a
    quarter of the host (`HOST_CPU_SHARE`, `sizes.rs:177`).
  - **Memory** is the largest non-file peak over the last 257 to 288 runs, times 1.2 plus 128 MiB, rounded up to
    256 MiB, at least 1 GiB, never below the class's kill floor (`summary.rs:492-508`, komira-ai/jobscheduler#1035).
- **The agent enforces neither booking.**
  - CPU becomes `cpu.weight`, 100 per core (`crates/js-contain/src/cgroup.rs:73`). There is no `cpu.max` unless the
    operator sets `--cpu-ceiling`.
  - Memory is written nowhere. A job lease has no `memory.max` and no `memory.swap.max`
    (`cgroup.rs:315`, `LeasePolicy::TrackedMemory => {}`, since komira-ai/jobscheduler#1027).
  - So a booking that is too low costs CPU share under contention, or host memory pressure. It never throttles
    or kills the job that was under-booked.
- **The profile is a curve, not a number** (`crates/js-sizing/src/profile.rs`).
  - It keeps a class's last 64 successful runs, weighted by `2^(-age / 6 h)` (`profile.rs:17-19`).
  - For each 2 s slot of ages 0 to 128 s it gives a decayed mean (`slot_mean`, `:265`), a load-weighted 90th
    percentile (`slot_p90`, `:273`) and how many kept runs reached the slot (`slot_runs`, `:278`). After 128 s it
    gives one decayed mean (`tail`, `:284`). Over the whole run it gives a decayed mean (`level`, `:259`).
  - CPU slots are means of their seconds; memory slots are the largest second (`profile.rs:65-76`).
  - A class is **shaped** when its curve's error over its last 16 scored runs is at most 80% of the flat level's
    (`CHECK_RUNS`, `FIT_PERMILLE`, `profile.rs:25-27`; `shaped`, `:337`). No run is scored before 8 kept runs
    (`MIN_RUNS`, `:21`).
  - Profiles are kept for at most 2048 classes (`MAX_PROFILES`, `crates/js-sched-placer/src/sizes_profile.rs:30`)
    and rebuilt from the job records after a restart; records are kept 7 days by default
    (`crates/js-lease-server/src/config/retention.rs:11`).
- **Shadow books nothing**, and a test pins it:
  `the_sizes_booked_are_the_same_with_the_profiles_off_and_in_shadow`
  (`crates/js-sched-placer/src/sizes_profile_tests.rs:561`).
- **What shadow has shown.** One window has been measured, in komira-ai/jobscheduler#1077 (`cpu-over-time.md` section 10, MEASURED):
  - 4 h 10 min of builds gave 29,256 check lines over 5,335 classes, and 330 classes had at least 16 lines.
  - Of those 330, 28 (8.4%) were shaped by CPU and 38 (11.5%) by memory.
  - The curve's CPU error was 95.4% of the flat line's: it removed 4.6% of the CPU prediction error.
  - 44% of the lines' CPU series disagreed with the exact CPU time by more than 5%, and 17% carried no series.
  - Since shadow was turned on across the farm, the check lines have not been reaching the log. That is being
    fixed separately. Until it is, there is no accuracy data beyond that one window.

### The design, in nine lines
1. **The booking number does not change.** Each class still books the number it books today, for both CPU and
   memory. That number is what the agent's `cpu.weight`, the store's room check and the pack limit read (section 2).
2. **CPU gains a time dimension.** A job of a shaped class is also charged its class's curve for the next 128 s.
   A host is refused for it when, at some 2 s slot, the host's expected CPU plus the job's would pass cores x
   slack (1.1 by default). This test can only hold a job back that today's rule would place. It never admits one
   that today's rule refuses (sections 2 and 3).
3. **Current use** is today's slot-0 figure from the host's readings (komira-ai/jobscheduler#1055), carried forward. Each running job
   adds the change its class's curve predicts between now and that slot, weighted by the share of its class's
   runs that were still running then (section 3).
4. **Memory stays one number per class**, from the largest recent peak, because a wrong memory prediction can
   kill a job on a busy host, and nothing teaches its class when that happens (section 2). The memory profile
   stays in shadow until a separate gate (section 4) and a separate decision (question 2).
5. **A cold class** books `--cold-cpu-millis` and `--cold-memory-bytes` (and the `-on <os>` tables) exactly as
   today. A class that is unshaped, or has fewer than 8 kept runs, is charged flat (section 2).
6. **Per class, the switch is automatic.** A class is charged its curve only while it passes four checks,
   computed by the job service from the same figures `js-cpu-replay shadow` prints. A class that fails one goes
   back to flat at the next pass (section 4).
7. **Farm-wide, the switch is an operator's.** `server.placement_curves = "on"` turns the test on, first on the
   hosts named in `server.curve_hosts`. It goes on only after 24 h of working check lines, a decide-in-shadow
   period and the replay's pass mark. Setting it back to `shadow` and restarting undoes it at once (section 5).
8. **When the curve is wrong**, nothing running is throttled, killed or moved. The host's reading charges what it
   actually uses at the next reading. The class fails its checks and goes flat, within 16 scored runs (section 4 says what delays that).
   A job is never held by the curve test for more than 30 s (section 4).
9. **Memory fallback.** A run above its memory booking raises its class's next booking at once, as today
   (#1035). The open komira-ai/jobscheduler#1024 adds the margin to runs that swapped (section 4).

### What changes where (for the build pull requests)
| Part | Change |
|---|---|
| `js-sizing` | The profile already answers `slot_mean`, `slot_p90`, `slot_runs`, `tail`. It gains, beside its window of 16 scores, a ring of the last 16 runs' peak-overrun flags (check 3) and a ring of the last 16 runs' CPU series status (check 4), and pure functions reading them (section 4). |
| `js-sched-placer` | `Curves` gains `On`. A per-host table of 64 two-second slots. One extra test in `try_job`, beside `Room::fits`. A first-held time per queued job, in memory. Two log lines. |
| `js-sched` | One read-only accessor for a host's slot-0 CPU charge (`cpu_left` at slot 0, `crates/js-sched/src/admission/horizon.rs:57`). The predicate, its minute horizon and the pack limit are unchanged. |
| `js-lease-server` | The `on` value, `server.curve_hosts`, `server.curve_max_hold_seconds`, three metrics. |
| `js-cpu-replay` | `shadow` prints the four checks per class. The trace carries series, so the replay can run the curve test (section 4). |
| Agent, wire, store | Nothing. |

### Decisions needed
Section 8 has the questions for the owner, each with a recommendation. In short:
1. Keep today's booking number and add the curve test: yes.
2. Memory stays on today's rule: yes, until the memory gate passes and refusals show memory is what binds.
3. Use the four per-class checks and the thresholds in section 4: yes.
4. Canary on named hosts, not per pool or per class: yes.
5. Hold bound 30 s, and a held job does not keep its host: yes.
6. The pack limit stays on bookings: yes.
7. Gate on the replay's pass mark before `on`, and stop if it is not met: yes.
8. Leave `MAX_PROFILES` at 2048 until a longer window shows evicted classes that had 16 runs: yes.

## 2. What the live mode books

### Two figures per job, used in different places
Today one figure, the booking, is used in four places. The live mode keeps it in all four and adds a second figure
for the new test.

| Where | Reads today (READ) | Live |
|---|---|---|
| Agent `cpu.weight` | the booking (`crates/js-agent/src/crun/make.rs:211-215`) | unchanged |
| Store room check `Capacity::has_room` | bookings summed per host (`crates/js-sched-placer/src/room.rs`) | unchanged |
| Pack limit | bookings with the new job, the larger of CPU and memory share (`crates/js-sched/src/load.rs:357`, `scheduler/worker.rs:126`) | unchanged |
| Load predicate at slot 0 | readings plus bookings (`horizon.rs:57-78`) | unchanged |
| **Curve test**, 2 s slots to 128 s | (none) | **new**, shaped classes only |

The test is an extra condition before the scheduler's own. So the set of hosts a job may go to is always a subset
of today's set. That gives an exact equality to test against: with every class flat, placement equals `shadow`
placement.

### CPU: a level and a peak over time
For a job `n` of a shaped class, with `s` its 2 s slot (0 to 63):
```
E_n(s) = slot_p90(s)    while slot_runs(s) >= 4      (MIN_SLOT_RUNS, profile.rs:23)
       = level           otherwise, and from 128 s on
each slot then clamped to host_share (a quarter of the host, sizes.rs:177)
```
- **The new job is charged its peak.** The load-weighted 90th percentile per slot is the start shape
  `cpu-over-time.md` section 4 designed for admission, the only moment placement acts.
- **The running jobs are charged their level.** Section 3 charges a running job its class's mean per slot. Summing
  many jobs' 90th percentiles over-counts: their bursts do not all fall in the same slots, and the host reading
  corrects the sum.
- **An unshaped class is flat** in the test: `E(s)` = its booking at every slot, which adds nothing to today's
  rule. The same holds for a class with fewer than 8 kept runs, one evicted past 2048 classes, one with no series
  (Mac hosts send none, `cpu-over-time.md` section 10), and a job that names its own CPU.
- **Why the booking number stays the median.** On the replay, a booking of the decayed mean and today's median gave
  the same total time within 0.5% (64.8 s against 65.1 s, REPLAY in `cpu-over-time.md` section 10). Changing the
  number would change every job's `cpu.weight` and every pack share for no measured gain. Question 1 asks to
  confirm.

### Memory: one number, from the peak
Memory differs from CPU in what a wrong figure costs.
- **CPU over its booking runs slower.** CPU above the booking is taken from idle cores, or shared by weight under
  contention. The job ends.
- **Memory over its booking can kill.** A lease has no `memory.max`, so a host short of memory reclaims, swaps and
  then OOM-kills a victim, which may be another job. The agent reports that kill as an ordinary exit, not as a
  memory kill: "a node short of memory is not the booking's fault" (`crates/js-agent/src/crun/spec.rs:226-241`).
  The placer raises a class's kill floor only for `ResultReason::Memory` (`sizes.rs:616`). **So a memory
  under-prediction that starts a host-wide kill teaches no class anything.**
- **Live memory books today's figure.** That is the largest peak over 257 to 288 runs with its margin. The profile
  keeps 64 runs with a 6 h half-life, so its maximum forgets a rare large peak sooner. Booking from it could only
  lower bookings, and so raise this risk.
- **Memory over time is not charged either.** Crediting a running job's memory as it falls (the remaining maximum,
  as `horizon.rs:14-17` does for leases) is the memory half of the curve idea. For jobs of which 92.5% end within
  128 s (MEASURED, `cpu-over-time.md` section 3) it frees little. It stays in shadow, behind the memory gate of
  section 4 (question 2).
- The memory predicate stays as it is: the larger of the summed bookings and the working set plus the uncovered
  jobs' bookings, at slot 0 (`horizon.rs:80-84`). This already mixes current and historical use.

### A cold class
- **No run of its own, its fallback class or its client.** It books `--cold-cpu-millis` (default 22 cores) and
  `--cold-memory-bytes` (default 4 GiB), or the `--cold-*-on <os>` entry for its os (`sizes.rs:154-159`,
  `crates/js-lease-server/src/lib.rs:54-56`). This is unchanged. The scheduler's cold cap, about one cold admission
  per 32 cores per window, is unchanged.
- **A class with runs but no usable profile** (fewer than 8 kept runs, unshaped, evicted, no series) books today's
  figure from its own, fallback or client history and is flat in the curve test.
- **A cold class is never curve-charged.** It has no profile, and its 22-core cold booking is already the
  conservative charge.

### The 1.1x CPU slack and the 50% pack limit
- **Slack.** The curve test refuses a host when, at some slot `k`, `L_h(k) + E_n(k) > cores x (1 + s_cpu)`. Here
  `s_cpu` is the existing `--cpu-slack` (`s_cpu_permille`, default 100, `crates/js-sched/src/admission/config.rs:123`).
  It is the same cap the slot-0 predicate uses, so nothing is booked past the slack (AGENTS.md B14). A larger
  `--cpu-slack` loosens both tests together. There is no separate curve slack.
- **Pack limit.** The pack limit orders hosts. It never refuses one (`load.rs:338-372`). It stays on bookings:
  the new job's booking plus the host's, against raw cores and schedulable memory, with default 0.5
  (`DEFAULT_PACK_LIMIT_PERMILLE`, `load.rs:157`).
  - Because the booking number does not change, a host fills to half of its bookings and then the least full
    host is chosen, exactly as today.
  - The curve test only removes candidates before the ordering. A burst-first job that would push the fullest host
    over its cores in some slot goes to the next host in the same order.
  - "Least full" is by bookings (`Pack::Past` sorts by booked share), not by measured load. Question 6 asks
    whether to change that. The recommendation is no.

## 3. Current use: the host's readings plus the predicted remaining use of its jobs

### Today, at slot 0 (READ, `horizon.rs:57-78`, #1055)
```
L_h(0) = max( measured, max(before, F_covered) + U_seen ) + U_unseen + V_cpu
```
- `measured` is the larger of the last two 5 s reading windows.
- `before` is the window before the newest, the one that ended at the reading before it.
- `F_covered` is the bookings of the jobs the newest reading covered.
- `U_seen` is the jobs on the ledger when that reading was taken in but not covered by it.
- `U_unseen` is the jobs placed after it, added on top in full.
- `V_cpu` is VMs.
- A reading older than 3 windows (15 s) is stale (`crates/js-types/src/sched/usage.rs:13`). The host is then
  charged by bookings alone.

### Live, at slot k of 2 s (k = 0 to 63)
```
L_h(k) = max( 0, L_h(0) + sum over jobs j on h of ( E_j(age_j + k) - E_j(age_j) ) )

E_j(t) for a running job j of a shaped class, with i = slot of its age, s = slot of t:
    s < 64 and slot_runs(s) >= 4:  slot_mean(s) * slot_runs(s) / slot_runs(i)
    s < 64, fewer runs reached s:  its booking            (it outlived its recorded runs)
    s >= 64:                       tail, else its booking
E_j(t) for a job of a flat class:  its booking, at every t
```
- **The reading is carried forward, not faded.** `L_h(0)` already holds the measurement and #1055's handling of
  jobs the reading has not seen. Each slot ahead adds only what the curves say will change. Any CPU the reading
  shows beyond the jobs' expectations (other work, or a job above its curve) stays in every slot.
  - `cpu-over-time.md` section 5 let that residual fade within 10 s (`B_h`). This document does not. A fade would
    let a burst-first job onto a host whose excess is a long job above its curve. Carrying it costs only holds,
    and holds are bounded at 30 s. This is a deliberate change from that section (section 9 records it).
- **Remaining use is weighted by survival.** `slot_runs(s) / slot_runs(i)` is the share of the class's kept runs
  that reached age `s`, out of those that reached the job's current age. A job whose class mostly ends at 40 s is
  charged little at 60 s. This is the "credit for jobs about to end" that `cpu-over-time.md` section 11 left open.
  - It is safe because the curve test only holds jobs. A credit that is too generous makes the test hold fewer
    jobs, which falls back to today's behaviour. It never admits past the slot-0 test.
- **The equality the first test asserts.** With every class flat, every `E_j` is constant, so `L_h(k) = L_h(0)`
  at every slot. The curve test then reads `L_h(0) + booking_n`, which is today's slot-0 test. Placement equals
  `shadow` placement.
- **Ages.** A job's age is the time since the agent reported it started. A job allocated but not yet started is
  age 0. The table is rebuilt for every host at each reading the scheduler accepts, and a placement adds its job's
  lane. This is the same rhythm as the existing per-minute lanes (`crates/js-sched/src/admission/profile.rs:1-25`).
- **Why not the existing minute lanes.** js-sched already charges leases over a 30-minute horizon of 1-minute slots
  (`HORIZON`, `load.rs:127`; `admission/profile.rs`). The job placer feeds them nothing (`life_minutes: 1`,
  `crates/js-sched-placer/src/jobs.rs:532`; `engine.rs:17`). Most jobs end within 128 s, so a minute slot holds a
  job's whole life. Its peak would be booked for the whole minute, which is booking the peak, not the shape. The
  2 s table lives in `js-sched-placer`, and js-sched keeps its minute predicate for leases.
- **Memory current use** is unchanged (section 2).

## 4. Gates, and the fallback when a profile is wrong

### Per class: four checks, computed by the job service at each pass
A class is charged its curve only while all four hold. Checks 1 and 2 are the profile's own `shaped` rule, over
the window of 16 scores it already keeps (`profile.rs:322-337`). That window holds only each scored run's curve and
flat errors (`profile.rs:190`), so checks 3 and 4 need new per-class state in `js-sizing`: a ring of the last 16
scored runs' peak-overrun flags, and a ring of the last 16 runs' CPU series status, scored or not. Both are rebuilt
with the profile from the job records after a restart. `js-cpu-replay shadow` computes the same four from the log,
so an operator reads in the table what the service decides.

| # | Check | Threshold | Exists today |
|---|---|---|---|
| 1 | Enough runs | 16 scored runs (`CHECK_RUNS`) | yes |
| 2 | Shape pays | curve error at most 80% of flat (`FIT_PERMILLE`, `shaped`) | yes, printed as `CPU curve/flat %` and `CPU shaped` |
| 3 | Peak calibrated | at most 3 of the 16 runs peaked more than 25% above their predicted peak | no: computable from `cpu_peak_millicores` and `cpu_predicted_peak_millicores` in the line |
| 4 | Series usable | at least 12 of the class's last 16 check lines have a CPU status `learned` or `not-learned` | no: computable from the `cpu` status in the line |

- **Check 3, ASSUMED.** The predicted peak is the largest per-slot 90th percentile over the slots the run reached
  (`profile.rs:434-436`). It is not the 90th percentile of the runs' peaks, so its overrun rate is not 10% by
  construction and is not yet measured. A class whose burst lands in a different slot each run has a low 90th
  percentile in every slot, and most of its runs overrun it. That is the case check 3 exists to catch. Such a
  class would otherwise be charged a burst at the wrong time, which is worse than flat. The 3 in 16 with a 25%
  margin is a starting value, to be set from the decide-in-shadow days.
- **Check 4, ASSUMED.** Farm-wide, 44% of the lines' series disagreed with the exact CPU time (MEASURED, #1077).
  A class that learns from only a minority of its runs learns from a biased minority, for example only its long
  runs.
- The checks need no new log field. `shadow` gains two columns, `CPU peak over %` and `CPU series usable %`, and a
  `CPU live` column: `yes` when all four hold at the class's last line.

### Farm-wide, before `on`
1. **The log works.** At least 24 h of normal builds whose check lines reach the log, from every server task, sorted
   by time (`cpu-over-time.md` section 10). The current one window of 4 h 10 min is too short. A class needs 16
   scored runs, and most classes run less often than that.
2. **Decide in shadow** (`cpu-over-time.md` step 3), for at least two working days. In `shadow`, the service also
   runs the curve test and logs each job it would have held:
   `a job would have been held by its class's curve` with `job`, `class`, `host_count` (hosts refused), `slot` (the
   first slot over the cap), `held_ms` (once placed). It is one line per held job, not per pass. This answers how
   many jobs a live mode holds, for how long, and on which classes, before any job waits.
3. **The replay's pass mark** (`cpu-over-time.md` decision 2). On recorded days held out from the learning days,
   the curve test takes 10% or more off total time against today's rule, with throughput within 2%.
   - This needs the trace to carry each job's series. Today it has only a mean and a peak, and its shapes are
     synthesised (`crates/js-cpu-replay/src/trace.rs:52-86`).
   - The farm figures so far bound the prize: 8.4% of the classes with 16 lines are shaped, and they remove 0.24%
     of the CPU prediction error (2,908 of 1,217,183 core-seconds, MEASURED, #1077). If the longer window looks
     the same, the pass mark will not be met and the recommendation is to stop at shadow (question 7).

### When a CPU profile is wrong
- **Nothing running is throttled, killed or moved** (the owner's ruling, `cpu-over-time.md` section 7). A host that
  turns out over its cores runs that way until its jobs end.
- **Within one reading** (5 s), the host's measured CPU enters `L_h(0)` and is carried to every slot. The next job
  is tested against what the host actually does.
- **Within 16 scored runs**, the class fails check 2 or 3 and goes flat at the next pass. Check 3 usually fails
  sooner: four overruns are enough. A run with no usable series adds no score, so it does not move checks 2 and 3.
  Check 4 bounds that: at 5 unusable runs of the last 16, the class goes flat.
- **Within 30 s**, a job held by the curve test skips the test (`server.curve_max_hold_seconds`, default 30). It is
  then placed by today's rule alone.
  - A held job does not keep its host: a job behind it in the queue may take the host. A curve-held job is not the
    walk's "first job that waits" on a host (`sizes.rs:175`).
  - So the curve test never blocks the queue behind a held job, and the delay the curve test itself adds is 30 s
    at most. While it is held, a job also has no first-waiter claim on a host. A stream of later jobs can use the
    room it would have taken, and after the 30 s it may still wait under today's rule, as any job that finds no
    room does. The bound covers the curve test, not the job's whole wait.
- **A curve that is too high** costs only holds. It is seen in the `cpu-curve` refusals and in the hold log line.

### When a memory figure is wrong
- **The live mode books today's memory figure**, so its fallback is today's:
  - A run above its booking reports its whole peak, since there is no `memory.max` to hold it. Its class's next
    booking is that peak x 1.2 + 128 MiB, rounded up to 256 MiB, at once (#1035, `summary.rs:492-508`).
  - A run killed for its own limit doubles its class's floor (`sizes.rs:86-89`).
- **komira-ai/jobscheduler#1024** (open) steps a class up when its runs end pinned at their memory limit. Its
  description assumes js-agent writes `memory.max` = booking. On main that stopped with #1027 (`cgroup.rs:315`), so
  no run is pinned at a limit today. Two parts of it still apply here:
  - a run that swapped is a held sample of its peak;
  - held samples get the same margin as measured ones.
  That PR is not a precondition for this design.
- **Memory gate, for a later memory step (question 2).** Booking memory below today's figure would need, per
  class, no run over 64 runs whose peak exceeded the profile's booking. It would also need a farm signal for a host
  OOM kill, which is not in `/metrics` today and not attributed to a class (`spec.rs:226-241`). A class that
  overruns would go back to today's figure, which includes the overrun with its margin: that is the step-up.

## 5. Rollout

### The switch
- **Farm-wide:** `server.placement_curves` (flag `--placement-curves`) gains `on`. `off` and `shadow` are
  unchanged: `shadow` stays booking-equal to `off`, and the existing test keeps asserting it. `on` needs
  `--agent-pool-config`, as `shadow` does.
  - This is the value `cpu-over-time.md` section 12 reserved, and the refusal text changes to
    `placement_curves must be off, shadow or on`.
  - `on` learns, logs and checks exactly as `shadow` does, and in addition applies the curve test.
- **Canary:** `server.curve_hosts`, a list of host ids. When it is non-empty, the curve test applies only on the
  hosts it names, and every other host is tested as in `shadow`. When it is empty, the test applies on every host.
  - Per pool was considered. The engine is one placer for every pool, and pools already must share one memory
    percent (`crates/js-lease-server/src/config/agent_setup.rs:79-98`). A host list needs no pool-level setting.
  - Per class is automatic (section 4). An operator list of classes was considered and not chosen: there were 5,335
    classes in 4 h, most named by a hash.
- **Hold bound:** `server.curve_max_hold_seconds`, default 30, from 0 to 300. At 0 the test only logs, as in
  `shadow`.
- All three are read once at start, like every placement setting. A change is a restart through the server's
  update hook (`docs/lease-server-update.md`). No setting is reloaded at runtime.

### Stages
1. **Fix the shadow log**, then collect 24 h of check lines (section 4, farm gate 1).
2. **Build the checks and the decide-in-shadow line**, still under `shadow`. Run two working days. `shadow` prints
   the per-class `CPU live` column. The hold lines say how many jobs `on` would hold.
3. **Replay** on recorded days with series. If it misses the pass mark, stop here: the profiles stay in shadow and
   the code for `on` is not built.
4. **`on` with `curve_hosts` naming one host** for one working day, then half the hosts, then empty (all).
5. **Memory** is a separate decision (question 2), not part of these stages.

### Metrics (on `/metrics`)
- Holds are a new reason, `cpu-curve`, of the existing `js_placement_refusals_total{host,reason}`
  (`crates/js-lease-server/src/metrics/render.rs:201`). That is an added label value, not a new metric.
- `js_curve_hold_expiries_total`: jobs placed after the hold bound skipped the test. If this rises, the curve
  test holds jobs it cannot place.
- `js_curve_classes{state}`, a gauge with `state` one of `live` (passing all four checks) or `flat`. `state` is a
  label name the metrics already use (`js_jobs{state}`); its values here are new.
- Watched alongside, and already present:
  - `js_queue_wait_seconds`, the wait the test adds;
  - `js_host_cpu_booked_millicores`, unchanged by design;
  - the hosts' CPU pressure series, which the test aims to cut (`crates/js-lease-server/src/hosts.rs`).
- **The success measure** is `cpu-over-time.md` decision 2, total time (wait plus run) per build, compared over
  matching working days. It is read from the job records, not from a new metric.

### Rollback
- **Set `server.placement_curves` to `shadow`** and restart. The test is gone at the next pass. The profiles go on
  learning, so a later `on` starts with them.
- **Nothing is stored that needs undoing.** Bookings are not changed by `on`, and no format changes: no log line
  is removed, and the new log line and metric values are additive. A job held at the moment of the restart is
  placed by today's rule at the next pass.
- **During a rolling update** two server tasks may overlap, one at `on` and one at `shadow`. Each places by its own
  setting. The worst case is today's placement for the overlap.

## 6. What the build pull requests test

The tests named here are the minimum, for the implementing pull requests. They are listed so the design can be
checked against them, not built in this one.

- **Flat equals today.** With every class flat, `on` books and places the same as `shadow` and `off`, job for job.
  It extends the existing `the_sizes_booked_are_the_same_with_the_profiles_off_and_in_shadow`; it does not
  replace it.
- **A burst-first and a quiet-first job share a host** where two burst-first jobs do not (`cpu-over-time.md`
  section 10).
- **Survival credit.** A class whose runs end at 40 s frees its slots after 40 s. A class whose runs outlive their
  recorded slots is charged its booking.
- **The reading is carried.** A host whose reading exceeds its jobs' expectations keeps the excess at every slot,
  and `L_h(k)` is never below 0.
- **Each check.** Each of the four checks turns a class flat on its own, at its exact boundary: 15 and 16 scored
  runs, 80% and 80.1%, 3 and 4 overruns, 11 and 12 usable lines.
- **The hold bound.** A job held for exactly 30 s skips the test. At 29.999 s it does not. The time comes from the
  injected farm clock. A held job does not stop a later job from taking the host.
- **Canary.** With `curve_hosts` naming host `a`, only `a` refuses by curve.
- **Settings.** `on` is accepted and `On` is refused. `curve_max_hold_seconds` of 301 is refused. `on` without
  `--agent-pool-config` is refused.
- **The shadow table.** It prints the new columns from lines the current release writes. It reads a log of the
  release before (no hold lines) without error.

## 7. Conflicts found while writing this

- **The #1076 commit message says the pack limit defaults to 0.8.** The code, the flag table and
  `placement-by-load.md` say 0.5 (`load.rs:157`, `lib.rs:52`). This document uses 0.5.
- **#1024 (open) assumes a `memory.max` that main no longer writes** (section 4).
- **`crates/js-agent/src/lifecycle/machine/mod.rs:500-502` still says** a job's memory is "held to its booking by
  its lease's `memory.max`". That contradicts `cgroup.rs:315`. A one-line doc fix, not part of this pull request.
- **`cpu-over-time.md` section 4 learns memory "the same way"**, while its section 14 lists memory curves as not
  chosen. This design keeps memory learned in shadow and booked by today's rule. Question 2 settles the rest.
- **`MAX_PROFILES` is 2048** against 5,335 classes seen in 4 h. Only 330 of them had 16 lines, and eviction is by
  the earliest last run, so the classes that can pass check 1 are the ones kept (INFERRED). Question 8.

## 8. Open questions for the owner

1. **Does the CPU booking number change?** It could stay the median (or the heavy-tail p90), or become the profile's
   level.
   **Recommend: keep it.** The replay found no difference in total time (64.8 s against 65.1 s). Changing it changes
   every `cpu.weight` and every pack share. The curve test is where the profile adds information.
2. **Does memory book from the profile, now or later?**
   **Recommend: not now.** Keep today's peak rule, and keep the memory profile in shadow. Revisit only if
   `js_placement_refusals_total` shows `booked-memory` or `predicted-memory` refusing more often than `cpu` over a
   working week. Revisit also only with a host-OOM signal in place, and the memory gate of section 4 passing. A
   memory under-prediction kills jobs that are not its own and teaches no class.
3. **Are the four per-class checks and their thresholds right?** They are: 16 runs, 80%, 3 of 16 runs more than
   25% over the predicted peak, and 12 of 16 lines usable.
   **Recommend: yes, as ASSUMED values to revisit after the decide-in-shadow days.** Checks 1 and 2 exist and are
   already the profile's own rule.
4. **Canary by named hosts, by pool, or by class?**
   **Recommend: named hosts** (`server.curve_hosts`). One engine places every pool, and classes are too many and too
   short-lived to list.
5. **The hold bound, and whether a held job keeps its host.**
   **Recommend: 30 s, and it does not keep its host.** 30 s spreads one burst of arrivals (`cpu-over-time.md`
   decision 4). Not keeping the host means a curve-held job never blocks the queue. The cost: while it is held, later
   jobs may take the room it would have had.
6. **Should "least full" past the pack limit mean least booked, as now, or least busy by reading?**
   **Recommend: least booked, unchanged.** #1076 has just landed. Changing its order in the same step as the curve
   test would make neither measurable.
7. **Gate `on` on the replay's pass mark, and stop at shadow if it is missed?**
   **Recommend: yes.** The one window so far shows 8.4% of classes shaped, removing under 0.3% of the CPU
   prediction error. The live machinery is worth building only if a longer window and the replay show the 10%.
8. **Raise `MAX_PROFILES`?**
   **Recommend: not now.** Eviction removes the classes that run least, which cannot pass check 1 anyway. If the
   24 h window shows evicted classes that had 16 runs, raise it then, after measuring the server's memory per
   profile.

## 9. Considered and not chosen

- **Book the profile's level as the number** (question 1): no measured gain, wide blast radius.
- **Feed the job profiles into js-sched's minute lanes:** a minute holds most jobs' whole lives, so it books the
  peak, not the shape (section 3).
- **Fade the unexplained reading within 10 s** (`cpu-over-time.md` section 5, `B_h`): it admits burst-first jobs
  onto hosts that are busy for a reason the curves do not see. Carrying the excess costs at most a 30 s hold.
- **Charge running jobs their 90th percentile:** it over-counts many jobs whose bursts do not coincide. The reading
  corrects the level, not the peak.
- **A per-class operator allow-list:** thousands of classes named by hash, and it duplicates the automatic checks.
- **Memory curves (remaining maximum)** (question 2): little to free for jobs this short, and a wrong figure can kill
  jobs that are not its own.
- **Throttling or killing on a wrong estimate:** ruled out by the owner (`cpu-over-time.md` section 7).
