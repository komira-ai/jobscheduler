# Booking from the learned profiles: the live mode after `--placement-curves shadow`

Status: built behind `--placement-curves on`, off by default: the four checks, the curve booking, the packing rule
on the curve hosts, the pack limit's fullest slot, the wait bound, `server.curve_hosts` and
`server.curve_max_wait_seconds` with their flags `--curve-hosts` and `--curve-max-wait-seconds`, the metrics of section 5,
the `shadow` table's three columns, a curve arm of the replay that learns from the series the trace carries, the replay
on held-out days (`js-cpu-replay run --learn-days`), and under `shadow` and `on` the memory shadow of section 2
("Memory over time, in shadow"). Not built: the decide-in-shadow line of section 4 for CPU. Not yet run: the replay on
the farm's recorded days.
The code references below are to the commit the design was written at. It follows [cpu-over-time.md](cpu-over-time.md), whose steps 1
(record) and 2 (learn in shadow, komira-ai/jobscheduler#1057) are built, and fixes what its steps 3 (decide in
shadow) and 4 (enforce) book, gate and roll out. Companions: [sizing.md](sizing.md) (how a class is booked today)
and [placement-by-load.md](placement-by-load.md) (the load predicate and the pack limit).

Every code reference is `path:line` on main at komira-ai/jobscheduler#1083 (`a4378c842`). Labels: **READ**, read
in the code at that commit. **MEASURED**, a figure measured on the farm or in an earlier pull request, named where
it is used and not pinned by a test. **REPLAY**, printed by `js-cpu-replay`. **ASSUMED**, a choice not yet tested.
**INFERRED**, reasoned from the code, not run.

## 1. First page

### What is asked
The owner's direction: the scheduler predicts each job's CPU from current and historical use and books that
prediction. A class whose use has a shape over time books that curve, and placement packs the curves so that the
bursts of the jobs on one host do not fall in the same seconds. A class with no reliable shape books a flat number
near its sustained level, not its peaks. Placement packs the fullest host to half of its bookings and then places on
the least full host (komira-ai/jobscheduler#1076), within the 1.1x CPU slack. This document says what the live mode
books, how it combines with the host's readings, what each class must show in shadow before it books its curve,
what it books until then, and how the mode is turned on and back off.

### Why: the farm is full by booking, not by use (MEASURED)
- **The hosts are booked full.** In a read-only measurement on the farm's real traffic, Linux hosts held 85 to 89
  booked cores of 96.8 (88 cores x 1.1), while 10 to 22 cores were busy.
- **Jobs wait.** Submitted to placed took 555 s at the median and 1,008 s at the 90th percentile.
- **One compile class shows why.** Over 64 runs, read from the job service's log:
  - mean CPU 4,250m, profile level 4,312m;
  - five-second peak 55,312m, and the shadow profile's predicted peak 78,125m;
  - booked 22,000m: the heavy-tail rule (komira-ai/jobscheduler#1074, `crates/js-sizing/src/summary.rs:517-535`)
    books its peaks' 90th percentile, held to a quarter of an 88-core host (`HOST_CPU_SHARE`,
    `crates/js-sched-placer/src/sizes.rs:185`).
  - So it books five times what it uses on average, because its compiler bursts to about 55 cores for seconds.
- **CPU is a share, not a limit.** A job's CPU booking becomes its `cpu.weight`, 100 per core
  (`crates/js-contain/src/cgroup.rs:73`), and no `cpu.max` is written unless the operator sets `--cpu-ceiling`. A
  burst that meets another burst runs slower. It is not throttled at its booking, and nothing is killed. Booking
  the peak buys nothing a level plus the slack does not, except where bursts coincide. Keeping bursts apart is what
  the curve is for.

### Two steps
1. **Flat at the sustained level, now.** A separate change, out of this pull request, stops booking CPU from
   five-second peaks. Every class books the median of its last 32 runs' mean CPU, rounded up to a quarter core.
   That is the flat number this design builds on. Section 2 says what follows if that change lands with another
   rule.
2. **Curves, this design.** A class that passes its checks books its curve over time. Every other class books the
   flat number. One booking function serves both, with two outcomes per class: flat or curve.

### The design, in ten lines
1. **A shaped class books its curve.** For each 2 s slot of its first 128 s, the booking is the slot's 90th
   percentile over the class's recent runs. From 128 s on it is the class's tail level. Each slot is held to a
   quarter of the host (section 2).
2. **Every other class books one flat number**, its sustained level. This covers unshaped classes, classes still
   gated, evicted classes, and classes with no series. The margin is the host's 1.1x slack, not a margin per job
   (section 2, question 1).
3. **A cold class** books `--cold-cpu-millis` and `--cold-memory-bytes` (and the `-on <os>` tables), as today
   (section 2).
4. **The packing rule.** A job fits a host when, at every 2 s slot of the next 128 s and at the slot from 128 s
   on, the host's booked CPU plus the job's booking stays within cores x 1.1. On a host where curves are booked,
   this replaces the check of summed flat CPU numbers, so a curve can place more jobs than flat as well as fewer
   (section 3).
5. **Current use.** The host's booked CPU at a slot is the curve or flat number of each job on it, at that job's
   age. The jobs are weighted by how many of their class's runs were still running at that age. On top of that
   sum goes whatever the host's readings show beyond it (komira-ai/jobscheduler#1055), carried to every slot
   (section 3).
6. **The pack limit** reads the host's largest booked slot over the horizon, with the job added. With every class
   flat this is today's sum of bookings (section 3).
7. **Memory stays one number per class**, from the high end of its recent peaks (`sizing.md`, 7.7). A wrong memory figure can kill a job on a
   busy host, and that kill teaches no class (section 2).
8. **Per class, curve booking is automatic.** A class books its curve only while it passes four checks, which
   `js-cpu-replay shadow` prints too. A class that fails one books flat at the next pass (section 4).
9. **Farm-wide, the switch is an operator's.** `server.placement_curves = "on"` turns it on, first on the hosts named
   in `server.curve_hosts`. It goes on after a day of check lines, two days deciding in shadow, and a replay that
   shows no loss against the flat step. `shadow` plus a restart undoes it (section 5).
10. **When a curve is wrong**, nothing running is throttled, killed or moved. The next reading charges the host what
    it uses, and the class fails its checks and books flat. A job that waits 30 s for its curve to fit is tested
    with its flat number instead (section 4).

### What this buys, and what it does not (INFERRED)
- **Against today's peak booking,** both steps place far more jobs per host. The compile class above books about
  4.5 cores flat instead of 22.
- **Against the flat step,** the curve mode packs by the moment, not by the average: "1.0x to 1.1x of the cores
  at every moment, not only on average" (`cpu-over-time.md` section 1). It cuts two ways:
  - **Fewer** jobs fit where shaped jobs' bursts would meet. The flat step lets them meet, and they run slower.
  - **More** jobs fit where the jobs on a host are past their bursts, or near their class's usual end. The flat
    step counts every job at its full level until it ends.
  - Which way wins on the farm is not known. The replay gate (section 4) measures it before `on`.
- **How large that is, is not known yet.** In the one window measured so far, 28 of 330 classes (8.4%) were shaped
  by CPU (MEASURED, komira-ai/jobscheduler#1077). The replay gate in section 4 measures the effect before `on`.

### What changes where (for the build pull requests)
| Part | Change |
|---|---|
| `js-sizing` | The profile already answers `slot_p90`, `slot_runs`, `tail`, `level`. It gains, beside its window of 16 scores, a ring of the last 16 scored runs' peak-overrun flags (check 3) and a ring of the last 16 runs' CPU series status (check 4), and pure functions reading them. It answers how many kept runs went past 128 s (`tail_runs`, from each kept run's tail seconds). |
| `js-sched-placer` | `Curves` gains `On`. `JobSizes` answers, per job, flat or curve with the figures. A per-host table of 65 slots (64 of 2 s, then one from 128 s on). The packing test in `try_job`, in place of the CPU axis of `Room::fits` on a curve host. A first-waited time per queued job, in memory. One log line. |
| `js-sched` | The pack limit's CPU share can read a per-host figure the placer gives (the largest booked slot) in place of the summed bookings. One read-only accessor for a host's slot-0 CPU charge (`cpu_left`, `crates/js-sched/src/admission/horizon.rs:57`). The minute horizon used for leases is unchanged. |
| `js-lease-server` | The `on` value, `server.curve_hosts`, `server.curve_max_wait_seconds` and their flags `--curve-hosts` and `--curve-max-wait-seconds`, metrics (section 5). |
| `js-cpu-replay` | `shadow` prints the four checks per class. The trace carries series, so the replay can run the curve mode and its held-out-day gate (section 4). |
| Agent, wire, store | Nothing. A job's stored booking and its `cpu.weight` are its flat number (section 2). |

### Decisions needed
Section 8 has the questions for the owner, each with a recommendation. In short:
1. The flat number is the sustained level with no margin per job; the 1.1x slack is the margin: yes.
2. On curve hosts, the slot-by-slot rule replaces the check of summed flat CPU numbers: yes.
3. A curve slot books the 90th percentile of the class's runs at that age, held to a quarter of the host: yes.
4. Memory stays on today's rule: yes.
5. The four per-class checks and their thresholds: yes, as starting values.
6. Canary on named hosts: yes.
7. After 30 s waiting for its curve to fit, a job is tested with its flat number: yes.
8. The pack limit reads the largest booked slot: yes.
9. Gate `on` on a replay that shows no loss against the flat step: yes.
10. Leave `MAX_PROFILES` at 2048 for now: yes.

## 2. What the live mode books

### Where stands (READ)
- **A booking is decided at each pass** and never stored on the job. Only the allocation records what it was
  booked (`sizes.rs:59-62`).
- **The profile** (`crates/js-sizing/src/profile.rs`):
  - It keeps a class's last 64 successful runs, weighted by `2^(-age / 6 h)` (`profile.rs:17-19`).
  - For each 2 s slot of ages 0 to 128 s it gives a load-weighted 90th percentile (`slot_p90`, `:273`) and how many
    kept runs reached the slot (`slot_runs`, `:278`). After 128 s it gives one decayed mean (`tail`, `:284`). Over
    the whole run it gives a decayed mean (`level`, `:259`). CPU slots are means of their seconds (`:65-70`).
  - A class is shaped when its curve's error over its last 16 scored runs is at most 80% of the flat level's
    (`profile.rs:25-27`, `shaped`, `:337`). No run is scored before 8 kept runs (`:21`).
  - At most 2048 classes are kept (`MAX_PROFILES`, `crates/js-sched-placer/src/sizes_profile.rs:30`). They are
    rebuilt from the job records at a start, and records are kept 7 days by default
    (`crates/js-lease-server/src/config/retention.rs:11`).
  - Shadow books nothing. The test `the_sizes_booked_are_the_same_with_the_profiles_off_and_in_shadow`
    (`crates/js-sched-placer/src/sizes_profile_tests.rs:663`) pins it, and it keeps pinning `shadow` against `off`.

### A shaped class: its curve
For a job `n` of a class that passes the four checks of section 4, its booking at its own age slot `s` is:
```
B_n(s) = clamp( slot_p90(s) )   for s in 0..63, while slot_runs(s) >= 4   (MIN_SLOT_RUNS, profile.rs:23)
       = clamp( flat_n )        for s in 0..63, once fewer than 4 kept runs reached s
B_n(H) = clamp( tail )          the slot "from 128 s on", when tail_runs >= 4
       = clamp( flat_n )        otherwise
clamp(x) = min( max(x, 250m), host_share )      host_share = a quarter of the host's cores (sizes.rs:185)
```
- **Why the 90th percentile per slot.** A booking says how much a job may need in that slot. The mean per slot
  would book the typical run, and every other run of the class would push its host over in its burst. The 90th
  percentile across runs at the same age is the class's peak where the peak is regular, and stays near the level
  where it is not. It is the start shape `cpu-over-time.md` section 4 designed for admission.
- **Why the tail is a mean, not a 90th percentile.** The profile keeps one decayed mean past 128 s per class
  (`profile.rs:284`), and only about 7.5% of jobs run that long (MEASURED, `cpu-over-time.md` section 3). Placement
  acts at a job's start, where the shape is kept. Past 128 s a job is booked its class's level there, as the flat
  step books a whole run. A class with fewer than 4 kept runs past 128 s books its flat number there.
- **Where the gate of 4 runs counts runs.** `MIN_SLOT_RUNS` counts kept runs, not decayed weight, as the profile
  does today. A slot reached by 3 kept runs books flat; one reached by 4 books its 90th percentile.
- **Why held to a quarter of the host.** It is the bound every CPU booking has today. Four bursts of shaped classes
  can still meet on an 88-core host, and the slack absorbs that. A burst above a quarter of the host runs slower,
  which CPU allows.
- **The compile class above.** If its burst comes at the same age in most runs, it is shaped. It then books about
  4.3 cores in its quiet slots and 22 cores in its burst slots, so two such compiles share a host freely while
  their bursts fall apart. If the burst moves from run to run, it is not shaped, and it books its flat 4.5 cores.

### Every other class: one flat number
- **What it is.** `flat` is the class's sustained level: the median of its last 32 runs' mean CPU, rounded up to a
  quarter core and held to a quarter of the host (`median_cpu` and `booking`, `summary.rs:539-556`). The step-one change makes it
  every class's booking, in place of the heavy-tail p90 of five-second peaks.
  - If that change lands with another sustained-level rule (for example the profile's `level`, or a margin), `flat`
    here is that rule. This design depends on `flat` being near the sustained level, not on which estimator gives
    it.
- **No margin per job.** The margin is the host's: `--cpu-slack` (default 1.1, `s_cpu_permille`,
  `crates/js-sched/src/admission/config.rs:123`) lets a host's bookings reach 1.1x its cores. A margin per job on
  top would book every host x% emptier for the same effect. Question 1 asks to confirm.
- **Who books it:**
  - a class that fails any of the four checks, including one with fewer than 16 scored runs, so while a class is
    gated it books flat;
  - a class with fewer than 8 kept profile runs;
  - a class evicted past 2048;
  - a class whose runs carry no usable series: an agent without `--cpu-series`, or runs whose series disagree with
    their CPU time. In the measured window, 17% of check lines carried no series (`cpu-over-time.md` section 10);
  - every class while the mode is `off` or `shadow`.
- **A job's stored booking is always its flat number.** It is the figure the allocation records, the agent's
  `cpu.weight`, and what `Capacity::has_room` sums (`crates/js-jobs/src/record.rs:594`). The curve exists only in the placer, rebuilt each pass from
  the profile. So the agent, the wire and the store see one number, as today.

### A cold class
- **No run of its own, its fallback class or its client.** It books `--cold-cpu-millis` (default 22 cores) and
  `--cold-memory-bytes` (default 4 GiB), or the `--cold-*-on <os>` entry for its os (`sizes.rs:161-166`,
  `crates/js-lease-server/src/lib.rs:54-56`). This is unchanged, as is the scheduler's cold cap.
- **A cold class books flat** at its cold size at every slot. A class's first runs move it from cold to flat after
  one run, and to its curve only after 16 scored runs and the checks.

### Memory: one number, from the peak
- **CPU over its booking runs slower.** Memory over its booking can kill.
  - A lease has no `memory.max` (`cgroup.rs:315`), so a host short of memory reclaims, swaps and then OOM-kills a
    victim, which may be another job.
  - The agent reports that kill as an ordinary exit: "a node short of memory is not the booking's fault"
    (`crates/js-agent/src/crun/spec.rs:225-241`). So no class learns from it.
- **So memory books today's figure in every mode.** That was the largest non-file peak over the last 257 to 288
  runs, times 1.2 plus 128 MiB, rounded up to 256 MiB, at least 1 GiB, never below the class's kill floor
  (komira-ai/jobscheduler#1035); it is now the 95th percentile of the last 32 runs' peaks in its place (`sizing.md`, 7.7).
- The memory profile, which keeps 64 runs with a 6 h half-life, stays in shadow.
- **Memory over time is not booked either.** Freeing a running job's memory as its curve falls is the memory half
  of the curve idea. For jobs of which 92.5% end within 128 s (MEASURED, `cpu-over-time.md` section 3) it frees
  little, and a wrong figure costs a kill. Question 4. The owner has since asked for placement by memory curves too;
  the next subsection is the shadow half of that, which books nothing and measures what enforcing would give and
  risk.

### Memory over time, in shadow
Built under `shadow` and `on` (`crates/js-sched-placer/src/memory_shadow.rs`, `crates/js-sizing/src/memory_curve.rs`).
Beside the flat memory booking, which still places every job, the job service computes what a booking that follows
each class's memory profile would allow. Nothing it computes reaches a placement, a refusal, a commit or an agent:
no job is placed, moved, throttled or killed by it, and the start path is unchanged.
- **A class's memory curve** (`MemoryCurve::of`): for each 2 s slot of the first 128 s, the load-weighted 90th
  percentile of the slot's largest second over the class's kept runs, and from 128 s on the 90th percentile of the
  runs' peaks after 128 s (`Profile::tail_p90`; the tail mean is no bound on what a run holds at once). Each slot
  takes the flat booking's margin (`1.2 x p90 + 128 MiB`) and is held to at most the class's flat memory booking. A
  slot fewer than 4 kept runs reached books the flat booking. **A class with too little history books flat:** until
  its memory profile has scored 16 runs (24 kept), it has no curve.
- **A host's projection:** 65 slots (64 of 2 s, then from 128 s on), `P(k) = sum_j M_j(k) + R`. A running job is
  projected to hold its class's booking at the slot it will be in, unweighted by survival (memory a job may still
  hold is held in full); nothing at a slot no kept run reached (every run of its class had ended by then); and its
  flat booking throughout once it has outlived its class's recorded runs, when its class has no curve, or when it
  named its memory. `R` is what the host's last reading (at most 15 s old) showed beyond `sum_j M_j(0)`, carried to
  every slot: a job above its profile, or anything else on the host, counts against every slot.
- **The fit rule, with its margin:** a queued job fits a host by curve when `P(k) + B(k)` is within the memory the
  room holds the host to at every slot, `B` being its own curve's booking (unweighted, its whole curve), and the
  room has space for it on every other axis. The scheduler's load test is not run for it.
- **Per decision:** a job placed nowhere while its curve fitted a host the flat memory booking refused starts a
  time; its placement then counts `sooner`, and logs `placement curves: a job's memory curve would have placed it
  sooner` with `job`, `class`, `host`, `sooner_ms`. A placement whose curve does not fit the chosen host counts
  `later` and logs `... would not have placed it on its host` with the first `slot` over. Every other placement on a
  host with a memory capacity counts `same`. Lines go to the target `js_sched_placer::sizes::profile`.
- **Per host:** each reading is compared with the projection the last walk made of its moment, and one above it
  counts too low, by how much, and logs `placement curves: a host held more memory than its jobs' memory curves
  projected` with `host`, `projected_mib`, `measured_mib`, `ahead_ms`. The measure is the host's non-file memory,
  so it includes what is not a job.
- **Operator read:** `/metrics` writes `js_memory_curve_placements_total{verdict}`,
  `js_memory_curve_sooner_milliseconds_total`, and per host `js_host_memory_projected_bytes`,
  `js_host_memory_measured_bytes`, `js_host_memory_projection_checks_total`, `js_host_memory_projection_low_total`
  and `js_host_memory_projection_short_bytes_total`, all only under `shadow` and `on`.
- **Not gated on overruns, unlike CPU's check 3.** The profile's overrun flag compares a run's peak with its start
  slots' 90th percentile only, so every long run whose peak comes after 128 s counts as one. The shadow's
  too-low counts measure calibration instead; an enforcing step chooses its gate from them.
- **What the recorded data says (MEASURED, one hour of usage records: 10,191 runs with a peak, no memory series).**
  The usage records carry each run's peak but not its memory series, which only the job store holds, so this
  bounds the tail and cannot measure the shape:
  - Runs past 128 s: 602, holding 76.6% of the booked memory-time (2,729,122 of 3,562,174 GiB-s); their part past
    128 s is 2,119,554 GiB-s.
  - The most a tail booking from peaks can give back, each long run booking its own peak with the margin past
    128 s: 189,517 GiB-s, 8.9% of the long runs' tails and 5.3% of all booked memory-time.
  - With the classes approximated by their booking (cpu, memory) pair, the record holding no class: 83 long runs
    had a class with 24 earlier runs, 4 of them past 128 s, within the hour. Their tail booking gives back 16,883
    GiB-s (4.5% of their tails), and it was too low for 43 of them (51.8%), short by 1,381 MiB at the median and
    4,131 MiB at the 90th percentile. Leaving out a class with more than 3 of its last 16 runs over 1.25x the
    projection: 49 judged, 8,785 GiB-s given back, 9 too low (18.4%), short by 2,418 MiB at the median.
  - So a memory curve that is only a tail peak gives back little, and its projection is too low often. What the
    shape inside a run would give back, and how often the projection of a whole host is too low, is what the
    shadow's counts measure on the farm.

## 3. The packing rule, and current use

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

### Live: the host's booked CPU at each slot
Slots `k` are 0 to 63 (2 s each, the next 128 s) and `H` (from 128 s on). A job `j` on host `h` is at age slot
`i_j`, and `s = i_j + k` is the age slot that slot `k` falls in. Its charge `C_j(k)` is, in order, the first case
that applies:
```
flat job:                                   C_j(k) = flat_j                                  every k and H
curve job, i_j < 64, slot_runs(i_j) >= 4    (its own age is inside the recorded runs), with w(x) = x / slot_runs(i_j):
    k < 64, s < 64, slot_runs(s) >= 4:      C_j(k) = B_j(s)  * w(slot_runs(s))
    k < 64, s < 64, slot_runs(s) <  4:      C_j(k) = flat_j  * w(slot_runs(s))           few or no runs reached s
    k < 64, s >= 64, or k = H:              C_j(k) = B_j(H)  * w(tail_runs)              past 128 s
curve job otherwise                         (past 128 s already, or past its recorded runs):
                                            C_j(k) = B_j(H) if i_j >= 64, else flat_j     every k and H
```
- `w` is never a division by 0: a job whose own age slot has fewer than 4 kept runs takes the last case.
- `w(x)` is at most 1, since no more runs reach a later age than an earlier one.
- A slot no kept run reached charges 0. The class's runs had all ended by then.
The host's booked CPU and the residual are then:
```
R_h    = max( 0, L_h(0) - sum_j C_j(0) )       L_h(0) above, with each job charged C_j(0)
L_h(k) = sum_j C_j(k) + R_h
```

### The packing rule
A job `n` fits host `h` when both hold:
```
1. for every k in 0..63 and H:   L_h(k) + B_n(k)  <=  cores_h x (1 + s_cpu)       CPU, slot by slot
2. the existing checks, unchanged: Capacity::has_room for memory, disk and GPUs, and js-sched's predicate
```
- **Rule 1 is the owner's packing.** Two shaped jobs whose bursts would meet in the same slot do not both fit. Two
  whose bursts fall apart do. A flat job fills every slot evenly. The cap is the existing slack, the same as the
  slot-0 predicate's, so nothing is booked past it at any slot (AGENTS.md B14).
- **Rule 1 replaces two CPU checks on a curve host.** These are the CPU axis of `Capacity::has_room`, the sum of
  stored flat numbers (`crates/js-jobs/src/record.rs:594`, through `Room::fits` in
  `crates/js-sched-placer/src/room.rs`), and js-sched's slot-0 CPU test.
  - Rule 1's slot 0 is that test, with each job charged `C_j(0)`.
  - Its slot `H` bounds the long run: every job past 128 s at its class's level.
  - Keeping the flat sum beside it would count every job at its full level until it ends. A curve could then only
    turn jobs away, never use the cores a past burst frees, which is the point (question 2).
  - On a host not named in `curve_hosts`, and in `off` and `shadow`, both checks stay as they are.
- **The new job is charged its whole curve,** unweighted. It is the job being admitted, and its burst is what the
  test is for. Once placed, it is a running job and is weighted by survival like the others.
- **With every class flat,** every `C_j` and `B_n` is constant, so rule 1 reads `L_h(0) + flat_n <= cap` at every
  slot. That is today's slot-0 test with the flat numbers. `L_h(0)` is at least the sum of the bookings at the default
  `cpu_floor_permille` of 1000, since `F_covered`, `U_seen` and `U_unseen` count every job once, so the flat sum check adds nothing to it. The build's
  first test asserts this equality: `on` with every class flat places as `shadow` does.

### Current use: readings plus predicted remaining use
- **Survival.** `slot_runs(s) / slot_runs(i)` is the share of the class's kept runs that reached age `s`, out of
  those that reached the job's current age. A job whose class mostly ends at 40 s books little at 60 s. A job that
  has outlived its recorded runs (fewer than 4 kept runs reached its age) books its flat number at every slot until
  it ends.
- **The residual is carried to every slot, not faded.** `R_h` is what the host's readings show beyond its jobs'
  bookings at their current ages: other work, or a job above its curve. `cpu-over-time.md` section 5 let it fade
  within 10 s (`B_h`). Here it stays. A fade would book a burst onto a host that is busy for a reason the curves
  do not see. Carrying it costs at most a wait, bounded in section 4. Section 9 records the change.
- **Ages.** A job's age is the time since its agent reported it started. A job allocated but not yet started is age
  0. The table is rebuilt for each host at every reading the scheduler accepts, and each placement adds its job's
  charges. This is the rhythm of js-sched's per-minute lanes (`crates/js-sched/src/admission/profile.rs:1-25`).
- **Why not js-sched's minute lanes.** js-sched already charges leases over a 30-minute horizon of 1-minute slots
  (`HORIZON`, `crates/js-sched/src/load.rs:127`). The job placer feeds them nothing (`life_minutes: 1`,
  `crates/js-sched-placer/src/jobs.rs:532`). Most jobs end within 128 s, so a minute slot would hold a job's whole
  life and book its peak for it. The 2 s table lives in `js-sched-placer`.
- **Memory current use** is unchanged: the larger of the summed bookings and the working set plus the uncovered
  jobs' bookings, at slot 0 (`horizon.rs:80-84`).

### The pack limit and the 1.1x slack
- **Slack:** rule 1's cap is the existing one, `cores x (1 + s_cpu)`. A larger `--cpu-slack` loosens it.
- **Pack limit** (default 0.5, `DEFAULT_PACK_LIMIT_PERMILLE`, `load.rs:157`):
  - It orders hosts and never refuses one (`load.rs:338-372`).
  - It reads a host's "booked share", the larger of CPU and memory with the new job added. Its CPU share becomes
    `max over k of (sum_j C_j(k) + B_n(k)) / cores_h`: the fullest slot of the host's booked curve. The residual
    `R_h` is a reading, not a booking, and stays out of it, as readings stay out of the share today.
  - So "fill a host to half its booking" means half at its fullest booked moment, and "least full" past it means
    least full at that moment.
  - With every class flat it is today's share, since every slot is the same.
  - The memory share is unchanged.

## 4. Gates, and the fallback when a profile is wrong

### Per class: four checks, computed by the job service at each pass
A class books its curve only while all four hold. While any fails, the class is gated and books its flat number.
- Checks 1 and 2 are the profile's own `shaped` rule, over the window of 16 scores it keeps (`profile.rs:322-337`).
- That window holds only each scored run's curve and flat errors (`profile.rs:190`), so checks 3 and 4 need new
  per-class state in `js-sizing`:
  - a ring of the last 16 scored runs' peak-overrun flags;
  - a ring of the last 16 runs' CPU series status, scored or not.
  Both are rebuilt with the profile from the job records at a start.
- `js-cpu-replay shadow` computes the same four from the log, so an operator reads in its table what the service
  decides.

| # | Check | Threshold | Printed by `shadow` today |
|---|---|---|---|
| 1 | Enough runs | 16 scored runs (`CHECK_RUNS`) | yes, `CPU scored` |
| 2 | Shape pays | curve error at most 80% of flat (`FIT_PERMILLE`) | yes, `CPU curve/flat %` and `CPU shaped` |
| 3 | Peak calibrated | at most 3 of the 16 runs peaked more than 25% above their predicted peak | no: computable from `cpu_peak_millicores` and `cpu_predicted_peak_millicores` in each line |
| 4 | Series usable | at least 12 of the class's last 16 check lines have a CPU status `learned` or `not-learned` | no: computable from the `cpu` status in each line |

- **Check 3 matters more once the curve is a booking.** Under-booking a burst slot is what lets two bursts meet.
  - The predicted peak is the largest per-slot 90th percentile over the slots the run reached
    (`profile.rs:434-436`). It is not the 90th percentile of the runs' peaks, so its overrun rate is not 10% by
    construction, and it is not yet measured.
  - A class whose burst lands in a different slot each run has a low 90th percentile in every slot, and most of
    its runs overrun it. Check 3 catches exactly that class, and it books flat.
  - 3 in 16 with a 25% margin is ASSUMED, to be set from the decide-in-shadow days.
- **Check 4, ASSUMED.** In the measured window, 44% of the lines' series disagreed with the exact CPU time
  (MEASURED, #1077). A class that learns from only a minority of its runs learns from a biased minority, for
  example only its long runs.
- **`shadow` gains three columns:** `CPU peak over %`, `CPU series usable %`, and `CPU curve`, which is `yes` when
  all four hold at the class's last line. No new log field is needed.

### Farm-wide, before `on`
1. **A day of check lines.** At least 24 h of normal builds, from every server task, sorted by time
   (`cpu-over-time.md` section 10).
   - A server start logs no check until it has taught its ended jobs: minutes on the farm's store. The line
     komira-ai/jobscheduler#1083 added marks when the checks begin.
   - The one window measured so far is 4 h 10 min. A class needs 16 scored runs, and most classes run less often
     than that.
2. **Decide in shadow, two working days.** In `shadow`, the service also runs the packing rule on the curves, beside
   the flat placement it makes. For each job whose curve rule would choose a different host, or no host, it logs
   one line:
   `a job's curve booking would place it differently`, with `job`, `class`, `flat_host`, `curve_host` (absent when no
   host fits) and `slot` (the first slot over the cap).
   - This says how many jobs the curves move or delay, and which classes, before any job is placed by a curve.
3. **The replay.** On recorded days held out from the learning days, the curve mode against the flat step must
   show three things:
   - total time (wait plus run) no worse, by more than the spread between two recorded days;
   - throughput within 2%;
   - fewer core-seconds asked of hosts past their cores, which is what overlapping bursts is for.
   This replaces `cpu-over-time.md` decision 2's 10% pass mark, which compared against peak booking (question 9).
   The trace carries each job's series for this (`crates/js-cpu-replay/src/trace.rs`, header `trace 2`), and
   `js-cpu-replay run <trace> --learn-days 2` prints the three, with a fourth: at least one held-out job booked by
   curve, so a trace without series cannot pass on the curve mode's other ranking of hosts alone
   (`crates/js-cpu-replay/src/gate.rs`).

### When a CPU curve is wrong
- **Nothing running is throttled, killed or moved** (the owner's ruling, `cpu-over-time.md` section 7). A host whose
  jobs' bursts do meet runs above its cores until they pass.
- **Within one reading** (5 s), what the host uses beyond its jobs' bookings enters `R_h`, which is charged at
  every slot. The next job is tested against what the host does.
- **Within 16 scored runs,** the class fails check 2 or 3 and books flat at the next pass. Check 3 usually fails
  sooner: four overruns are enough.
  - A run with no usable series adds no score, so it does not move checks 2 and 3.
  - Check 4 bounds that: at 5 unusable runs of the last 16, the class books flat.
- **The wait bound.** A job of a curve class that has waited `server.curve_max_wait_seconds` (default 30 s) with no
  host fitting its curve is tested with its flat number, at every slot, from then on.
  - It waits as any queued job does, under today's queue order, including the walk's hold of a host for the first
    job that waits on it (`sizes.rs:182-184`).
  - The bound covers the delay the curve adds over the flat step. A job can still wait under the flat test for
    room, as any job can.
  - 30 s spreads one burst of arrivals (`cpu-over-time.md` decision 4). CPU being a share, starting a job whose burst
    may meet another is better than holding it longer.

### When a memory figure is wrong
The live mode books today's memory figure, so its fallback is today's:
- **A run above its booking** reports its whole peak; there is no `memory.max` to hold it. Its class's next booking
  is that peak x 1.2 + 128 MiB, rounded up to 256 MiB, at once (#1035, `summary.rs:493-509`).
- **A run that reached its booking** is a held sample, sized with the same margin. Its class books a step above the
  booking next time, and keeps stepping while its runs reach it (komira-ai/jobscheduler#1024). The placer judges
  whether a run reached its booking (`limit_hits` from `reached`, `sizes.rs:649-653`).
- **A run killed for its own limit** raises its class's floor to twice the booking it died at (`sizes.rs:86-89`).
- **For a later memory step (question 4):** booking memory below today's figure would need, per class, no run over
  64 runs whose peak exceeded the profile's booking. It would also need a farm signal for host OOM kills. Neither
  `/metrics` nor any class sees those today (`spec.rs:225-241`).

## 5. Rollout

### The switch
- **Farm-wide:** `server.placement_curves` (flag `--placement-curves`) gains `on`.
  - `off` and `shadow` keep booking every class flat, and the existing test keeps asserting it.
  - `on` needs `--agent-pool-config`, as `shadow` does, and learns, logs and checks exactly as `shadow`.
  - The refusal text becomes `placement_curves must be off, shadow or on`. This is the value
    `cpu-over-time.md` section 12 reserved.
- **Canary:** `server.curve_hosts` (flag `--curve-hosts`, host ids separated by commas), a list of host ids.
  - Given: curves are booked only for jobs placed on the hosts it names. On every other host, every job is
    booked flat.
  - Absent: curves are booked on every host. An empty list is refused at start, as are an empty name, a name twice
    and a name that is not a host id, each refusal naming the key or the flag and quoting the name. The flag and
    the key are read by the same code (`crates/js-lease-server/src/config/placement_curves.rs`), so a server
    configured by flags alone, as the live one is, can run the canary.
  - Per pool was considered. One engine places every pool, and pools must already share one memory percent
    (`crates/js-lease-server/src/config/agent_setup.rs:79-98`). A host list needs no setting per pool.
  - Per class is automatic (section 4). An operator list of classes was not chosen: there were 5,335 classes in 4 h,
    most named by a hash.
- **Wait bound:** `server.curve_max_wait_seconds` (flag `--curve-max-wait-seconds`), default 30, from 0 to 300. At 0
  a curve never delays a job.
- **All three are read once at start**, like every placement setting. A change is a restart through the server's
  update hook (`docs/lease-server-update.md`).

### Stages
1. **The flat step** lands and is measured on the farm (the separate change).
2. **The checks and the decide-in-shadow line** land under `shadow`. Collect a day of check lines, then two working
   days of decide-in-shadow lines. `shadow` prints the per-class `CPU curve` column.
3. **The replay** on recorded days with series. If it shows a loss against the flat step, stop here: the profiles
   stay in shadow and the code for `on` is not built.
4. **`on`**, with `curve_hosts` naming one host for one working day, then half the hosts, then empty (all).
5. **Memory** is a separate decision (question 4), not part of these stages.

### Metrics (on `/metrics`)
- **Curve waits** are a new reason, `cpu-curve`, of the existing `js_placement_refusals_total{host,reason}`
  (`crates/js-lease-server/src/metrics/render.rs:201`). That is an added label value, not a new metric.
- **`js_curve_wait_expiries_total`:** jobs tested flat after the wait bound. If this rises, curves are booking
  more than hosts can give.
- **`js_curve_classes{state}`:** a gauge with `state` one of `curve` (passing all four checks) or `flat`. `state` is
  a label name the metrics already use (`js_jobs{state}`); its values here are new.
- **`js_host_cpu_booked_peak_millicores{host}`:** the fullest slot of each host's booked curve. It sits beside the
  existing `js_host_cpu_booked_millicores`, which stays the sum of flat numbers.
- **Memory shadow, under `shadow` and `on` only** (section 2, "Memory over time, in shadow"):
  `js_memory_curve_placements_total{verdict}` (`same`, `sooner`, `later`), `js_memory_curve_sooner_milliseconds_total`,
  and per host `js_host_memory_projected_bytes`, `js_host_memory_measured_bytes`,
  `js_host_memory_projection_checks_total`, `js_host_memory_projection_low_total`,
  `js_host_memory_projection_short_bytes_total`. Nothing is booked from them.
- **Watched alongside, already present:**
  - `js_queue_wait_seconds`;
  - `js_host_cpu_booked_millicores` against the hosts' busy cores (the gap that started this);
  - the hosts' CPU pressure series, which the curves aim to cut (`crates/js-lease-server/src/hosts.rs`).
- **The success measure** is total time (wait plus run) per build, against the flat step over matching working
  days, read from the job records.

### Rollback
- **Set `server.placement_curves` to `shadow`** and restart. Every class books flat at the next pass. The profiles
  go on learning.
- **Nothing stored needs undoing.** A job's stored booking is its flat number in every mode. No format changes: the
  new log line and metric values are additive.
- **A host packed by curves may hold more flat bookings than its cores x 1.1.** After a rollback, the flat checks
  refuse it new jobs until enough of its jobs end. The longest is the longest run on it. No job is moved or stopped.
- **During a rolling update,** two server tasks may overlap, one at `on` and one at `shadow`. Each places by its own
  setting. The one at `shadow` sees curve-packed hosts as full, as after a rollback.

## 6. What the build pull requests test
These are the minimum tests, for the implementing pull requests. They are listed so the design can be checked
against them, not built in this one.
- **Flat equals the flat step.** With every class flat, `on` places the same as `shadow` and `off`, job for job. It
  extends `the_sizes_booked_are_the_same_with_the_profiles_off_and_in_shadow`; it does not replace it.
- **Bursts apart.** Two jobs of a class bursting at 20 s do not fit together on a host where one of them and a job
  bursting at 60 s do.
- **The clamp.** A slot's 90th percentile above a quarter of the host books a quarter.
- **Survival.** For a running job of a class whose kept runs all end by 40 s, the slots past 40 s charge 0. A job
  whose own age fewer than 4 kept runs reached charges its flat number at every slot. The boundary pair is 3 and 4
  runs reaching a slot, asserting the booking and the charge on each side.
- **The tail.** A class with 3 kept runs past 128 s books its flat number from 128 s on; one with 4 books its tail.
- **The residual.** A host whose reading exceeds its jobs' bookings keeps the excess at every slot, and `R_h` is
  never below 0.
- **Past the flat sum.** On a curve host, a job whose curve fits every slot is placed even when the host's flat
  numbers with it pass the cap. On a host not in `curve_hosts`, it is refused.
- **Each check.** Each of the four checks turns a class flat on its own, at its exact boundary: 15 and 16 scored
  runs, 80% and 80.1%, 3 and 4 overruns, 11 and 12 usable lines.
- **The wait bound.** A job waiting exactly 30 s on the injected farm clock is tested flat; at 29.999 s it is not.
- **The pack share.** With curves, it equals the fullest booked slot. With every class flat, it equals today's.
- **Canary.** With `curve_hosts` naming host `a`, only jobs on `a` are booked by curve. Through the flags, a server
  started with `--placement-curves on --curve-hosts a` books a job on `a` by its curve and places a job that fits
  flat on another host flat there; without `--curve-hosts` it books by curve on every host.
- **Settings.** `on` is accepted and `On` is refused. `curve_max_wait_seconds` of 301 is refused. `on` without
  `--agent-pool-config` is refused. `--curve-hosts` and `--curve-max-wait-seconds` give what the keys give and
  refuse what they refuse, and each is refused without `--agent-pool-config` and beside `--config`.
- **The shadow table.** It prints the new columns from the current release's lines, and reads a log of the release
  before without error.

## 7. Conflicts found while writing this
- **Pack-limit default.** The #1076 commit message says the pack limit defaults to 0.8. The code, the flag table and
  `placement-by-load.md` say 0.5 (`load.rs:157`, `crates/js-lease-server/src/lib.rs:52`). This document uses 0.5.
- **Stale agent doc.** `crates/js-agent/src/lifecycle/machine/mod.rs:500-502` still says a job's memory is "held to
  its booking by its lease's `memory.max`". That contradicts `cgroup.rs:315`. It needs a one-line doc fix, not part
  of this pull request.
- **Memory step-up.** komira-ai/jobscheduler#1024 describes js-agent as writing `memory.max` = booking. Since
  komira-ai/jobscheduler#1027 it writes none. Its step-up still applies, because the placer judges a run against its
  booking.
- **`cpu-over-time.md` on memory.** Its section 4 learns memory per class, while its section 14 lists memory
  curves as not chosen. This design keeps memory learned in shadow and booked by today's rule.
- **`cpu-over-time.md` decision 3 is reversed.** It kept today's booking as a flat ceiling, so the curve could only
  hold a job back. Here the curve is the booking for a shaped class, as the owner has directed. On a curve host, rule 1
  replaces the flat CPU checks.
- **`MAX_PROFILES`.** It is 2048, against 5,335 classes seen in 4 h. Only 330 of them had 16 lines, and eviction
  takes the earliest last run, so the classes that can pass check 1 are the ones kept (INFERRED). Question 10.

## 8. Open questions for the owner
1. **A margin per job on the flat number?**
   **Recommend: none.** The 1.1x slack is the margin, at the host. A margin per job books every host emptier by that
   share for the same effect, and the flat step books the median bare.
2. **On a curve host, does rule 1 replace the check of summed flat CPU numbers?**
   **Recommend: yes.** Rule 1 keeps every slot, and the slot from 128 s on, within cores x 1.1, which is "never far
   over 100% booked" at every moment. The flat sum counts every job at its full level until it ends. Kept beside
   rule 1, it would stop the curve from using the cores a past burst frees.
   The cost: after a rollback, a curve-packed host takes no new job until enough of its jobs end. The canary
   (question 6) and the replay gate (question 9) come before it reaches every host.
3. **What a curve slot books: the 90th percentile at that age, held to a quarter of the host?**
   **Recommend: yes.** The mean per slot under-books half the runs of the class in its burst. The maximum books
   every rare spike. The quarter is the bound every CPU booking has.
4. **Does memory book from the profile, now or later?**
   **Recommend: not now.** Revisit only if all three hold:
   - `js_placement_refusals_total` shows `booked-memory` or `predicted-memory` refusing more often than `cpu` over a
     working week;
   - there is a host-OOM signal;
   - the memory gate of section 4 passes.
   A memory under-prediction kills jobs that are not its own and teaches no class. The memory shadow (section 2)
   gives the first condition's evidence in its own terms, how much sooner the curves would place and how often a
   host's projection is too low; the host-OOM signal is still missing.
5. **Are the four checks and their thresholds right?** They are 16 runs, 80%, 3 of 16 more than 25% over, and 12 of
   16 usable.
   **Recommend: yes, as starting values to set from the decide-in-shadow days.** Checks 1 and 2 are the profile's
   own rule already.
6. **Canary by named hosts, by pool, or by class?**
   **Recommend: named hosts.** One engine places every pool, and classes are too many and too short-lived to list.
7. **The wait bound: 30 s, then flat?**
   **Recommend: yes.** CPU is a share. After 30 s, a start whose burst may meet another's is worth more than a longer
   wait.
8. **The pack limit's CPU share: the fullest booked slot, or the sum of flat numbers?**
   **Recommend: the fullest booked slot.** With curves, a host's booking is its curve, and "half its booking" should
   mean half at its fullest moment. With every class flat the two are equal.
9. **Gate `on` on no loss against the flat step, not on 10% against today?**
   **Recommend: yes.** The flat step will already take most of the gain against peak booking. The curve's own
   effect is fewer collisions. It should be measured against the flat step and must not cost total time.
10. **Raise `MAX_PROFILES`?**
    **Recommend: not now.** Eviction removes the classes that run least, which cannot pass check 1 anyway. If the
    longer window shows evicted classes with 16 runs, raise it then, after measuring the server's memory per
    profile.

## 9. Considered and not chosen
- **Keep today's peak booking and use the curve only to hold jobs back** (this document's first version, and
  `cpu-over-time.md` decision 3). Hosts stay full by booking while their cores sit idle.
- **Book every class at its sustained level and stop there** (the flat step alone). It is the immediate fix, and
  the gate compares against it. It lets the bursts of shaped classes meet.
- **Feed the job profiles into js-sched's minute lanes.** A minute holds most jobs' whole lives, so it books the
  peak, not the shape (section 3).
- **Fade the unexplained reading within 10 s** (`cpu-over-time.md` section 5, `B_h`). It books bursts onto hosts
  that are busy for reasons the curves do not see. Carrying it costs at most the wait bound.
- **A curve slot at the mean, or at the maximum** (question 3).
- **A per-class operator allow-list.** There are thousands of classes, named by hash, and it duplicates the
  automatic checks.
- **Memory curves (remaining maximum)** (question 4). In shadow the projection is per slot, not the remaining
  maximum: a job is projected to hold each slot's 90th percentile, and the margin and the residual carry the risk.
- **Throttling or killing on a wrong estimate.** Ruled out by the owner (`cpu-over-time.md` section 7).
