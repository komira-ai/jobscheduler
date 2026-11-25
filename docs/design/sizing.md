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
    limit_of: fn(booking) -> limit,   // build hosts: 1.5 x + 512 MiB; js-agent: none, or the booking (docker, podman)
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
(formerly 11.3, dropped with the ucdk realization: [ucdk.md](ucdk.md)).

## 7.3 One limit rule
The build-host 1.5x + 512 MiB rule lives in `cgroup.rs` (`LeasePolicy::BuildHost`) and in the native driver; the estimator derives no
limit from a booking. The Start `limits` field plus `SizingPolicy::limit_of` make the scheduler the one source: the scheduler computes
the limit, sends it, and the job service's decision ("did this run reach its limit", `Run::limit_hits`) uses the same limit. A provider may still enforce a stricter limit and say so in usage.

## 7.4 Ladder per class
`--memory-ladder-gib` is server-wide. The generic form is per resource class: an unseen request climbs its class's ladder; an
out-of-memory kill moves to the next rung above the killed run's *booking* and below the cap, else to the cap (`MemoryLadder::raised`, 4.3).
The cap is an explicit core query, `cap_of(request)`, which keeps `Scheduler::memory_cap`: the memory of the largest live pool,
cordoned or not, that serves the request's kind, platform, CPU and GPU needs, and always the pool the killed run used. It is not
Fit's candidate set, which leaves cordoned pools out; a `cap_of` built on Fit's view would change the rung a killed run climbs to
during a cordon or drain. For a pool that scales, the cap is the largest shape the pool can acquire (the memory of
its biggest configured shape, 5.7), not the live fleet. Because the build-host limit is 1.5x + 512 MiB, a 12 GiB booking has an 18.5 GiB limit
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

The estimator is `js-sizing`: memory is `1.2 x` the largest recent peak plus 128 MiB (at least 1 GiB), CPU the median of
its runs' mean CPU (CPU time over wall time) rounded up to a quarter core, or, for a class whose five-second peaks are a heavy
tail, the 90th percentile of those peaks (7.10). A class with a fresh memory sample of its own books what its own runs ask for
(`Summaries::peak_memory`: `1.2 x` their largest recent peak plus 128 MiB, rounded up to 256 MiB, at least 1 GiB, and its kill
floor), from its first run on, whatever their number or spread. The kind's maximum and the client's no longer apply to it, and the
estimator's own rules for mixed runs (the largest of 32 samples over 4x their median books at least the default) and for fewer than
`N_MIN` samples no longer hold a job class at the cold size: the placer does not call `Summaries::estimate` for memory. A class with
no run of its own books its fallback class's figure (the same formula over the kind's runs, 7.8), else the whole history of its client
on its platform (a third key every run of the client but a memory kill teaches), else the cold size, and at least its own kill floor.
A fallback whose figure reaches the host is no fallback. Only a client with no fresh run at all books the cold size: `server.cold_cpu_millis` and
`server.cold_memory_bytes` (or their flags), each one left out its default: 22 cores and 4 GiB; a job whose `os` constraint the server names
in `server.cold_memory_bytes_on` (`--cold-memory-bytes-on OS=BYTES`) books that operating system's memory in place of `cold_memory_bytes`,
and one it names in `server.cold_cpu_millis_on` (`--cold-cpu-millis-on OS=MILLIS`) that operating system's CPU in place of
`cold_cpu_millis` (the name as the job gives it; a job of another `os`, or of none, books the server's). The cold CPU is never more than a quarter of
the most cores of the hosts the engine knows (the most any class books, so that the first run of an unknown class has room for
the cores it uses), and the cold memory never more than the most bookable memory (the pool's `memory_schedulable_percent` applied) of the hosts the engine knows, and at
least the class's kill floor. The CPU is the class's own learned CPU, else its fallback class's, else its client's (the first
with a CPU sample), at most a quarter of the most cores a host has (7.9); the cold CPU when none has a sample. A class whose
own figure reaches the largest host's bookable memory books all of it and waits for an empty host.

**Why a class follows its own runs.** On a build farm of hosts of 88 cores and about 250 GiB, memory bookings, not CPU, decided how many
jobs a host ran: booked memory was at the host's limit in 80% of samples while booked CPU was under 60 of the 96.8 cores allowed in
93% of them. The jobs that finished on one host booked 5,813 GB against 1,147 GB of peak; a link step that peaks at 10 MB booked
16 GiB and a compile that uses 2.8 GB booked 16 GiB. Under the rule before this one, a class with fewer than five memory samples (which a run
under a second never gave) booked its kind's or client's largest peak, a class whose peaks were spread booked the default, and a
success near or above its booking could not raise it.

The cold default is a fixed size, not a share of the hosts. It was 16 GiB, which a farm whose hosts have 88 cores and 251 GiB
filled at 15 jobs a host while its build jobs peaked at 0.6 to 3.2 GB; it is 4 GiB, above the largest of those peaks, and it books
only the first jobs of a client with no history. A job that needs more is raised by its runs: a memory kill books twice the
booking it died at next (the floor), and each run's peak with its margin raises its class's next booking.

The cold memory of an operating system is set for the hosts of one platform without changing the others'. A farm whose build hosts
run with `cold_memory_bytes` at 16 GiB has Macs with 88 GiB of bookable memory, which that size fills at five jobs a Mac
whatever their CPU; `cold_memory_bytes_on = { macos = 4294967296 }` books a new Mac class 4 GiB and leaves the build hosts' 16 GiB as
it is. With 4 GiB a Mac is then held by the cold CPU: at `cold_cpu_millis` 4000 seven jobs fill its 28 cores at the CPU slack of
1.1; `cold_cpu_millis_on = { macos = 1000 }` books a new Mac class one core, and the Mac's memory holds it at 22. It is a setting, not a
share of the hosts: a share (the cold CPU's quarter of the cores, say) would change what every platform books at once. A Mac's classes
learn from their runs as Linux's do once its agent reports their usage.

The size is decided by the placement pass, not at submit, and is not written on the job: its needs stay as submitted, so a retried
submit with the same id is still the same job. What it was booked is its allocation's `booked`, and which history sized it its
allocation's `sized_from` (`class` for a class with a run of its own, `fallback` for one booked from its kind or client, `cold` or
`over-cap`; none for a job that named both). A contained job's memory
booking is no limit: its cgroup has no `memory.max` and no `memory.swap.max`, so a job that needs more than it booked runs on
what the host has (its swap absorbs a little) and reports its whole peak, and the bound on the jobs of a host together is the
share of it placement books (`memory_schedulable_percent`). Its CPU booking is what it is guaranteed (7.9), and placement packs a host by the bookings;
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
`cpu.stat` (`cpu_throttle`); the server stores them with the job's output. An agent started with `--memory-series` also samples
the group's non-file memory about once a second while the job runs, `memory.current` less `file - shmem` of its `memory.stat`
(anonymous and kernel memory, shared memory and tmpfs; no page cache of the files the job read, through a mount of the tree store or
anywhere else, which the kernel can drop), and sends the most of each span as the result's `memory_series`. The peak a run teaches
its class is that series' largest point when the result has a whole one (not a `partial` series of a run adopted after an agent
restart); otherwise it is `memory.peak`, which counts page cache as well. On the farm, page cache was about a tenth of running jobs'
`memory.peak` (a read of every running job's group on six hosts: 31.7 of 325.8 GiB of peaks, 8% for the median build job). A Mac's native runtime sends the same fields from samples of the run's processes and its supervisor's wait of
the job (`docs/design/mac-runtime.md`, section 4): the peak is the processes' physical footprint together (which counts no file page)
or the largest resident set, the series their footprint. A job whose
result carries none (an agent from before the field, cgroup version 1, a result read back after an agent restart) has no usage, and
a success with no peak is not a memory sample, so a class whose jobs all lack it books from its fallback or its client (7.8),
else the cold size. The count a server logs at
its start (`taught`) counts the runs that gave a class a sample, not the jobs it read.

**What a run teaches.** When a pass sees a job's allocation freed, it reads the job once and gives the estimator one run: an exit 0 is
a success; any other exit, a wall limit or a failed input a failure, which teaches the memory nothing; a stop for memory is a kill at
the job's own limit, which raises the class's floor to twice the booking it died at (only an agent that still held a job at its
booking reports it); the kill is a sample of at least that booking. A success's peak is a measurement whatever its booking: the crun
runtime writes no `memory.max`, so a peak above the booking is taken as it is, and a run that used more than its booking raises the
class's next booking at once, with the margin. But a success whose group's `memory.peak` came within 5% of its booking and stayed
within it reached its booking: a host that holds a job at its booking (the docker and podman runtime's `--memory`, whose limit counts
the page cache that the non-file peak leaves out) held it there, so its peak does not show what it needed. The placer counts it as
having reached its booking (`Run::limit_hits`), and the estimator takes it as a held sample of the larger of its peak and its booking,
sized with the same margin: its class books a step above the booking next (`1.2 x` the booking plus 128 MiB, rounded up to 256 MiB),
steps again while its runs keep reaching it, and stays there once they fit. A held sample is kept below the host, so the step stops
at the whole host (`over-cap`). A run's swap peak, which the agent reports, is not in the job record, so a run that a host short of
memory squeezed into swap is not yet counted. A run under a second gives a memory sample (its series' peak when a poll sampled it, else the
group's `memory.peak`, exact whatever the run's length) and no CPU sample: the placer hands the estimator a one-second wall and no CPU time for it. A run's CPU sample is its mean: its CPU time over its wall time, which every run
reports. The most cores its group used over any five seconds of the run, which the agent measures from the group's `cpu.stat` every
second and reports beside the CPU time (`cpu_peak`), is kept beside the sample and is not the sample: it books a class only when the class's peaks are a heavy tail (7.10). A run's sample is a measurement only when its own contention is low and nothing held it: its
contention is how long some task of its group waited for a CPU (the `some` total of the group's `cpu.pressure`, which the agent
reports beside the CPU time) per unit of wall time, and below a tenth the sample is kept as measured. At or above a tenth the run
used less than it would have alone, and its CPU may only raise its class's CPU. A run with no pressure reported (an agent from
before the field, and every run recorded before it) counts as having waited for nothing, as every run did before the field. A run held at a CPU ceiling is a lower bound in the same way: its
group was throttled in at least one period in 20 (`nr_throttled` against `nr_periods` of its `cpu.stat`, reported beside the
CPU time; a group with no `cpu.max` counts no periods), or, with no throttle counters reported, its mean CPU came within 5% of the
CPU it was booked (every run recorded while the agent wrote the booking as `cpu.max`). So a class is never held at a ceiling it
was given or at what a run that waited used: what it is booked rises with the runs that used more, and falls only on quiet, measured
runs that used less.

**Where the history lives.** Nowhere new: after its start the server teaches the estimator every ended job its job store still holds
(seven days by default), in the order they ended, with what each allocation booked; then each pass teaches the jobs it sees freed.
The start does not wait for it: it reads the live allocations alone, so what it reads does not grow with the history, and the server
answers ready; the ended jobs are read a page at a time while it serves. Until they are taught, a job that books nothing is sized from
what is taught so far (at a start, its fallback's or the cold size), and the runs the passes free are held and taught after the ended
jobs, so the history once taught is the one a server that read everything before its first pass holds. A failed read is logged and
every class starts cold, the held runs still taught. Each job's usage history row now carries what its host reported
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
`size-class`): the class whose history sizes it while its own class has no run. A build farm sends the target as
`size-class` and the action's kind as `size-fallback`.

- The fallback class is keyed exactly as a `size-class` of that name is (the same client, platform and basis), so a job whose
  `size-class` is `zig-cc` and one whose `size-fallback` is `zig-cc` read and teach one history.
- Every run a job teaches is taught to its own class and to its fallback class (once, when they are the same class). The kind
  therefore learns from every target of the kind, and each target from its own runs, so the target takes over as its runs come.
- **Booking.** A job whose own class has a fresh memory sample books what its own runs ask for (`1.2 x` their largest recent peak
  plus 128 MiB, from the first run on, whatever their number or spread) and its own kill floor. A job whose own class has no run
  books its fallback class's figure (the same formula over the kind's runs) and its own kill floor. A fallback class with no fresh
  run (none, idle over 60 days, or one whose runs reach the host) is no fallback: the client's whole history on the job's platform
  stands in its place, and the cold size only when the client has no fresh run either. Its CPU is its own class's learned CPU when
  it has a CPU sample, else the fallback class's, else the client's.
- The kill floor of the job's own class applies on every path; the fallback's own floor is part of what the fallback asks for.
- The fallback's memory is the maximum of the kind's recent peaks with a margin, not a percentile, and not the cold memory for a
  kind whose peaks are spread. It books only a target with no run of its own, that is its first job; one run of its own takes it
  over, so a kind with one large target no longer holds its young targets at that peak. A target that needs more than its kind is
  raised by its own runs and its kill floor. A kind whose peak with the margin reaches the host is no fallback at all (7.9 has the
  farm's numbers). A client that runs such a target as a step of a kind of small steps names it a kind of its own.
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
a job's booked CPU as its `cpu.weight`, a share of the host's CPU. Nor is a job's memory booking a limit: it is what placement
counts, and the bound is the pool's `memory_schedulable_percent` per host.

**What the CPU booking is: the median of the runs' mean CPU.** (A class whose five-second peaks are a heavy tail books them
instead: 7.10.) A class books the median (the nearest-rank 50th percentile) of its
recent runs' mean CPU: the CPU time of a run over its wall time, the cores it used on average. The mean is what placement needs.
Placement admits a host by its measured load (`placement-by-load.md`), and the CPU slack (1.1x by default) bounds that measured load,
not the sum of the bookings; so a booking that is a job's average, packed to the slack, fills a host with the work it does, where a
booking of every class's burst held a host to a few jobs whose peaks fill it while their use left most of it idle (only a class whose bursts are a heavy tail books them, 7.10). A job that bursts above its
booking uses what its host has idle or shares by weight (`cpu.weight`, from the booking): it runs slower when the host is busy, it
does not fail, and a host whose measured load reaches the slack takes no new job until it falls. The median books what half of the
runs of a class use or less: half the runs burst above their booking, and one outlier run does not hold the class high for the next
32. The most cores a run used over five seconds (`cpu_peak`) is measured, stored with the run's usage and shown by `GetUsage`,
and sizes only a class whose peaks are a heavy tail (7.10). A run that waited for a CPU (contention of a tenth or more) or was held at a ceiling can only raise its
class, never hold it down: its sample is kept only when it exceeds the class's current estimate, which is the median. So the floor of a
quarter core absorbs nothing. A class books at most a quarter of the most cores a host has (22 of 88): no class's history books a host
alone, and a job of that class that waits for room (the walk holds a host for the first job that waits on it) leaves at most a quarter
of the host unbooked meanwhile.

**Example.** A compile uses 2 cores on average for 30 seconds and peaks at 20 over five of them: it is recorded with its CPU time (60
core-seconds) and its peak (20 cores). The next run of its class books 2 cores. An 88-core host whose agent measures 10 cores busy
takes twelve such compiles at once (24 cores booked, the measured 10 and the bookings placed since within the 96.8 cores of the 1.1x
slack), where a booking of the peak took four (80 cores). When the host is measured at 95 cores busy, another compile waits, although
the bookings on it sum to 24 cores: the slack bounds the measured load, and 95 + 2 is above 96.8. When several compiles burst at once
they use up to what the host has and share it by weight, so each is slower, not killed. Once five of the class's runs have reported
such a peak, its peaks are a heavy tail (20 cores is over twice its 2) and it books 20 cores (7.10): the host then takes four.

**What placement packs.** Placement packs these bookings with the load-based admission of `placement-by-load.md` and the CPU slack
(default 1.1x), both unchanged. A host is charged the larger of its measured CPU and the bookings of the jobs it ran through the last
reading, plus the bookings of the jobs placed since (covered leases, below). With the admission's floor at its default (1), a
measurement below the bookings does not release room: a host whose jobs are between bursts is still booked by their means (a heavy-tail class's by its peaks, 7.10), and it is
busy at about those means. A measurement above them (a job bursting past its booking) blocks new jobs until it falls. The floor stays
at 1: between-burst room is not released. With no measurement of a host yet, the bookings are all the host is charged, so a host
takes up to its slack in bookings, which are means (or, for a heavy-tail class, its peaks, 7.10).

**Unknown jobs.** A class with no CPU history books its kind's learned CPU, else its client's, and only a client with no history
at all books the cold CPU: by default 22 cores, held at a quarter of the largest host, the most any class books. Its first run is
therefore placed with room for the most cores a class may use, and one run is enough to learn its mean. An unknown job's memory is
its kind's or its client's peak with the margin, else the cold 4 GiB, until its first run; then its own peaks with the margin. Both are raised by a kill's floor (a booking, not a limit on the job).

**The history from before.** Every run a server rebuilds at its first start after the change has a mean: its CPU time over its wall
time, from its job record. A server re-derives each run's sample from its job record as the run's mean when it reads the ended jobs after its start; there
is no conversion, and the `cpu_peak` an agent reported with a run, if any, is kept beside it (7.10). The runs taken under a `cpu.max` stay
what they were: a run whose mean came within 5% of its booked CPU (or throttled in at least one period in 20) was held at its ceiling
and can only raise its class, so a class whose runs were all held at a quarter core has no CPU sample: its jobs book its fallback's or
its client's learned CPU, else the cold CPU, and its first run that was not held books its mean, even on a busy host. Runs from before
the `cpu_peak` field and runs after it are alike: every run is sampled by its mean, and what a run with no peak reported was, held
or not, no longer depends on the peak; a run with no peak counts toward no class's tail.

**Run time.** The estimator keeps the minutes each run lasted as it did. The job walk charges each job flat for its life, and the
whole-host path waits on a job's wall limit, as before.

**Memory.** A class books its own runs' memory peaks (of their non-file memory series, 7.7) with a margin from its first run, a class with no run its kind's or client's, not the cold size (7.7, 7.8); memory is bounded by the
pool's `memory_schedulable_percent` per host (a job's booking is no limit of its own), and the kill floor stays the safety net.

**Proof on runs shaped like the farm's** (`crates/js-sched-placer/src/sizes_farm_tests.rs`, measured by the tests):

- 120 targets of three kinds (peaks 0.6 to 2.0 GB, 1.0 to 3.2 GB, 0.6 to 1.2 GB), one to three runs each under a quarter-core
  to 1.25-core ceiling at 16 GiB: every target's next booking is at least its own largest peak, from its own peaks;
  the bookings sum to 1.37 times the peaks (1.78 when each target booked its kind's largest), and 126 such jobs fit a host by memory
  (97 then, and 15 at 16 GiB): test `the_farms_targets_book_their_own_peaks_not_their_kinds_or_the_cold_size`.
- A class whose 32 runs were held at a quarter core books the cold 22 cores at the first start (not the 250 m the old history held
  it at), 6 cores after one run that used 6 on average (and peaked at 20) on a busy host, and half a core after the first quiet run
  of 0.3 cores (the median of the two samples); runs of agents from before the peak field, held at their booking, never lower it.
- A class whose runs use all 88 cores books 22; four of its jobs leave a host 8.8 of its 96.8 cores.
- `a_compile_that_peaks_at_20_cores_books_its_mean_and_a_host_with_measured_room_takes_twelve`: a compile that used 2 cores on
  average for 30 seconds and peaked at 20 is stored with 60 s of CPU time and a 20-core peak, the next job of its class books
  2 cores, and one pass over an 88-core host measured at 10 cores busy places all twelve queued (where 4 were placed at the peak);
  measured at 95 cores busy, the host is refused another compile.
- One pass over a farm host with no measurement from queued steps of the mix (compiles averaging 6 cores and bursting to 20, a
  heavy tail booked at 20 (7.10), tests 2, genrules 0.5, and a step averaging 30, booked the 22-core quarter of the host, at the
  head of every ten) places 10 jobs (one of the large step, then three each of tests, genrules and compiles), booked 89.5 of the
  96.8 cores and 26.3 GB of memory (a host with no reading charges each lease at least a core, so 91 are charged), and the next
  large step, which needs 22, waits: the host is full by CPU bookings, not by memory (15 jobs at the old 16 GiB). Its bookings are the jobs' means (the compiles' their 20-core peaks, a heavy tail), so a host that is measured busy is refused by its load, not
  by the sum of the bookings.

**Covered leases.** The placer's engine used to drop the reading's list of leases the measurement covers, so admission charged a
host's measured CPU and every job's booking again: a host of jobs that use their booking was refused at about half its cores. The
engine now covers the job leases it held at the previous accepted reading of the host and still holds, as
`placement-by-load.md` defines covered leases. A lease placed between that reading and the newest is seen by the newest: it is charged in full on top of the window before the newest, and that sum is raised to the measurement rather than added to it. Only a lease placed after the newest reading is charged in full on top of the measurement.

**Why a host is not full.** Each refusal of a host to a queued job is reported with the predicate that refused and its figures:
`cpu` (the CPU side of the load predicate, with the measured CPU), `predicted-memory` (the memory side), `booked-memory` (the flat
memory guard) and `cold-cap`. A host that the walk's own ledger (`Room`: the summed bookings of the host's live allocations against
the memory its agent offers, `memory_schedulable_percent` applied) finds full by memory is dropped before the scheduler sees it. It
is booked as a `booked-memory` refusal with the ledger's figures (counted is the memory the host's allocations book, new the job's
memory, limit the memory it offers), logged at debug and in the per-host info line and counted like the scheduler's refusals. Before,
such a host was dropped silently, so the log showed only cpu refusals even when memory filled the host. Hosts the ledger refuses for
CPU, disk or a whole host still say nothing. The placer logs each at debug level and one line per host per pass at info level, and `/metrics`
counts them as `js_placement_refusals_total{host,reason}`. Before, the only reason a job waited was its queue status
(`WAITING_FOR_CAPACITY`), and the scheduler's count of blocked requests was not exported.

**On the farm.** The before and after pictures are the same read-only reads of the job service (`js --json usage`, the ended
bookings, joined by job id with `js --json job list`, each job's `size-class` and `size-fallback`): per kind the peaks and mean
CPU, and the share of runs whose mean CPU came within 5% of their booked CPU (held); per host the jobs at once, the summed
bookings against the summed peaks and the booked against the used CPU; per class and hour the booked CPU, the mean CPU as a share
of it and the booked memory against the peak. After both changes, the held share is about 0, the summed memory bookings are within
three times the peaks (16 GiB bookings against peaks of 0.6 to 3.2 GB were five to 28 times), a host holds well over 15 jobs, its used CPU is near its cores at busy
hours, and memory kills are not above their level before.

## 7.10 Classes whose CPU has a heavy tail
**Why.** Booking the median of the runs' means (7.9) is right on average and wrong in the tail. Over one busy hour of the farm, the
CPU booked on its busiest host matched the CPU it used (11.7 cores booked, 11.6 used), yet 2.6% of the runs peaked at 8 cores or
more, booked at a median of 2.75 cores and peaking at a median of 16, some at 65 to 70. Placement fills a host to about all of its
booked CPU and the measured load cannot brake a burst it has not yet read, so a burst of such jobs, each booked at its mean, lands on
one host and runs it far past its cores while the others stay idle.

**The rule** (`js-sizing`, `Summary::cpu`). Each CPU sample keeps its run's five-second peak (`cpu_peak`) beside its mean. A class
has a heavy tail when at least five of its last 32 samples carry a peak and the nearest-rank 90th percentile of those peaks is over
twice its median booking (the median of the means rounded up to a quarter core). Such a class books that percentile, rounded up to
a quarter core, held to the node's cores and, by the placer, to a quarter of the largest host (22 of 88); every other class books
its median exactly as before, so a class whose peaks stay within twice its mean (a steady job, a small one) is not booked above its
use. A run with no peak reported (an agent from before it) counts toward no tail. A pressured or held run is kept, with its peak,
only when its mean exceeds the median of the means, as before. The tail is of the last 32 samples: a class whose bursts stop
books its median again once 32 runs without them have replaced them. The stored summary (version 3) adds the ring of peaks after
the minute ring; a version 1 or 2 row still decodes, with no peaks.

**Why the 90th percentile, twice and five runs.** A class's peaks are its burst; the median of them would leave half the
bursts over their booking, which is the tail that overloads a host, and the maximum (the 100th) lets one outlier run hold the
class high for the next 32. The 90th books what nine runs in ten burst to and leaves the tenth to the slack and the measured load.
Twice the median separates a burst from noise: a run's five-second peak is at least its mean, a steady class's peaks are close to
it, and a farm compile that averages 6 cores and bursts to 20 is over three times it. Five runs with a peak keep a single burst
from lifting a class.

**Mixed classes.** A class whose jobs differ widely in size (one mixed 50x) books its tail for every job, small ones included. The
placer cannot tell a job of such a class's small mode from one of its large mode before it runs: a job record names no input size,
and its arguments name only one action. The split is the client's: a client that sends one `size-class` for work of two sizes
splits it by naming each size its own class (a build farm can add the size of an action's inputs to the class it sends), and each
then books its own median or tail.

**Proof.** `crates/js-sizing/src/tests/tail.rs` holds the rule's boundaries (exactly twice the median books the median, one
millicore more the tail; four peaks are not a tail, five are; a tail ages out after 32 runs; peaks survive storage, and a version 2
row books its median). `crates/js-sched-placer/src/sizes_burst_tests.rs`
(`a_burst_of_a_heavy_tail_class_books_no_host_past_its_cores_while_another_fits`): a class of ten runs averaging 2.75 cores and
peaking at 16 to 70 books 22 cores (its 52-core 90th percentile held to a quarter of a host), and a burst of 24 of its jobs over six
88-core hosts books four on each host, 88 cores; the same runs reported without peaks book 2.75 cores and the whole burst lands on
one host. `js-cpu-replay` replays a trace with the arm `A heavy tail p90, 1.1` beside `A median, 1.1` and reports, per arm, the
most CPU booked on a host at once, the Gini coefficient of the jobs placed per host and the core-seconds the hosts were asked for
past their cores.

