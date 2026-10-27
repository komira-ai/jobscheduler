# Lease limits, usage and labels

Moved from design.md sections 12.1 to 12.3. Section numbers refer to design.md.

## 12.1 Lease limits (always on)
Max wall time (required), max attempts, max resource size, max arguments size. These bound how long and how large one lease can be, so a
lease always ends: the wall time is the deadline of the attempt, and the fence (5.2) ends the lease when its agent cannot be heard.
Checked at submit, as part of the start contract (5.2), and refused there. A refusal carries a plain constant code; no central code table
is introduced until a second consumer needs one.

**Lease TTL and the reaper.** A held lease also has a TTL: the client renews it before it lapses, and a renewal never extends it past
its acquisition plus the max wall time. The reaper is a periodic tick, the only place time acts: it stops and frees every lease whose
TTL has passed, and retries a start, a stop, an acquire or a release whose outcome was unknown, freeing a lease only when the provider is
observed without it (5.6). So a client that disappears loses its lease at the end of its TTL, never later than its max wall time. The
lease table in this repository (`js-lease`) implements this.

The reaper also owns the machines. In the same tick it (a) releases an elastic host that has been idle past its pool's `idle_timeout` (5.10) (a registered `default` host is never
released, and no minimum is kept), and a tainted elastic host with no lease (5.12), through `release`, and (b) releases a leaked instance: one that `list` returns, that carries this scheduler's tags,
and that no pool host or acquire record accounts for after a grace of ticks. A `list` that fails reaps nothing. The reaper acts on
nothing in its first N ticks after start, and never releases an instance whose agent is connected and holding leases: that instance is
adopted as a pool host from its tags and its hello, or alerts. Pool scaling (5.8) also runs only in `tick`, so `js-lease` stays the one place time acts and scaling is deterministic under an injected clock.

## 12.2 Usage record (always on)
The scheduler's record is per machine lease: instance-hours (attribution, not a charge), wall phases, queue time, and the aggregate usage
the agent reports for the lease. Per-run usage (CPU-seconds, GB-seconds, GPU-seconds, peak memory with basis, and whether it ran, was
cached or was joined) is the consumer's: it arrives from the agent on the work session, and kbf feeds its own sizing history (7, level
2). The scheduler writes per-run records only for classes whose runs it places itself (ucdk). Stored in the state store and queryable,
for sizing and for the run record (8.3), not for charging anyone. This is the "Accounting" record planned in
`docs/design/scheduler.md`. Attempts charged to the platform-fault budget (8.1) are recorded and flagged.

## 12.3 Labels (always on)
Every request carries labels. jobscheduler owns a small label schema and imports no ucdk type: a reserved prefix `jobscheduler.dev/` with
the keys `tenant`, `run`, `validation-run`, `definition`, `instance`, `scheduler`, `pool`, `acquire` and `lease`, each value at most 63 characters of
`[a-z0-9._-]`. The caller mints the validation run id; the scheduler only carries and applies it. A user label under the reserved prefix
is refused at submit. Labels flow request, lease, `Start`, agent. A capacity provider tags each instance it launches with `pool`,
`acquire` and `scheduler` (the scheduler's instance id), and the lease once one is bound, as `jobscheduler.dev/*` instance tags (aws), so `list` and the
reaper can find what is ours and attribution can read it. In ucdk the adapter translates the label set into `Stamp.extra`
(`ucdk/crates/ucdk-provider/src/stamp.rs`); a tagless kind cannot carry it, so a class that carries a validation run id is refused at
submit on such a kind unless its adapter defines a name rule that includes the id.

Runs are not nodes in any ledger, so the sweep key is the `instance` label. Destroying a Job definition, or the environment, cancels or
drains its runs and sweeps billable leftovers by that label; the sweep is part of the closed-world delete, with a test on the fake.
