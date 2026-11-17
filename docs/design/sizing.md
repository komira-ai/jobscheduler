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

The estimator is `js-sizing` unchanged: memory is `1.2 x` the largest recent peak plus 128 MiB (at least 1 GiB), CPU the 90th
percentile of CPU used per second of wall time rounded up to a quarter core. A class with too few runs books the cold size:
`server.cold_cpu_millis` and `server.cold_memory_bytes` when the server's file sets them, each one left out a quarter of the most
cores or of the most bookable memory (the pool's `memory_schedulable_percent` applied) of the hosts the engine knows; never more
than those most, and at least the class's kill floor. A class that has an estimate but no CPU sample yet (its only runs were
memory kills) books the estimator's own unlearned CPU, two cores, and not the cold CPU. Before five runs the memory may only rise from the cold memory. A class whose
estimate reaches the largest host's bookable memory books all of it and waits for an empty host.

On a farm whose hosts have 88 cores and 251 GiB, a quarter is 22 cores and 62.75 GiB, so a host holds four cold jobs; a client
that books every build target by its history (CPU and memory 0, the target's name as `size-class`) would run four jobs a host until
each target had five runs. Such a farm sets the cold size to what an unlearned build step uses (two cores and 8 GiB: 31 a host by
memory), and a target that needs more is raised by its runs: a memory kill at the cold size books twice it next, and runs that
come near it raise the next booking after two runs.

The size is decided by the placement pass, not at submit, and is not written on the job: its needs stay as submitted, so a retried
submit with the same id is still the same job. What it was booked is its allocation's `booked`. A contained job's booking is its
`memory.max` and `cpu.max`, and the CPU slack and memory percent bound the booking as they bound every booking.

**What a run teaches.** When a pass sees a job's allocation freed, it reads the job once and gives the estimator one run: an exit 0 is
a success; any other exit, a wall limit or a failed input a failure, which teaches the memory nothing; a stop for memory is a kill at
the job's own limit, which raises the class's floor to twice the booking it died at. A success whose peak came within 5% of its
booking counts as having hit its limit (the estimator's own censoring rule assumes the build-host limit of 7.3), so it can hold the
class at its booking but not lower it. CPU counts as measured at no node pressure: a contained job's CPU is held to its booking by
its `cpu.max`, so what it used is what it was given. Because CPU is capped at the booking, the runs a class makes at the cold size
never teach it more CPU than the cold size; a class that needs more names its CPU (its runs that do are learned too).

**Where the history lives.** Nowhere new: at its start the server teaches the estimator every ended job its job store still holds
(seven days by default), in the order they ended, with what each allocation booked; then each pass teaches the jobs it sees freed.
A failed read at the start is logged and every class starts cold. Each job's usage history row now carries what its host reported
(`used`: the peak memory, and the CPU per second of its run as both the highest and the mean), so `GetUsage` can set what a host
used beside what it booked, and is kept under the name of the job's class as its `profile`: the `size-class` label as it is, else
`run-` and the 16 hex digits of the class's shape. The name is what the job builds (for example `kbf.cargo-test.kbf-server`), the
same for every run of it and never its job id or image digest, so the rows of one build read side by side.
