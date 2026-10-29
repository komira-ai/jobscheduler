# Decisions D1 to D65

Moved from design.md section 15 to keep that file under its size limit; design.md holds one line for each decision and D35 in full.
Section numbers refer to design.md. Each decision has options and a recommendation. None is final until the owner answers, except where
marked settled. The ids run D1 to D65 with no gaps. D26 is open. Superseded decisions are marked and say by which decision.

- **D1. Home of the code.** Settled: the code lives in this repository, where the merged crates already are. Crates extracted from kbf
  move in at step 22; until then they build in the kbf workspace, and kbf pins this repository by revision.
- **D2. Wire strategy.** (a) dual stack in the daemon; (b) a server-side adapter and a new package with negotiation; (c) edit v1 in
  place. (c) breaks R3. Recommend (b), daemon switched last and only after the server advertises the package. Registration rides the same
  package (10.3).
- **D3. Durable leases across agent restart.** (a) required in v1; (b) not in v1, an agent capability flag. Recommend (b); kbf kills
  leases with the daemon today. An instance that terminates takes its leases with it, so "leases die with the host" is now the normal case.
- **D4. Sizing rule for seen work, and the ladder.** R1 says last peak plus buffer; the shipped estimator is a long-window maximum plus 20%
  and 128 MiB, and `on` mode applies only the kill-driven floor. Options: (a) implement R1 as `SeenRule::LastPeakPlusBuffer` and replay
  it; (b) keep the shipped estimator behind the generic API and amend R1; (c) both selectable per class. Where "a lease is a node or
  half a node" lives is settled: it is a level-1 machine lease (9.1). Second part: R1's "next rung above" is implemented as above the *booking*,
  not above the limit just used (7.4). Options: keep (current behaviour) or move to above-the-limit, which is a behaviour change with its
  own replay proof and not part of the extraction. Recommend (c) with (a) as the default after a replay supports it, and keeping the
  ladder as it ships.
- **D5. ucdk role.** (a) launcher only; (b) infrastructure provider only; (c) both. Recommend (c), launcher first, as an optional
  realization beside ucdk's direct launcher (11.1).
- **D6. Fence.** (a) self-fence for everything; (b) the class declares a default (self-fence unless the definition declares hermetic or
  idempotent) and every request resolves and records its fence at submit, so no default is silent. Recommend (b).
- **D7. Memory booking for non-kbf classes. Amended by D56.** (a) same as kbf; (b) `Book` for ucdk job classes, with the declared size as
  booking and ceiling and history advisory only, so that many jobs share a host (it first said `BookAll`, a whole free host for each job).
  Recommend (b); kbf actions keep today's rule.
- **D8. Validation run id and label key.** The caller mints; the scheduler carries and applies; the key is `jobscheduler.dev/validation-run`
  (12.3). Recommend yes.
- **D9. Triggers.** Cron-like triggers live in job definitions as data, with UJ3 semantics, when Job is realized by jobscheduler; the
  cloud scheduler role node is dropped in that realization and kept in the direct one (11.2). Recommend yes.
- **D10. Resources and capability schema.** Named resource vector in v1 with the lease-count cap separate; whole-device GPUs, no fractional
  GPUs. Capability schema: provider-registered, namespaced keys, unknown request keys refused, unknown report keys skipped. Recommend yes.
- **D11. Consensus.** The generic core relies on the state store's conditional writes for who places (`crates/kbf-sched/src/scheduler/handoff.rs`), not on a
  consensus log. `crates/kbf-raft` exists in kbf. Options: (a) the core requires only the store; (b) the core also supports a replicated
  log. Recommend (a); `kbf-raft` stays in kbf until a second user asks.
- **D12. Name collision.** jobscheduler's *capacity provider* and ucdk's *Provider* trait are different. Options: rename ours to *source*
  or *backend*; or keep and always qualify. The ucdk-side adapter lives in ucdk (11.1) and no jobscheduler code names ucdk's trait, so the
  clash is confined to prose. Recommend keeping "capacity provider" here and writing "infrastructure provider" for the ucdk side; the
  rename is the owner's option if the prose proves confusing. Inside this repository the merged trait `Provider` is renamed (D16).
- **D13. Where the state store lives.** `kbf-state` is general (thirteen kbf crates use it, 4.2). Options: (a) move it all into
  jobscheduler, which widens "kbf depends on jobscheduler" to installer, enrollment and history; (b) move only the trait, fake and
  conformance suite as `js-state` and keep the SQLite and Postgres backends in kbf; (c) keep it all in kbf behind a neutral crate.
  Recommend (b).
- **D14. Fleet scaling (reverses N2). Amended by D38 (no minimum).** Settled by the project owner; recorded here with its reason. N2
  excluded fleet autoscaling. With no provider that scales its own fleet, nothing else scales a pool: a machine count held by hand is wrong
  whenever demand moves. So where a pool's hierarchy has an elastic provider, the scheduler has it get and give machines up to the pool's
  `max_machines` (5.8). What does not change: `crates/kbf-coordinator` still only sets desired versions, and no cost guardrail is added (N4).
- **D15. Scope of the extracted core.** (a) The core is level 1 only: machines, small, with no per-request placement. (b) Level 1 is the
  base and per-request placement (`js-sched`, steps 3 to 18) is a consumer of it; kbf is a consumer too. (c) Per-request only, with level 1
  a client of it. Recommend (b): level 1 is merged and small, ucdk needs per-request launch, status and retries (11), and kbf keeps
  `kbf-sched` inside its leases until `js-sched` is proven. Steps 3 to 18 are conditional on this.
- **D16. Names.** (a) Rename the merged `Provider` to `Pool` and `ProviderId` to `PoolId`, delete `Flavour`, and add `CapacityProvider`;
  (b) keep `Provider` and add `CapacityProvider` beside it. Recommend (a): "provider" now means a capacity provider (3, D12), and two
  traits that differ by a prefix would be misread.
- **D17. Scaling policy.** Demand is the leases in `acquiring`, or that plus a look-ahead. `max_step`, `cooldown`, `idle_timeout` and `boot_timeout` are
  per-pool configuration, unset meaning none (`max_step` defaults to 2). Recommend the simple form of 5.8 and no look-ahead until a trace shows a need.
- **D18. Self-registration auth.** (a) A per-pool join token; (b) a client certificate per pool, reusing the identity path of
  `crates/kbf-auth` and `crates/kbf-oidc`; (c) both, chosen per provider. Recommend (a) now, (b) added later without a wire change
  (R3). For a launched machine, the cloud's signed instance identity (aws), never a secret in the launch template.
- **D19. Acquire and the table lock.** The merged server calls the provider with the table lock held, so a slow call stalls every
  request. (a) Asynchronous acquire: the table records `acquiring` and returns, the scaler calls outside the lock, and the result is an
  event. (b) Keep the synchronous call with a timeout and answer every slow launch Unknown. Recommend (a).
- **D20. Config format and validation.** One TOML file, `deny_unknown_fields`, validated whole at load, secrets as paths, no reload in
  the first slice (a restart re-reads it). Alternatives: one file per provider, or YAML. Recommend one TOML file, as `js-pool-static` does.
- **D21. Who pays cold start. SUPERSEDED by D38.** It offered (a) the queue pays, or (b) `min_machines` idle hosts per pool. There is no
  `min_machines`: the queue pays, and the wait counts toward `UNSERVABLE_WAIT` (8.2).
- **D22. Work-session topology.** (a) The agent dials the consumer; (b) the consumer dials the agent; (c) the scheduler relays. Recommend
  (a): agents only dial out, as the kbf daemon does today (4.4), and the scheduler stays out of the data path.
- **D23. Machine reuse and release. Option (c) SUPERSEDED by D38.** (a) One machine per job, terminated after it; (b) a machine serves jobs
  back to back and is released after `idle_timeout`; (c) b plus a floor of idle hosts, which D38 removes. Recommend (b). Reuse is safe because
  the agent removes the container and scratch and checks the host after every job (5.10, 5.12).
- **D24. Caps.** (a) `max_machines` only; (b) `max_machines` plus a per-tick `max_step`, bounded to real queued demand; (c) b plus a
  consumer cap. Recommend (c); the consumer cap is the consumer's own and is not read by the scheduler. This is overload protection, not
  a cost guardrail, and the earlier ruling against cost guardrails (N4) stands (5.11). D39 adds the per-provider share of `max_machines`.
- **D25. Isolation and taint.** (a) Every job contained, no host jobs; (b) a job declares `contained` or `host`, a host job taints its
  machine, the agent checks after every job and taints on any finding, and a tainted machine is replaced or cordoned, never cleared on
  the agent's word; (c) b, and the scheduler also records the taint itself when it grants a `host_jobs` lease or sends a host `Start`,
  before the job runs. Recommend (c), host jobs off by default and needing consumer, pool and agent consent (5.12). The scheduler's record is of what it granted, not a check of the host, so it does not depend on an agent that may be compromised; the agent's report is the second path.
- **D26. macOS VM sizing: fixed or dynamic CPU and memory. OPEN; another engineer is answering it, and the answer is folded in here.**
  Facts: the virtualization framework fixes a VM's CPU count and memory at creation; a memory balloon can only reclaim memory and give it
  back, never grow a VM past its configured size; a VM is cloned and booted for each run, so any size can be chosen when the run starts;
  booted VM memory is committed in full (5.15). (A) **Fixed size classes** declared by the agent; a lease names a class; bookings are a
  few quantized vectors; a job larger than the largest class cannot use a VM and a small job pays for a class. (B) **Sizing per run**: the
  lease states CPUs and memory and each run's VM is created at the size it needs, within the lease's booking; bookings are arbitrary, so
  packing matters more (6.5). The trade-off is the boot cost per run, which neither option changes. The wire carries the VM's size under
  both, and the lifecycle, the channel and the integrity rules do not change; only the size fields (step 43) wait. No recommendation
  until the answer arrives; detail in [vm-backend.md](vm-backend.md).
- **D27. How sets and counters ride the wire.** (a) Pack them into the existing string map (`xcode=16.0,16.2`); (b) an additive typed
  `facts` field. Recommend (b): (a) needs a parsing convention per key, hits the 128-byte value cap, and cannot distinguish a set from a
  scalar; (b) is optional and skipped by an old server, and scalars are mirrored into the old map so old servers still match them (5.13).
- **D28. Runtime traits for VMs.** (a) Widen `ContainerRuntime` to cover VMs; (b) a second trait, `VmRuntime`, with one lifecycle over an
  internal `Sandbox` seam; (c) a second lifecycle. Recommend (b): the stages, channel, destroy and restart rules differ, (a) forces optional
  parts on every backend and changes merged code, and (c) writes the cleanup order, results and slot accounting twice (5.14).
- **D29. VM reuse. Settled by the project owner:** a fresh VM for each run, cloned from its base image and destroyed after the run; a lease
  may hold several runs and never reuses a VM between them. Rejected: reverting a used VM to a snapshot or cleaning it in place, because
  what remains of a guest after a job cannot be shown to be clean; destroying it can. The cost is a boot per run, which D26 weighs.
- **D30. Readiness of a whole-node lease.** (a) Assume the host is quiet at the grant; (b) the agent reports ready after it has confirmed
  that no VM is running and finished running work, and the lease token is minted only then. Recommend (b): (a) lets a performance test
  start beside a booted VM, and the token gate needs no change in an older client (5.15). The whole class, its drain and its caps are
  D46 to D52 ([whole-node.md](whole-node.md)); the "quiet" of (b) there also covers no run, clean checks and no taint.
- **D31. Matching what is inside an image.** (a) The consumer names the image and constrains `base_images has <name>`; (b) each image has a
  profile and a `guest` scope matches when one image meets all the guest constraints. Recommend (b), with (a) always available: Xcode and
  runtimes are properties of the image the VM boots, and a consumer should ask for what it needs, not memorise image names (5.13).
- **D32. GPU placement. Settled by the project owner; amended by D35.** GPUs exist only on the Mac Studio machines of the GPU pool (two
  today). Their shape declares `gpu_class = "apple-m"` and `gpus = 1`; a GPU request (`gpus > 0` or a `gpu_class` constraint) is
  satisfiable only by that pool. It queues when its registered hosts are busy, with no acquire made for it, and (D35) is refused with
  `no-capable-host` when no registered GPU host could ever serve it. The whole-host GPU lease service (the merged class `gpu-host`, now the
  whole-node class of D46, whose pool `class = "gpu-host"` loads as `whole-node`) is the only GPU path; its hosts register through `js-agent` (D36); a VM gets no GPU; container GPU jobs on Linux are not a current use
  ([capabilities.md](capabilities.md)).
- **D33. Deployment shape and state store. Direction settled by the project owner; the store is chosen here.** The target is one service in
  Kubernetes (scheduler, lease API, agent API) with kbf as a client and `js-agent` registering with it; the single-host `js-lease-server`
  is the stepping stone. State: (a) the lease file and the binding file on a volume, one writer; (b) an external store with conditional
  writes. Recommend (a): it is the merged persistence with its crash-safety rules (atomic replace, commit before act, the lock, fail-closed
  boot, observe once) unchanged; the table is small; there is nothing more to operate. (b) when the service must survive a node or zone
  loss without a minutes-long outage, when the rewrite shows (K29), or when a second replica is wanted. One replica under `Recreate`
  (5.16, [deployment.md](deployment.md)).
- **D34. Leases across a restart.** (a) As merged: the first tick after a boot expires every lease whose expiry passed in the outage;
  (b) none expires before `--boot-grace` has passed after a boot (off by default); absent and unknown observations are unchanged.
  Recommend (b), the grace set to the default TTL (step 49).
- **D35. Admission against registered hosts: refuse when no host could ever serve, queue when one is busy. Settled by the project owner
  (reading (a)).** A `DefaultProvider` pool refuses a request with `no-capable-host`, at submit, only when no registered host could ever
  satisfy its constraints and shape (a host that exists, is not tainted and is merely busy, lost or cordoned is capable), unless an
  elastic provider of the pool could launch a machine that meets it; when a capable host exists but is busy, the request waits in the
  queue. A queued request whose last capable host leaves keeps waiting for `regain_grace` before it is refused (D41). Refusing whenever no
  host is free now was considered and rejected: it turns an ordinary busy fleet into errors and moves the queue into every caller (two
  requests for the one GPU host would refuse the second). Text in [providers.md](providers.md).
- **D36. The static pool is for testing only. Settled by the project owner.** Operator-listed hosts are a test provider: `js-pool-static`
  stays for tests and conformance, a production configuration declares none (`--allow-test-providers` gates it), and every production host
  registers through `js-agent`. The single-host GPU lease service's operator-listed pool (`--pool-config`) is the stepping stone: its two
  machines move to running `js-agent` and registering with a `default` provider (step 54), after which the file is retired. Until then it
  keeps working unchanged.
- **D37. The default provider and elastic registration. Direction settled by the project owner; the details are chosen here.** (a) The
  `default` kind (`DefaultProvider`) is the hosts that registered themselves; it never creates or destroys compute, has no acquire and no
  release, and its core is the merged `RegisteredProvider`. (b) An elastic provider's `acquire` boots a machine running `js-agent`, which
  registers itself with the cloud's signed identity; the provider's job ends at boot plus registration (or failure or timeout), and
  `release` terminates. A registered elastic host is an ordinary host. Alternative for (b): the provider hands the scheduler a ready host
  and registers it on the agent's behalf; rejected, because the agent's own hello is the one proof that it is up and reachable, and the same
  admission then holds for every host. Recommend (a) and (b) as stated.
- **D38. No minimum machines, anywhere. Settled by the project owner; supersedes D21 and D23(c).** `min_machines` is removed: from the
  schema, validation, scaling rules, the simulation invariants, the examples and the risk mitigations. A pool specifies `max_machines`
  only; nothing is kept warm; the queue pays the cold start, and a pool of elastic entries is empty when idle. A file that names `min_machines`
  or `min` is refused with a message saying so (5.7).
- **D39. A provider hierarchy. Direction settled by the project owner; the rules are chosen here.** A pool lists providers in order and takes
  capacity from the first that can satisfy; the default order is `default`, then the elastic provider. Options for "can satisfy now": (a) a
  free capable host, else (for an elastic entry) an acquire possible under its caps; (b) as (a) but a busy earlier host makes the request
  wait for it rather than fall through. Recommend (a), as the owner described: "falling through only when the earlier ones cannot
  satisfy" with a busy host not satisfying now; (b) would make a burst wait behind a long job while cloud capacity is available. Two passes
  (free host anywhere first, then acquire in the first entry that can) so no machine idles while another is launched. The hierarchy is
  per pool, and a class that needs one order across shapes gives its pools the same list. `max_machines` is the pool's total; an optional
  `provider_max_machines.<name>` bounds one provider's share. Text in [providers.md](providers.md).
- **D40. A host frees while a later entry's machine is booting.** (a) Release the booting machine; (b) keep it and let the idle rule
  release it. Recommend (b): the acquire was recorded before the call and is already paid for, a terminate mid-boot can read back Unknown,
  and the next burst would launch again after a `cooldown`; keeping it bounds the waste to one boot plus one `idle_timeout`, and
  requests queued meanwhile use it. The waiting request goes to the freed host at once (providers.md).
- **D41. What counts as capable, and queued requests when capability goes.** Capable: in the host table, not tainted, not denied, not
  draining, not `releasing`, total shape and facts meeting the request. Busy, lost (inside the retention window), cordoned and quarantined
  hosts are capable; **tainted hosts are not**, because a taint ends only by an operator reimaging the machine, an unbounded human wait. A
  queued request whose last capable host leaves (and no elastic provider could serve it) waits `regain_grace` (default 5 minutes on the
  injected clock) and is then refused with `no-capable-host`; a new submission gets no grace. Alternatives: refuse at once (a routine agent
  restart would refuse every queued request), or wait for ever (a wait nobody can end). Recommend the grace.
- **D42. (Superseded: see the note after D45.) kbf's update coordinator is not generalised; one version per host. Settled by the project owner, on the advice of the owner of
  kbf's update coordinator.** That coordinator is single-service today (one desired version per host, limits keyed by kbf's four roles, one
  heartbeat version and health per host, a helper that installs one signed kbf package). Options: (a) generalise it to a service map now;
  (b) leave it as it is. Adopted: (b). The jobscheduler server updates by its own Kubernetes redeploy ([deployment.md](deployment.md); an
  operator act with the owner's go, R7). When kbf's daemon splits into `js-agent` plus `kbf-executor` (9.2), both ship in the one kbf host
  package the updater already installs, so a host keeps one version and the coordinator needs no change. Rejected (a): it buys nothing
  while every agent runs kbf, and it changes a working updater and its trust. Text in [rollout.md](rollout.md).
- **D43. (Superseded: see the note after D45.) Rollout order. Settled by the project owner.** The jobscheduler server first; then hosts (`js-agent` with `kbf-executor` in the
  kbf package, rolled by kbf's coordinator with its own slots and gates); then kbf frontends, as clients, once the server they call
  supports what they send. Servers before agents is the wire rule (10.3). Elastic machines boot the current image at launch, so scale-up
  is their update path. Text in [rollout.md](rollout.md).
- **D44. (Superseded: see the note after D45.) Compatibility rules between server, agent and kbf. Settled by the project owner.** Protocol majors, with additive fields inside a
  major (R3, 10.3); the server speaks every protocol major that its agents and kbf clients use, and refuses an agent major it does not
  speak; kbf clients negotiate or check the server's advertised version and features before sending what an older server would skip; if
  `js-agent` and `kbf-executor` ever ship apart, a version-pair check at the agent (it refuses a start it cannot pair) and not a service
  map in the coordinator. Step 55.
- **D45. (Superseded: see the note after D45.) What would change D42, and the decision that comes first. Direction settled; each trigger is decided when it happens.** (1) A
  `js-agent` population that does not run kbf and must be updated: a second coordinator instance, or replace-not-update for elastic
  machines. (2) `js-agent` and `kbf-executor` on different release cadences with a required order: the version-pair check at the agent
  first, not a coordinator service map. (3) jobscheduler releasing faster than kbf with urgent agent fixes: package coupling hurts (K35),
  and the options are reopened. (4) The kbf helper's single signer not extending to jobscheduler's release signer: a security decision
  for the owner before any option (K36).

> **D42 to D45 are superseded** by [kbf-on-js-agent.md](kbf-on-js-agent.md) (the design for the owner's model) and the rewritten
> [rollout.md](rollout.md). The premises they rest on, a separate jobscheduler server, a trusted `kbf-executor` and `js-agent` shipping
> inside the one kbf host package, no longer hold. The replacements: D42 the coordinator is already multi-service and `js-agent` is a
> service; D43 the order is the kbf service (storage, daemons, frontends) and then `js-agent` with `after = ["kbf"]`; D44 compatibility by
> protocol major, additive fields and capability gating, with the version-pair check replaced by task spec versus `job-io`; D45 the triggers
> in rollout.md, with the signer decision (K36) taken before any production install. The original text is kept above as history.
- **D46. Giving a whole node is a lease class of its own. Direction settled by the project owner; the rules are chosen here.** (a) A large
  resource request under `Book` or `BookAll`; (b) a class `whole-node` with its own admission `Whole`: exclusive, quiet, reserved by a
  drain, capped. Adopted: (b). A big request is placed beside other work, starts on any state, is never drained for and has no cap; the
  owner's requirement (nothing else placed for the lease, drained first, the shape for GPU testing) is a different promise. `BookAll` stays
  as an admission (a whole free host for one lease; ucdk Job classes book a share, D7, D56) without reservation, quiet or cap. kbf's `whole_machine` and the merged `gpu-host` are this class (a pool
  `class = "gpu-host"` loads as `whole-node`); the GPU path on the Mac Studio machines is a whole-node lease, and a GPU request under any
  other class matches no GPU pool (D32). A node lease that books a whole shape under `Book` is not whole-node. Text in
  [whole-node.md](whole-node.md).
- **D47. Admission and reservation of a whole-node request.** Eligible hosts are the capable hosts of D41 in the matching pools; the request
  is refused only when no registered host could ever be given whole and no elastic provider could launch one (`no-capable-host`, D35), else
  queued in queue order. It takes a quiet host at once, else (after `drain_after`, default zero) reserves exactly one host: the least worst-case
  drain (the latest end bound of its leases and runs), then the fewest leases, then the lowest host id; it takes any host that becomes quiet
  first and does not switch to a cheaper one otherwise. Alternatives: reserve every eligible host (idles the pool), or wait for a quiet host
  with no reservation (starves, which R1's one allowed reservation exists to prevent). Recommend as stated.
- **D48. The drain bound: re-queue, never kill.** After `drain_deadline` the reservation is given up, the host returns to `serving` with its
  work untouched, and the request reserves another host, the first excluded for `reserve_backoff`. The default deadline is the pool's max
  wall time, by which every lease on the host at the reservation has ended. Options: (a) re-queue on another host; (b) stop the running work
  after a grace; (c) wait without a bound. Recommend (a): (b) silently destroys other tenants' results for one consumer's convenience, and
  (c) lets a request hold a host and idle it for ever. The scheduler stops nothing for a reservation; the renewal answer carries
  `host_reserved` (additive) so a cooperating consumer releases early.
- **D49. Caps.** Per pool: `max_whole_node` (reserved plus held), `max_draining` (default 1), `max_whole_node_per_consumer` (default 1),
  `keep_for_ordinary` (default 1 in a mixed pool, 0 in a dedicated one); the effective ceiling is the least of `max_whole_node` and capable
  hosts less `keep_for_ordinary`. A reservation does not start on the last unreserved capable host of an ordinary request queued ahead of
  it unless `allow_last_capable` is set. A held lease is never revoked when the ceiling falls. A mixed pool with no `max_whole_node` does not
  offer the class. Options: a fraction of the pool (rejected: no protection in a two-host pool), a global cap (rejected: one consumer takes
  every slot). Protecting only older queued work keeps the rule from starving the whole-node request.
- **D50. Elastic providers: acquire before draining.** When an entry of the pool's hierarchy can launch a machine that meets the request, the
  request acquires one rather than reserving a busy host (D39's two passes; a launch idles nothing, a drain idles a host for up to a max
  wall time). The machine is committed to the request at registration. A quiet host that appears while it boots takes the request and the
  machine is released by the idle rule (D40). Alternative: drain first and acquire as a fallback; rejected as slower and idle-making.
- **D51. Host jobs only in a whole-node lease.** A lease may carry `host_jobs` only if it is whole-node, in addition to the three consents
  of 5.12. Options: (a) any class, as 5.12 said; (b) whole-node only. Recommend (b): a host job taints its host unconditionally, and in a
  share that would destroy other consumers' work for a taint they did not cause.
- **D52. A whole-node lease's clock starts at the token.** The TTL and max wall time count from the minting of the token, not from the
  commit, so a consumer is not charged for the drain or the preparation; readiness is bounded by `prepare_timeout` and `max_replace`.
  Alternative: from the commit; rejected, since a consumer could lose its time before it could use the host.
- **D53. Environment measurement and the rerun after a loss are the consumer's.** The scheduler's readiness is only that the host is quiet
  in its table and its checks are clean. Noise, CPU frequency and other tenants' traces are measured by the consumer, in a readiness hook
  the agent runs inside `preparing` before it reports ready; on loss of the host mid-run (lost, tainted, lease ended) the consumer repeats
  the measurement, and the scheduler, which never sees an action, does not try. Alternatives: the scheduler measures noise (rejected: it
  would own a number it cannot judge for every consumer); the scheduler reruns the action (rejected: impossible in two-level scheduling, 9.1).
- **D54. Readiness refusals are typed and prepare failures have their own cap.** The agent or its hook may refuse with a code from a bounded
  set (`not-quiet`, `noise-over-bound`, `setup-failed`, `host-unsafe`, `unsupported`; unknown reads as `setup-failed`) and a detail of at most
  256 bytes, and the scheduler acts at once instead of waiting for `prepare_timeout`; each code is a data row (host effect, request effect).
  A refusal or a timeout is a prepare failure and counts toward `max_replace` (default 3); drain give-ups count toward `max_reserve_attempts`
  (default 2); the two are separate because one is the cost of waiting for a host and the other is a host or setup that does not work.
  Alternatives: one shared counter (rejected: a noisy host would eat the drain budget and the reverse); only a timeout (rejected: a known
  refusal would idle a host for up to ten minutes).
- **D55. Reservations are persisted.** A reservation record (request id, host, deadline, both counters, exclusions) is written to the lease
  file in the same atomic write as the leases, under the same single-writer rules (D33); after a restart a request presented again with its
  id is bound to its record and the drain continues; an unclaimed record is dropped after `regain_grace`. Alternative: not persisted, so a
  restart drops every mark and the drains restart; rejected, since a drain costs up to a max wall time of idle host, and a restart loop
  would reset the give-up counter and defeat the bound of D48.
- **D56. A provider brings hosts that run many jobs; capacity is booked as a request vector and packed by the placement rule. Direction
  settled by the project owner; the rules are chosen here. Amends D7.** (a) One machine or one VM for each job; (b) a host sized to hold
  many jobs, each run booking its request vector (a container its CPU and memory under `Book`; a VM its CPUs, memory and one `vms` slot in
  full; a share lease the vector it names), packed by the pipeline of 6.1 with `FirstFit` in a pool that scales (6.5, 6.6); new machines
  are launched only for the hosts the queued bookings need after packing. Adopted: (b). It is the rule the earlier text already
  followed for reuse (D23) and sharing (5.15); what changes is that ucdk Job classes book their declared size under `Book` instead of a
  whole free host under `BookAll` (D7), and that the scaler's demand is hosts after packing. `BookAll` stays an admission a class may
  name. Rejected: a machine for each job (a boot for every job, K9 thrash), and a VM on its own machine (a machine boot on top of the VM
  boot of D29). Text in [hosts.md](hosts.md).
- **D57. Many fresh VMs on one host.** A VM run still gets a fresh VM, destroyed after the run and never reused (D29, unchanged). Several
  such VMs exist on one host at once, up to its `vms` slots, each booked in full (5.15), alongside containers. Options: (a) a VM per
  machine (rejected: see D56); (b) a pool of reusable VMs (rejected by D29); (c) many fresh VMs on a host. Adopted: (c).
- **D58. VM support is a provider-declared, agent-checked host capability of a launch option.** The provider declares, per launch option
  (instance type and terms), `virt` (`none`, `nested`, `bare-metal`) and the `vms` its host can run; the agent probes and reports the
  least of its flag and what the probe allows; the placement rule reads `vms`, `virt` and `lifecycle` as host facts (5.13). A declared
  option whose probe fails serves containers only, with an alert, and is not tainted. Options: (a) trust the declaration; (b) trust the
  probe alone; (c) the declaration decides what the scaler may launch for a VM request, the probe decides what is true. Recommend (c): (a)
  places VMs on hosts that cannot run them, and (b) cannot know before launch which option to buy. Facts stay unverified claims (K27).
- **D59. Spot hosts: a reclaim notice taints, drains and reschedules, with the machinery that exists.** The provider's `events()` stream
  (or, without it, the agent's `draining` phase on the cloud's shutdown signal) gives the notice. In that tick the scheduler records the
  host as tainted with the reason `reclaim` (so the cordon survives a restart, 5.12), asks consumers through a renewal flag `host_leaving`
  with `leaves_at` (additive, the sibling of `host_reserved`) to place nothing new and release, lets running work finish until the
  deadline, requeues what had not started, and at the deadline or on a read-back Absent gives up what is left and reruns it elsewhere
  against the platform-fault budget (8.1). The usual replacement acquire follows. A reclaim taint counts toward neither
  `max_taints_in_a_row` nor `max_boot_failures`. Options: (a) treat a notice as nothing and wait for the loss (orderly work lost for
  nothing); (b) a new reclaim state machine (a second path that will rot); (c) the above. Recommend (c).
- **D60. Price and capacity are provider data used for choosing, never limits.** A provider reports, per launch option, a price in its own
  unit and advisory capacity signals; the scheduler uses them as sort keys to choose among options inside a provider and among providers
  inside a tier (D61): lowest price, then best capacity signal, then id. They refuse nothing and stop no launch; there is no cost
  guardrail, spend cap, budget or dollar model in the scheduler (N4). A refused acquire cools down the option that was refused. Options:
  (a) no price data (spot choice is then the operator's static list); (b) price as a guardrail (rejected by N4); (c) as above. Recommend (c).
- **D61. Several clouds are several providers in the one ordered hierarchy; a tier groups equals.** `aws`, `gcp`, `azure` and on-prem
  sources are kinds of provider behind the one trait; clouds are values in a provider's table, never branches on a name in the core, and a
  kind may appear twice under two names. The pool's `providers` list keeps the order of D39; an entry may be a tier, an inner list of
  providers that are equals, among which D60 chooses (a flat list means what it did). A tier is passed over only when none of its members
  can satisfy now; an unreachable or refused member is passed over in the same tick; release runs the other way, so the last tier is given
  back first. Options: (a) order the whole list by price (rejected: the order is the operator's statement, and the market would reorder
  own hardware behind a cloud); (b) one pool per cloud and a consumer that chooses (rejected: every consumer would rebuild the
  fall-through); (c) the tier. Recommend (c).
- **D62. At its machine cap with a growing queue, an elastic pool launches the biggest type it can get and retires its smallest instance.
  Settled by the project owner; the details are chosen here.** The first request sizes the first instance (the smallest option that
  holds it) and more demand adds instances up to `max_machines` (5.8, 5.11). Only when the pool has been at the cap, with queued demand
  that has not eased, at every sample of one observation window (`swap_window`, default 10 minutes), the scaler picks a victim and the
  biggest option the provider can get, launches it, drains the victim once the launch is accepted (D63), then waits one window, counted
  from when the new machine is up, and repeats if the queue has not eased. No second swap starts while a victim has not read back Absent.
  Overshoot is accepted. There is no consolidation logic at the cap: the earlier draft (a drain with headroom for one extra host and open
  consolidation below the cap) is replaced. **Launch before drain is decided (owner's confirmation):** a refused launch loses no machine;
  the count may be `max_machines + 1` between the accepted launch and the victim's read-back Absent. Text in [hosts.md](hosts.md), section 11.
- **D63. Drain before terminate; an eviction charges no budget and fails nothing. Decided by the project owner.** The victim is recorded
  tainted `evict` (no new work) and its running jobs finish; it is terminated when it empties or at the end of the drain grace
  (`swap_drain_grace`, default 30 minutes, a pool key, never an environment variable), whichever is first. Only jobs still running at the
  end of the grace go through the lost-host path of 5.6 (leases given up and the consumer told, scheduler-placed runs requeued as a new
  attempt); they are never reported failed. The attempt ends `evicted` and counts against no budget: not the platform-fault budget, not
  the loss count that refuses a poison operation, not a task failure. Options: (a) terminate at once (rejected by the owner: builds must
  not just fail); (b) charge it as a platform fault or a loss (rejected: it is the scheduler's choice); (c) drain, then requeue the rest,
  charging nothing (adopted), with `max_evictions` (**decided: 3**) per request, counting only jobs still running at the end of the grace (those requeued as `evicted`), kept as an eviction
  record per request id written in the same atomic write as the lease change and deleted when the request ends or no replacement is asked
  for within `regain_grace`; after the cap its host is passed over as a victim, so one request cannot be hit for ever. A terminate that reads back Unknown frees nothing (5.6).
- **D64. The victim, what is never the victim, and the catalog.** The victim is the smallest host whose running jobs are expected to finish
  soonest: hosts whose expected drain (the end bound of whole-node.md section 3, or a better predicted remaining time where a consumer has
  one) is within the grace tie and the smallest of them wins; others rank after, by expected drain; a job with no prediction counts as its
  max wall time. Never chosen: a registered `default` host, a host with a whole-node lease or a
  reservation (the next candidate is taken), a host with a `reclaim` notice, a host holding a request evicted `max_evictions` times, and a
  host whose removal would push the whole-node ceiling (counted by hosts) below reserved plus held. The biggest option is found by walking
  the provider's catalog downward and skipping a type with no capacity, spot or on-demand, or one cooling down (provider data, D60); a
  refused launch uses the existing per-option `cooldown`; the pool stays as it is if the biggest obtainable type is not larger than the
  victim. The lifecycle never changes by itself (the operator's order in the hierarchy, D61).
- **D65. Scale down has one signal: the queue has drained and utilization is low.** At every sample of `scale_down_window` (default 60
  minutes) the queue is empty and the booked share (the tighter of CPU and memory) is below `scale_down_util` (default 30 percent); then one
  elastic machine per window, the least booked, is marked tainted `retire` and released when empty. Nothing is killed; the mark is cleared
  if the queue becomes non-empty. Hysteresis is the longer window and the utilization floor against the 10-minute window and the growing
  queue of D62; all five keys (`swap_window`, `swap_drain_grace`, `scale_down_window`, `scale_down_util`, `max_evictions`) are pool keys, absent means the pool
  never swaps or scales down by this rule (R3). The judgement is on a per-minute pool sample, which the design did not have; it is in
  memory and a restart restarts the windows (swaps come later, never sooner).
