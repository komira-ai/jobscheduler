# Extraction steps 23 to 47

Companion to [docs/design.md](../design.md), section 14. Phases D and E moved here from design.md unchanged, and Phase F (capabilities as
data and the VM backend) follows them. Phases 0, A, B and C (steps 0 to 22) are in [extraction-steps.md](extraction-steps.md). Section
numbers refer to design.md.

**Phase D. New capability.**

23. Change `js-provider`: rename `Provider` to `Pool` and `ProviderId` to `PoolId`, delete `Flavour`, rename `capacity()` to `hosts()`, add
    `CapacityProvider` with `list`, `AcquireAck`, `ReleaseAck` and `Instance` beside them (5.9), and model the fence and no-retries rules in
    `StartSpec`. Add the new `Stop` effect and its retained-intent row (5.1, 5.2). *Proves:* the merged tests pass unchanged apart from the
    names, so the GPU-host service is unchanged; a stop of a held lease is resent until the lease leaves the heartbeat.
24. Usage record per lease (12.2); labels (12.3). *Proves:* usage on failure; labels on every fake resource.
24b. The *task failure* retry axis of 8.1 and the portable `max_retries` budget. *Proves:* a UJ1/UJ2 case on the fake in which a
    non-zero exit is retried up to `max_retries` and a kbf class with budget zero is not retried.
25. Schedule triggers and run records; the launcher API. *Proves:* run record before return; missed ticks skipped; overlap rules, all
    with an injected clock.
26. ucdk side: the launcher wiring and, later, the provider crate, in the ucdk repository.

**Phase E. Capacity providers and pools.** Steps 27 to 29 need no agent. Step 30 needs steps 20 and 21a. Step 31 is tested on fake agents.

27. `js-config`: the schema of 5.7, the loader and its validation; the merged `--pool-config` file loads as one implicit provider.
    *Proves:* each rule of 5.7 refuses with the key and line; an old file loads unchanged; the layering test: no crate below the binary
    names a provider kind.
28. The fake capacity provider (13.1) and the capacity suite with its broken providers (13.2); `js-conformance` re-keyed. *Proves:* the
    suite is green on `static` and the fake and red on each broken provider.
29. The lease table: the `acquiring` state, the acquire record committed before the call, acquire off the table lock (D19), the pool
    name in the lease file, and share leases under `Book` admission. *Proves:* a crash between the record and the call resolves by `list`;
    a lease file written before the change loads; the GPU-host tests pass unchanged.
30. Agent registration in `js-agent` and the server: hello with identity and shape, admission, join credentials, deny file,
    `RegisteredAgentSource`. *Proves:* a host with a wrong credential, an unknown pool, a duplicate id, a rebinding or a shape that does
    not satisfy is refused; a denied host's stream closes at the next tick; a lost host's leases are given up once.
30b. Reuse and integrity in `js-agent` and the server: post-job removal and slot freeing, the `isolation` field and its three consents,
    the post-job checks, the taint report and the durable tainted set, `js-cli host clear-taint`. *Proves:* the taint paths of 13.1 on
    the fake agent; a `host` start without any one consent is refused; a taint is not cleared by a clean hello; a tainted id is refused.
31. The pool scaler in `tick`: `min_machines`, `max_machines`, `max_step`, cooldown, idle and boot timeouts, the boot-failure brake,
    `releasing`, replacement of tainted hosts. *Proves:* the invariants of 13.3 over seeds, with an injected clock and no elapsed-time assertion.
31b. Placement reuse: a freed slot places queued work that fits before any acquire. *Proves:* a burst that fits idle machines acquires
    nothing; the same seeded run is identical with the scaler on or off when no demand exceeds capacity.
32. `js-provider-aws` against a fake cloud client, disabled by default; seating becomes re-adoption by tag; the IAM scope of 12.4.
    *Proves:* tags on every created instance; an Unknown `acquire` never launches twice; a restart re-adopts and reaps; the documented
    policy passes the check of 12.4.
33. Consumer pool selection: class and label selectors in the lease request, and `--pool`, `--class`, `--label` in `js-cli`. *Proves:*
    no match is refused, a pool at `max_machines` queues, a selector picks the right pool.
34. `kbf-executor` beside `js-agent`, with host mounts for the input cache (9.2). *Proves:* the action container holds no storage
    credential; a container that reads the executor's credential path, the metadata endpoint or a mount outside the roots fails, and
    the agent refuses a `Start` naming such a mount; the same seeded scenario yields the same event trace as before the split.
34b. kbf as a consumer. The kbf server holds a standing machine lease per existing daemon host (a static pool of kbf's own hosts, a node
    lease at first), renews it, and places actions only inside its leases. Lease demand and release follow kbf's queue (a configured
    floor of whole-node leases, more on queue depth, drained before release); each frontend leases only for the hosts it owns, as
    today. *Proves:* golden traces and corpus unchanged with the scheduler in the loop on fakes; a scheduler outage inside the TTL
    changes no placement; the cross-version matrix with a frontend pair.
35. The first real aws pool, disabled by default; enabling it needs the go in R7.

**Phase F. Capabilities as data and the VM backend.** Steps 36 to 40 need no VM. Steps 41 to 47 need step 30b (reuse and integrity in the
agent) and the merged lifecycle. Step 36 can come before step 15; step 15 then moves kbf's comparators in as schema entries of the crate
step 36 made. Steps 43 (the size fields) and 45 wait for D26.

36. `js-caps`: the typed model, the version order, the schema, the constraint evaluator with host and guest scope, and the refusal codes
    (5.13, [capabilities.md](capabilities.md)); no wire, no server. *Proves:* a case per op and kind that holds and one that does not;
    the order properties on generated versions; absent is unmet; each refusal code; an unknown op is refused, not skipped.
37. The wire: `Hello.facts`, `Heartbeat.facts_rev` and `facts`, `HeartbeatReply.facts_ack`, the new limits, the mirror into the legacy
    map, in `js-agent-wire`; the server stores the newest snapshot per host. *Proves:* the golden corpus gains the messages; a decoder of
    the previous version skips them; a legacy-only hello still meets a scalar constraint; a conflicting legacy entry is `invalid-hello`; a
    lost ack repeats the snapshot and a repeat is ignored; each limit refuses.
38. Facts in `js-agent`: `os_version`, `xcode`, `sim.*` probes through the bounded command runner, `--cap`, `--facts-interval`, the
    revision bump on change. *Proves:* on the fake runner each probe, a failing probe reports the key absent with a warning, a flag
    overrides a probe, and the revision moves only when a value changed.
39. Needs in the lease request and matching in the server: `needs`, the refusal codes at submit, host and guest scope, the image named in
    the grant, the lease record's `rev` and image (additive), and the unservable wait for a constraint no host meets. *Proves:* the worked
    example of capabilities.md on three fake hosts; an old lease file loads; an older server skips the new keys; a stale revision does not
    match; a malformed constraint is refused before it is queued.
40. The consumer side for kbf: the translation table in kbf's configuration and its test of the worked example. *Proves:* the table
    yields exactly the constraints of the example; an unknown platform property follows kbf's policy of step 15.
41. The `Sandbox` seam in the lifecycle, with `ContainerSandbox` the only implementation. *Proves:* the lifecycle tests of the container
    path pass unchanged; the coverage of `js-agent` is not lower (R6).
42. `VmRuntime`, `GuestChannel`, the fake VM runtime, `js-guest-wire` and its hostile-frame corpus, and the `JobSpec` fields `runtime` and
    `vm` with their validation (5.14). *Proves:* a `host` VM job and a GPU VM job are refused; the old decoder skips the fields; every
    hostile frame ends a run with `vm-guest-protocol` and nothing else.
43. The VM lifecycle in `js-agent` (`VmSandbox`): clone, boot, ready, inputs, run, collect, result, destroy, scratch, checks, free; the
    start-up sweep; the VM store check; `base_images` and `image_profiles` as facts. Size fields wait for D26. *Proves:* the cleanup order
    on the fake; a failed destroy frees nothing and taints; no VM serves two runs; a restart destroys every clone; the guest never holds a
    writable host path.
44. Mixed-host accounting: the `vms` dimension in `js-types` (it is refused today as unknown), VM bookings in full, containers checked
    against the total less VM memory, the offer rules of 5.15. *Proves:* the offer is never above total less booked less warm over seeded
    runs; the example of 5.15; the previous pools load unchanged.
45. The warm pool: `--vm-warm`, assignment, refill, maximum age, the failure brake, `warm_images`. Waits for D26. *Proves:* a warm VM is
    never handed a job twice; assignment is irreversible; refill is one at a time and only into free capacity; age and the brake with an
    injected clock.
46. The whole-node class: the `exclusive` and `exclusive_ready` fields, the withheld lease token, drain of the warm pool,
    `prepare_timeout`, restore. *Proves:* the sequence of 5.15 on the fake agent; the token is not minted before the report; a timeout
    gives the lease up; an older client that ignores readiness still waits.
47. The first real VM driver, disabled by default, on an operator's host; it passes the VM conformance suite there. *Proves:* the same
    suite as the fake; it is not run on shared CI.
