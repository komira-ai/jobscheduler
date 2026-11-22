# Request history and sizing

Companion to [docs/design.md](../design.md), section 7 (7.1 to 7.6). Section numbers, steps and decisions refer to design.md.

## 7.1 The generic key
`(class, variant, partition, basis)` where `class` is opaque, `variant` is optional and dropped for the fallback level (as `Key::l2()`
drops the configuration in `crates/kbf-estimator/src/key.rs`), `partition` is the tenant, and `basis` is the measurement instrument.
Values with different bases never pool. kbf derives its class from the remote execution command shape
(`crates/kbf-server/src/action_usage/shape.rs`); ucdk derives it from job definition id and image digest.

## 7.2 Sizing policy
Constants that are scattered today become one value:

```rust
struct SizingPolicy {
    limit_of: fn(booking) -> limit,   // today memory_max: 1.5 x + 512 MiB
    round: u64, min: u64,             // 256 MiB steps
    slack_frac: Ratio, headroom: u64, // 20% + 128 MiB
    n_min: usize,                     // 5; gates lowering below the default booking
    mixed_ratio: u64,                 // 4: a key whose largest recent sample exceeds this times the median is never lowered
    default_memory: u64,              // the booking an under-sampled, mixed or raise-only key falls back to
    raise_only: RaiseOnly,            // kills in a run window that make a key raise-only, and for how long
    decay_runs: usize, decay_days: u32, stale_days: u32,
    cpu_step: u64, cpu_min: u64,      // 250 m, 1000 m
    ladder: MemoryLadder,
    seen: SeenRule,                   // LastPeakPlusBuffer | WindowMax
    mode: SizeMode,                   // Learned | Declared (ucdk: the declared size is the booking and the ceiling)
}
```

`SizingPolicy::kbf()` returns the current values and the property test asserts that every existing estimator output is unchanged (step
14). The codec's length depends on fixed slot counts, so those stay fixed or the codec version byte is raised
(`crates/kbf-estimator/src/codec.rs`). In `Declared` mode history only informs a warning or a suggested size and never raises a booking
(11.3).

## 7.3 One limit rule
The 1.5x + 512 MiB rule lives in `cgroup.rs`, in `memory_max()` in the estimator, and in the native driver. The Start `limits` field plus
`SizingPolicy::limit_of` make the scheduler the one source: the scheduler computes the limit, sends it, and the estimator's censoring
decision ("did this run hit its limit") uses the same function. A provider may still enforce a stricter limit and say so in usage.

## 7.4 Ladder per class
`--memory-ladder-gib` is server-wide. The generic form is per resource class: an unseen request climbs its class's ladder; an
out-of-memory kill moves to the next rung above the killed run's *booking* and below the cap, else to the cap (`MemoryLadder::raised`, 4.3).
The cap is an explicit core query, `cap_of(request)`, which keeps `Scheduler::memory_cap`: the memory of the largest live pool,
cordoned or not, that serves the request's kind, platform, CPU and GPU needs, and always the pool the killed run used. It is not
Fit's candidate set, which leaves cordoned pools out; a `cap_of` built on Fit's view would change the rung a killed run climbs to
during a cordon or drain. For a pool that scales, the cap is the largest shape the pool can acquire (the memory of
its biggest configured shape, 5.7), not the live fleet. Because the limit is 1.5x + 512 MiB, a 12 GiB booking has an 18.5 GiB limit
and the next rung (16 GiB) is below the limit that just failed; whether to move to "above the limit" is D4, not part of the extraction.
The scheduler interface stays `set_size_key`, `key_floor_bound`, `lower_key_floor`, `restore_floors`
(`crates/kbf-sched/src/scheduler/memory.rs`). The ladder becomes a plugin behind `RetryPolicy`, because `memory.rs` today mixes retry
accounting with the ladder and is 1286 lines.

## 7.5 What rule R1 says and what ships
R1 says a seen request starts at its last peak plus a buffer. The shipped estimator is a long-window maximum with 20% plus 128 MiB, and
in `on` mode only the kill-driven floor is applied (4.3). The design does not decide this. `SeenRule` has both values; kbf stays as
shipped until the project owner picks (D4). Whichever is chosen, lowering a booking stays off until a replay on recorded data supports it
(shadow mode first, as `--learned-sizes` does, then `shadow_diff` in `crates/kbf-estimator-replay/tests/shadow_diff.rs` to prove parity;
replay output is an upper bound).

## 7.6 Learning handshake
`Learned::stage` then `requeued` or `finish` (`crates/kbf-server/src/learned.rs`) learns from an attempt only after the scheduler committed the result, so a discarded result does not teach. This becomes `SizeLearner` with kbf types removed; persistence is a `SizeStore` trait (get_all, put_batch, delete, scan) with rows written outside the scheduler transaction, best-effort after kills and write-behind otherwise.

## 7.7 Jobs that book nothing
A job that leaves `Needs.cpu_millis` or `Needs.memory_bytes` at 0 (or sends no needs) is booked, for each of them, what the history
of its class gives (`crates/js-sched-placer/src/sizes.rs`). A dimension the job names is booked as named. This is the generic key of
7.1 for jobs:

- **class**: the value of the job's label `size-class` when it has one (the name of the work, such as the build target), else its
  image's name (not its digest) and command. The arguments are never part of it: a client's arguments name what one run works on (an
  action's digest, a lease, a deadline), so a class that held them would hold one run and never learn. A client that runs different
  work under one image and command names each work's class in the label. The estimator's shape is the first eight bytes of the
  class's SHA-256. A label value is 1 to 63 bytes of `[a-z0-9._-]`, which a build target's name is not, so
  `js_types::SizeClass::of_target` (`crates/js-types/src/size_class.rs`) writes one as a class: letters lowered, each run of other
  bytes one `.` (`root//crates/js-sched:js-sched` is `root.crates.js-sched.js-sched`), and a class over 63 bytes cut to its last
  whole words within 46 bytes plus `-` and the 16 hex digits of the FNV-1a hash of the whole class. Targets that differ only in case
  or separators share a class. A build farm labels each action's job with its target's class and leaves the CPU and memory at 0.
- **partition**: the client that submitted the job; **platform**: the job's `os` and `arch` as it names them; **basis**: cgroup.

The estimator is `js-sizing`: memory is `1.2 x` the largest recent peak plus 128 MiB (at least 1 GiB), CPU the 90th percentile of
its runs' peaks (the most cores a run used over five seconds) rounded up to a quarter core. A class with five fresh runs books its own estimate. A class with
fewer books the memory its fallback's runs ask for (`Summaries::peak_memory`: `1.2 x` their largest recent peak plus 128 MiB, from
one run on, whatever their spread): its fallback class's (7.8), else the whole history of its client on its platform (a third key
every run of the client but a memory kill teaches), raised by its own runs' peak with the same margin and by its kill floor, so a class is never
booked below a peak of its own. Only a client with no fresh run at all books the cold size: `server.cold_cpu_millis` and
`server.cold_memory_bytes` (or their flags), each one left out its default: 22 cores and 4 GiB. The cold CPU is never more than a quarter of
the most cores of the hosts the engine knows (the most any class books, so that the first run of an unknown class has room to
show its peak), and the cold memory never more than the most bookable memory (the pool's `memory_schedulable_percent` applied) of the hosts the engine knows, and at
least the class's kill floor. The CPU is the class's own learned CPU, else its fallback class's, else its client's (the first
with a CPU sample), at most a quarter of the most cores a host has (7.9); the cold CPU when none has a sample. A class whose
booking reaches the largest host's bookable memory books all of it and waits for an empty host.

The cold default is a fixed size, not a share of the hosts. It was 16 GiB, which a farm whose hosts have 88 cores and 251 GiB
filled at 15 jobs a host while its build jobs peaked at 0.6 to 3.2 GB; it is 4 GiB, above the largest of those peaks, and it books
only the first jobs of a client with no history. A job that needs more is raised by its runs: a memory kill books twice the
booking it died at next (the floor), and each run's peak with its margin raises its class's next booking.

The size is decided by the placement pass, not at submit, and is not written on the job: its needs stay as submitted, so a retried
submit with the same id is still the same job. What it was booked is its allocation's `booked`, and which history sized it its
allocation's `sized_from` (`class`, `fallback`, `cold` or `over-cap`; none for a job that named both). A contained job's memory
booking is its `memory.max`, with no swap. Its CPU booking is what it is guaranteed (7.9), and placement packs a host by the bookings;
nothing holds a job below what it needs: a compile that needs 20 cores for 30 seconds gets them whenever the host has them. The
lease's `cpu.weight`, from the booking, is how the kernel keeps the guarantee when a host is busier than planned (leases that all
want CPU get the cores in proportion to their bookings); there is no `cpu.max` unless the agent's operator sets a ceiling
(`--cpu-ceiling`, a multiple of the booking, off by default), and then the lease and the job's own cgroup are held to the booking
times it. The CPU slack and memory percent bound the booking as they bound every booking. The job's start carries its size class and
where each of its CPU and memory bookings came from (`sizing`: `named` or the allocation's `sized_from`), which the agent writes in
its journal at the job's start and at its result.

**Where a run's usage comes from.** Each run's peak memory and CPU time are what its host reported: when a contained job ends, its
agent reads `memory.peak` and the `usage_usec` of `cpu.stat` of the job's lease control group, before the group is removed, and sends
them as the `usage` of the job's result (`docs/design/agent-wire.md`), with the `nr_periods` and `nr_throttled` of the same
`cpu.stat` (`cpu_throttle`); the server stores them with the job's output. A job whose
result carries none (an agent from before the field, cgroup version 1, a result read back after an agent restart) has no usage, and
a success with no peak is not a memory sample, so a class whose jobs all lack it books from its fallback or its client (7.8),
else the cold size. The count a server logs at
its start (`taught`) counts the runs that gave a class a sample, not the jobs it read.

**What a run teaches.** When a pass sees a job's allocation freed, it reads the job once and gives the estimator one run: an exit 0 is
a success; any other exit, a wall limit or a failed input a failure, which teaches the memory nothing; a stop for memory is a kill at
the job's own limit, which raises the class's floor to twice the booking it died at. A success whose peak came within 5% of its
booking counts as having hit its limit (the estimator's own censoring rule assumes the build-host limit of 7.3), so it can hold the
class at its booking but not lower it. A run's CPU sample is its peak: the most cores its group used over any five seconds of
the run, which the agent measures from the group's `cpu.stat` every second and reports beside the CPU time (`cpu_peak`). A run
with no peak reported (an agent from before the field, and every run recorded before it) has only its mean CPU, which is a lower
bound of its peak: it counts as held (below). A run's sample is a measurement only when its own contention is low and nothing held it: its
contention is how long some task of its group waited for a CPU (the `some` total of the group's `cpu.pressure`, which the agent
reports beside the CPU time) per unit of wall time, and below a tenth the sample is kept as measured. At or above a tenth the run
used less than it would have alone, and its CPU may only raise its class's CPU. A run with no pressure reported (an agent from
before the field, and every run recorded before it) counts as having waited for nothing, as every run did before the field. A run held at a CPU ceiling is a lower bound in the same way: its
group was throttled in at least one period in 20 (`nr_throttled` against `nr_periods` of its `cpu.stat`, reported beside the
CPU time; a group with no `cpu.max` counts no periods), or, with no throttle counters reported, its mean CPU came within 5% of the
CPU it was booked (every run recorded while the agent wrote the booking as `cpu.max`). So a class is never held at a ceiling it
was given or at a mean below its peak: what it is booked rises with the runs that used more, and falls only on quiet, measured
runs that used less.

**Where the history lives.** Nowhere new: at its start the server teaches the estimator every ended job its job store still holds
(seven days by default), in the order they ended, with what each allocation booked; then each pass teaches the jobs it sees freed.
A failed read at the start is logged and every class starts cold. Each job's usage history row now carries what its host reported
(`used`: the peak memory, and the CPU per second of its run as both the highest and the mean), so `GetUsage` can set what a host
used beside what it booked, and is kept under the name of the job's class as its `profile`: the `size-class` label as it is, else
`run-` and the 16 hex digits of the class's shape. The name is what the job builds (for example `kbf.cargo-test.kbf-server`), the
same for every run of it and never its job id or image digest, so the rows of one build read side by side.

## 7.8 The fallback class
**Why.** A build farm's remote cache answers every action whose inputs did not change, so an action runs again only when its
inputs change, and a build target gathers runs slowly. On one farm, in thirty minutes of work, every job that booked nothing was
booked the cold size, across 753 classes most of which were new targets: per-target history alone rarely reaches an estimate.
The work of one kind (a Mojo compile, a `zig cc`, a `cargo test`, a genrule) is far more alike across targets than the cold size
assumes, and each kind runs many times a minute.

**What.** A job may name a second class in its label `size-fallback` (`js_types::SIZE_FALLBACK_LABEL`, a value like any
`size-class`): the class whose history sizes it while its own class has too few runs. A build farm sends the target as
`size-class` and the action's kind as `size-fallback`.

- The fallback class is keyed exactly as a `size-class` of that name is (the same client, platform and basis), so a job whose
  `size-class` is `zig-cc` and one whose `size-fallback` is `zig-cc` read and teach one history.
- Every run a job teaches is taught to its own class and to its fallback class (once, when they are the same class). The kind
  therefore learns from every target of the kind, and each target from its own runs, so the target takes over as its runs come.
- **Booking.** While the job's own class has fewer than five fresh runs (the estimator's `N_MIN`), its memory is the largest of
  the memory its fallback class's runs ask for (`1.2 x` their largest recent peak plus 128 MiB, from one run on, whatever their
  spread), its own runs' with the same margin, and its own kill floor. A fallback class with no fresh run (none, idle over 60
  days, or one whose runs reach the host) is no fallback: the client's whole history on the job's platform stands in its place,
  and the cold size only when the client has no fresh run either. Its CPU is its own class's learned CPU when it has a CPU
  sample, else the fallback class's, else the client's. With five runs of its own, a class is sized from them alone.
- The kill floor of the job's own class applies on every path; the fallback's own floor is part of what the fallback asks for.
- The fallback's memory is the maximum of the kind's recent peaks with a margin, not a percentile, and not the cold memory for a
  kind whose peaks are spread: the largest peak of a kind is what any of its targets may need, and it is far below the cold size
  of before (7.9 has the farm's numbers). A target that needs more than its kind is raised by its own runs and its kill floor.
  The other side of the maximum: a kind one of whose targets peaks far above the rest books every young target of the
  kind near that peak until the kind's long window (256 to 288 of its runs) no longer holds it, and a kind whose peak with the
  margin reaches the host is no fallback at all. A client that runs such a target as a step of a kind of small steps names it
  a kind of its own.
- **Measuring.** Each placement of a job that booked nothing is logged with what sized it: `class` (its own class's estimate),
  `fallback` (its fallback class's estimate), `cold` (the cold size) or `over-cap` (the whole of the largest host's memory). The
  share of each over a window of farm work is how this section is judged. `fallback` also names a job sized from its
  client's whole history.

The label is optional and additive: a job without it is sized as before, and a server that predates it ignores it (a label is
opaque to it), so a client may send it to any server.

## 7.9 Sizing when the CPU booking is a share
**Why.** On the farm of 7.7 (six hosts of 88 cores and 251 GiB), the hosts were 11% to 33% busy. Each job ran under a `cpu.max`
of its booked CPU (0.25 to 1.25 cores) and was throttled almost all the time, and each host was full by memory bookings at about
15 jobs: most jobs booked the cold 16 GiB and used 0.6 to 3.2 GB. No job now has a CPU limit, known or unknown: the agent writes
a job's booked CPU as its `cpu.weight`, a share of the host's CPU. Memory stays a hard limit on every job.

**What the CPU booking is: the peak.** A class books the 90th percentile of its recent runs' peaks: the most cores a run used over
any five seconds of it. Five seconds is the window placement already measures hosts on (`placement-by-load.md`), longer than a
scheduler tick or the spawn of a parallel build's processes, and short enough that a compile that runs 20 cores for 30 seconds
reads as 20 cores. The 90th percentile books what nine runs in ten reach; the tenth bursts above its booking into what its host
has idle or shares by weight, and one outlier run does not hold the class high for the next 32. A run that waited for a CPU
(contention of a tenth or more), was held at a ceiling, or reported no peak (its mean is a lower bound) can only raise its class,
never hold it down, so the floor of a quarter core absorbs nothing. A class books at most a quarter of the most cores a host has
(22 of 88): no class's history books a host alone, four jobs of the largest class bursting at once fit a host within its slack,
and a job of that class that waits for room (the walk holds a host for the first job that waits on it) leaves at most a quarter of
the host unbooked meanwhile.

**Example.** A compile that needs 20 cores for 30 seconds is recorded with its CPU time (600 core-seconds) and its peak (20 cores
over five seconds). The next run of its class books 20 cores. An 88-core host packs bookings up to 96.8 cores (the 1.1x slack),
so it takes four such compiles (80 cores) and has 16.8 cores left for other jobs; a fifth would need 100. When all four burst at
once they use 80 of the 88 cores; with the other jobs at their peaks as well the host is asked for 96.8 cores and each job gets
its share by weight, 91% of its peak.

**What placement packs.** Placement packs these peaks with the load-based admission of `placement-by-load.md` and the CPU slack
(default 1.1x), both unchanged. A host is charged the larger of its measured CPU and the peaks of the jobs it ran through the last
reading, plus the peaks of the jobs placed since (covered leases, below). With the admission's floor at its default (1), a
measurement below the peaks does not release room: a host whose jobs are between bursts is still booked by their peaks, and it is
busy at the share of their peaks its jobs use on average. A measurement above them (a job bursting past its booking) blocks new
jobs until it falls. The floor stays at 1: between-burst room is not released.

**Unknown jobs.** A class with no CPU history books its kind's learned peak, else its client's, and only a client with no history
at all books the cold CPU: by default 22 cores, held at a quarter of the largest host, the most any class books. Its first run is
therefore placed with room for the largest peak a class may book, and its share of a busy host is that large, so one run is
enough to learn its peak. An unknown job's memory is its kind's or its client's peak with the margin, else the cold 4 GiB, a hard
limit raised by a kill's floor.

**The history from before.** Every run a server rebuilds at its first start after the change was taken under a `cpu.max` and has
no peak: its mean is a lower bound, so it can only raise. A class whose runs were all held at a quarter core therefore has no CPU
sample: its jobs book its fallback's or its client's learned peak, else the cold CPU, and its first run that reports a peak books
it, even on a busy host. A new server beside agents from before the peak field never lowers a class from what those agents report,
so it books high (its fallback's peak, its client's, or the cold CPU) until the agents report peaks.

**Run time.** The estimator keeps the minutes each run lasted as it did. The job walk charges each job flat for its life, and the
whole-host path waits on a job's wall limit, as before.

**Memory.** A young class books its fallback's peaks with a margin, not the cold size (7.7, 7.8); memory stays a hard limit per
job and the pool's `memory_schedulable_percent` per host, and the kill floor stays the safety net.

**Proof on runs shaped like the farm's** (`crates/js-sched-placer/src/sizes_farm_tests.rs`, measured by the tests):

- 120 targets of three kinds (peaks 0.6 to 2.0 GB, 1.0 to 3.2 GB, 0.6 to 1.2 GB), one to three runs each under a quarter-core
  to 1.25-core ceiling at 16 GiB: every target's next booking is at least its own largest peak and at most 4 GiB, from its kind;
  the bookings sum to 1.78 times the peaks, and 97 such jobs fit a host by memory where 15 did.
- A class whose 32 runs were held at a quarter core and report no peak books the cold 22 cores at the first start (not the
  250 m the old history held it at), 20 cores after one run that peaked at 20 on a busy host, and half a core after nine quiet
  runs that peaked at 0.3 cores; runs of agents from before the peak field, held at their booking, never lower it.
- A class whose runs peak at all 88 cores books 22; four of its jobs leave a host 8.8 of its 96.8 cores.
- A compile that needs 20 cores for 30 seconds is stored with 600 s of CPU time and a 20-core peak, the next job of its class
  books 20 cores, and one pass over an 88-core host with six of them queued places four (80 cores booked, 16.8 left).
- One pass over a farm host from queued steps of the mix (compiles peaking at 20 cores and averaging 6, tests 4 and 2, genrules 1
  and 0.5, and a step peaking at 88 and averaging 30 at the head of every ten) places 9 jobs of every kind, booked 77 of the 96.8
  cores and 22 GiB of memory: the host is full by CPU peaks, not by memory (15 jobs at the old 16 GiB), and the next compile, which
  needs 20, waits with 19.8 cores of room. Its jobs use about 49.5 cores on average (56% of the host, against 11% to 33%
  before) and up to all 88 when they burst together; the gap between the two is the between-burst room the admission floor keeps.

**Covered leases.** The placer's engine used to drop the reading's list of leases the measurement covers, so admission charged a
host's measured CPU and every job's booking again: a host of jobs that use their booking was refused at about half its cores. The
engine now covers the job leases it held at the previous accepted reading of the host and still holds, as
`placement-by-load.md` defines covered leases (a lease placed since then is still charged in full on top of the measurement).

**Why a host is not full.** Each refusal of a host to a queued job is reported with the predicate that refused and its figures:
`cpu` (the CPU side of the load predicate, with the measured CPU), `predicted-memory` (the memory side), `booked-memory` (the flat
memory guard) and `cold-cap`. The placer logs each at debug level and one line per host per pass at info level, and `/metrics`
counts them as `js_placement_refusals_total{host,reason}`. Before, the only reason a job waited was its queue status
(`WAITING_FOR_CAPACITY`), and the scheduler's count of blocked requests was not exported.

**On the farm.** The before and after pictures are the same read-only reads of the job service (`js --json usage`, the ended
bookings, joined by job id with `js --json job list`, each job's `size-class` and `size-fallback`): per kind the peaks and mean
CPU, and the share of runs whose mean CPU came within 5% of their booked CPU (held); per host the jobs at once, the summed
bookings against the summed peaks and the booked against the used CPU; per class and hour the booked CPU, the mean CPU as a share
of it and the booked memory against the peak. After both changes, the held share is about 0, the summed memory bookings are within
three times the peaks (16 GiB bookings against peaks of 0.6 to 3.2 GB were five to 28 times), a host holds well over 15 jobs, its used CPU is near its cores at busy
hours, and memory kills are not above their level before.

