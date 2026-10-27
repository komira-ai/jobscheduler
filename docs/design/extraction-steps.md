# Extraction steps 0 to 22

Companion to [docs/design.md](../design.md), section 14. These are Phases 0, A, B and C of the extraction ladder: the gates, the moves
inside kbf, the crates that move out, and the wire and the agent. Section numbers refer to design.md. Phases D and E (capacity providers
and pools) are in design.md itself.

**Phase 0. Gates.**

0. Golden traces. Write every trace line in an explicit, hand-written format that does not use `{:?}` (in `crates/kbf-sim/src/sim.rs` and
   the scenario worlds), covering both hash mechanisms of 4.5 (SHA-256 in `kbf-sim`, 64-bit FNV-1a in the F1 capacity world) and saying
   whether the 64-bit hash is replaced. Check in golden hashes per scenario and seed for the CI sweep. Own PR; the old hashes are regenerated once and the reason stated. *Proves:* the golden file fails on a one-event change in a scenario.
1. Golden corpus (13.7) and the cross-version harness skeleton. *Proves:* the corpus decodes and re-encodes byte-identical on main.

**Phase A. Inside kbf, no behaviour change.**

2. Documentation corrections: queue persistence text in `crates/kbf-sched/src/lib.rs` and `docs/design/scheduler.md`; the stale sentence in `crates/kbf-estimator/src/lib.rs` ("Only the offline replay (`kbf-estimator-replay`) calls it yet: the server
   adapts its run records to these types in a later change"; `crates/kbf-server/src/learned.rs` calls it); the lines of `docs/design/failure-classes.md` that still say the booking doubles (found by searching for "doubl"; line 527 says the code no longer doubles); the `memory_bytes` and `MEMORY_KILL_OWN_LIMIT` comments in `worker.proto`. *Proves:* nothing in code changes; docs agree with the files cited.
3. Split `crates/kbf-types` into a generic module and a REAPI module with re-exports. *Proves:* the workspace builds with no import edits
   outside the crate.
4. Introduce `Queue` around the ordered set. *Proves:* golden traces unchanged.
5. Introduce `Fit` and `Rank` with the existing code as the implementation. *Proves:* golden traces unchanged; a test with a second
   trivial `Rank` shows the seam is real.
6. Additive durable fields. Add optional `class_id` (absent means the `kind` enum), `payload`, `labels` and `tenant` to the control-log
   `Request`; the old fields are still written for kbf classes, and decoders accept the new fields. A non-kbf row is written only when
   its class is enabled, and a class stays disabled until every frontend sharing the store reads it. *Proves:* kbf rows byte-identical
   (`control_golden.rs`, `state_golden.rs`); a cross-version test shows an old reader rejects a non-kbf row instead of misreading it.
7. Add optional `fence`, `limits`, `assignments`, `labels` to `kbf.worker.v1` `Start`; teach scheduler and daemon; absent means today's
   behaviour. *Proves:* cross-version matrix both directions; behaviour identical with fields absent.
8. Replace the `LeaseKind` match in `has_room` with `ResourceClass` data; kbf maps its three kinds and stores keep the enum. *Proves:* the
   lease-kind tests (`crates/kbf-sched/tests/lease_kind.rs`, `vm_slots.rs`, `gpu.rs`) pass unchanged.
9. `RetryPolicy`. 9a: introduce the trait with the current four axes and code behind it. 9b: split `memory.rs` below the file-size limit
   with no code change. *Proves:* `crates/kbf-sched/tests/memory.rs`, `requeue.rs`, `lost.rs`, `sim_memory.rs` unchanged.
10. Make `Request` generic over `Payload` and `Outcome` over the result blob, instantiated with `ActionKey`. *Proves:* golden traces
    unchanged; the trivial instantiation carries the full suite (13.6).
11. Make `Resources` a named vector with the four dimensions; `leases` stays a separate typed field. *Proves:* corpus bytes identical; a
    stored or wire `Resources` with an unknown extra dimension is skipped by the old decoder and preserved by the new.
12. Capacity-provider spike. In a test-only crate, drive the draft traits (`CapacityProvider`, `Queue`, `Fit`, `Rank`, `RetryPolicy`,
    `Payload`) and the two-level lease against the fake aws (`js-provider-fake` on `main`); run the three-way observation invariants
    (Unknown never frees a booking, Absent frees exactly once, no double launch for one acquire id) over seeds. The spike crate moves
    into `js-sim` at step 18; `js-provider` already exists on `main` and gains the capacity trait at step 23. Freeze the traits only
    after it passes. *Proves:* the observation invariants of 13.3
    hold over the seeds; a trait change found here costs no crate move.

**Phase B. Move crates out (still building in the kbf workspace).**

Steps 13 to 15 come before `js-types` moves at 16a, so each may use only types that still live in `kbf-types`. The estimator has no
dependencies on other kbf crates (4.3). Where `SizingPolicy` (resource-vector windows) or the capability schema needs a shared type, step
14 or 15 states which type it takes and re-exports it, or the step is ordered after 16a.

13. Move the `Store` trait, fake and conformance suite as `js-state`; the backends stay (D13). *Proves:* all three backends pass the suite
    from the new path; the corpus is unchanged.
14. Move `kbf-estimator` as `js-sizing` with `SizingPolicy`; kbf passes today's values. *Proves:* golden outputs identical on recorded
    input; the codec version is unchanged; `shadow_diff` passes.
15. Move `kbf-caps` matching as `js-caps` with the schema; kbf supplies its tables. First pin kbf's present behaviour on an unknown request
    key with a test that passes on main (own commit). Then make the schema's unknown-key policy a parameter (refuse, or skip and report),
    with kbf supplying today's value; both are tested on the trivial schema. *Proves:* the existing capability tests pass. kbf's policy
    does not change in this PR.
16. 16a: move the kbf side of `js-types` (the crate exists on `main` with the level-1 vocabulary; reconcile the two). 16b: move
    `js-sched` (per-request placement, a consumer of level 1; conditional on D15), and update in the same PR the layering test's pure set, forbidden list and clippy-deny
    check to cover both crates. 16c: the server adapter translating events and effects. *Proves:* golden traces unchanged; `js-sched` has
    no `kbf-*` dependency; `js-sched` holds 99% line and branch with its own tests only.
17. Move the generic durable transitions into `js-store`; kbf extension rows (floors, key floors, estimator rows, done-of) stay. *Proves:*
    recovery tests, multi-frontend tests and the rolling-upgrade test (13.7) unchanged.
18. Move the simulation harness and invariants into `js-sim`; kbf's simulation instantiates it. *Proves:* same seeds, same golden hashes.

**Phase C. Wire and agent.**

19. Extract `js-contain` from `kbf-driver-container`. *Proves:* the driver's cgroup tests pass from the new crate.
20. Define `jobscheduler.agent.v1`. 20a: the proto and the server adapter, no daemon change. 20b: the agent suite and its broken
    providers (13.2) on the fake agent, in `js-conformance`. 20c: the kbf daemon passes the suite through the adapter. *Proves:* the suite is green on
    the daemon and the fake and red on each broken provider.
21. 21a: split the daemon into `js-agent`, the trusted `kbf-executor` and the kbf extras (9.2) with no wire change; the executor is a
    module boundary here and becomes a separate process at step 34. 21b: switch the daemon to agent v1 using the negotiation of 10.3; it ships only after the release containing step 20 is the minimum server. *Proves:* the cross-version matrix including the downgrade after the
    switch; the same seeded scenario run through the real daemon on the fake runtime yields the same event trace as the v1 path; a
    soak of real daemons counts explicit events with zero unacknowledged results and asserts no elapsed time.
22. Move the extracted crates in, after Phase C and as a mechanical move: the jobscheduler repository already exists with the merged
    crates; copy the crates built in the kbf workspace into it with history; kbf pins it by revision; the layering test becomes a cross-repository dependency check; each crate enters `coverage-baseline` at zero (R6); the
    public-repo lint runs on the new repository. *Proves:* golden traces and corpus unchanged.

# Gates carried with the ladder

Moved from design.md sections 10.4, 13.4, 13.6 and 13.7. Section numbers refer to design.md.

## 10.4 How kbf keeps working at every step
1. Each PR leaves kbf as the sole implementer of any new trait.
2. The gate for each PR from step 3 on is: the golden trace file (step 0) is byte-identical; the golden corpus (step 1) decodes and
   re-encodes byte-identical; per-crate line and branch coverage is stated old and new, not lower (R6), and a moved crate passes kbf's 99%
   with its own tests only (zero once it is in this repository, 13.6); the CI build is green. A change to a trace line's format or to a Debug output is a step 0 change,
   not an extraction side effect.
3. A wire change is optional-field only. The cross-version tests (`crates/kbf-integration-test/tests/cross_version.rs`) run a matrix:
   {old, new} daemon against {old, new} frontend; two frontends of different versions sharing one store, with a daemon reconnecting
   between them; and a downgrade after the switch.
4. Rollback is a revert of the PR. No stored row changes until step 6, which is additive: kbf rows stay byte-identical (golden test); a
   row for a non-kbf class is written only when that class is enabled, and disabling the class, not a revert, is its rollback.

## 13.4 Request-history replay
`kbf-estimator-replay` becomes the template: a generic trace of (arrival, need, duration, key, capacity) and a pluggable arm, run through the real scheduler in virtual time with null, floor and candidate arms and Wilson intervals, labelled an upper bound. Export and row shaping stay kbf glue.

## 13.6 Coverage
(R6.) A crate in this repository has zero uncovered lines and branches, and a crate still in kbf holds kbf's 99% or its recorded floor.
The gate counts a generic by its best single instantiation, and a function compiled into several test binaries by its best-covered copy,
so every branch of a body must be hit by one instantiation in one test binary: the trivial test type gets the full suite, or
arm-bearing bodies are made non-generic helpers. A new crate enters at zero. `js-provider-aws` keeps its cloud client behind a trait with
a fake, so the code that touches the network is minimal and covered. Each crate moved from kbf reaches zero with its own tests before it
enters `coverage-baseline`; `js-sched` must get there without those of `kbf-sim` or `kbf-sim-cell`, so tests move with it or are written
first. Test target names must not collide with files that tests write under `target/`. A bug fix ships with a test that failed before it.

## 13.7 Persisted and wire compatibility
The golden trace hashes say nothing about stored or sent bytes. Step 1 commits a corpus of control-log rows, store rows and wire messages
produced by the last released binaries. Every step that touches a persisted or sent type decodes the corpus and re-encodes it
byte-identical. A rolling-upgrade recovery test recovers state written by the previous release with the new one, and reads the new
release's writes with the previous one. The lease file gains its pool name and acquire records additively (R3): a test pins that the new
server reads a file written before, and that an older server skips the new keys.

