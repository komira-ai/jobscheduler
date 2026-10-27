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
