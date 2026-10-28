# Risks K19 to K48

Moved from design.md section 16 to keep that file under its size limit; K1 to K18 stay there. Section numbers refer to design.md.

- **K19. A wrong constraint places work on a host that cannot run it, or on none.** Skipping an unknown op, reading an absent fact as zero,
  or matching images one constraint at a time would. Mitigated by refusing unknown ops, absent-is-unmet, one-image guest scope, and the constraint suite (5.13).
- **K20. Stale or false facts.** An agent that reports a runtime it lost, or a snapshot lost in transit, misleads placement. Mitigated by full
  snapshots with an ack, the agent re-probing after each job, and the agent refusing a `Start` it cannot honour (`image-unavailable`); a fact never reaches a lease already granted. A server from before this change would skip `needs` and place unconstrained: the consumer reads the server's `features` and refuses to send `needs` or `runtime = vm` without `needs` support (10.3, capabilities.md).
- **K21. A guest attacks the host through the channel or the hypervisor.** Mitigated by a bounded, fuzzed frame codec, no shared writable host
  path, no network by default, no credential in a guest, host checks after every VM job, and taint on a failed destroy (5.14). A hypervisor escape is not
  covered by any check here.
- **K22. VMs starve containers on the host.** A booted VM holds its CPUs and memory in full for its run. Mitigated by counting it in full, the offer rule, the `vms` ceiling and containers admitted against the total less VM memory (5.15).
- **K23. Base image drift.** A mutable image would make two leases differ. Mitigated by immutable names, a digest pinned in the `Start` and verified (5.14).
- **K24. A whole-node lease never becomes ready, is shared by mistake, is starved, or idles the pool.** Mitigated by the withheld token, `prepare_timeout` with a retry cap and back-off, the offer invariant, and the reservation, drain bound and caps of the whole-node class (5.17, [whole-node.md](whole-node.md); K38).
- **K25. Boot cost per run.** A boot for every run, since no VM is reused. Bounded by the size choice of D26; a warm pool is future work.
- **K26. Wire growth.** Facts add bytes to every hello; new limits bound them and an agent over them is refused as any other (13.6).
- **K27. Advertised facts are unverified claims.** A registered agent can claim any fact, `sandbox=vm` isolation included. Bounded: facts never override pool selection by class and labels, the lease token still binds one consumer, and a lie costs queueing, misplacement or refusal and the agent its taint and deny-file path; a consumer needing isolation leases from a pool whose operator it trusts (Trust, capabilities.md).
- **K28. One pod is one point of outage.** A crash, a pod deletion or a node loss stops new leases, renewals and scaling until the pod is back:
  seconds for a restart, minutes for a node loss (the volume must detach first), and a restore from a snapshot for a lost zone. Mitigated by
  durable state written before every answer, agents that re-hello, the boot grace (D34), placed work that does not depend on the service
  (D22), and a disruption budget that keeps automated drains from restarting it unseen. Not mitigated: the outage itself; an external
  store and an election are the way out (D33, 5.16).
- **K29. The lease file is rewritten whole on every change.** The cost grows with the table. Acceptable while the table holds machine leases
  (hundreds to thousands of rows); the first deployment records the rewrite time as a number the operator watches, and an external store
  replaces the file when it is a noticeable share of the tick (D33).
- **K30. The volume is the state.** A network filesystem breaks the rename, fsync and lock rules; a deleted claim deletes the leases; a
  snapshot may hold the lease file and the binding file from different moments. Mitigated by a block volume of a local filesystem, reclaim
  policy `Retain`, `--boot resume` (a missing file stops the pod), scheduled snapshots and a verify mode that checks the two files against
  each other before a restore is trusted (5.16, step 50).
- **K31. The container runs as root to read root-owned secrets.** The merged server reads a secret only from a file owned by its own user,
  and a Secret volume's files are owned by root. Mitigated by dropping every capability, no privilege escalation, a read-only root
  filesystem, a runtime-default seccomp profile, user namespaces where the cluster has them, and no service account token. A change to
  the server that accepts a root-owned file on a read-only mount would remove the exposure; it is not proposed here.
- **K32. A refusal turns a flapping host into an error.** `no-capable-host` (D35) is final for the caller. A registered host that restarts
  or is briefly tainted could, with no grace, refuse requests that a moment later would have served. Mitigated by counting lost and busy
  hosts as capable, `regain_grace` for requests already queued (D41), and a plain constant code so a caller can retry. Residual: a new
  submission during a restart of the only capable host is refused.
- **K33. A booting machine is kept after the demand that launched it went.** A host freed while a later entry's machine boots (D40) leaves a
  machine nobody asked for. The waste is bounded to one boot plus one `idle_timeout`, and `cooldown` stops a loop of launching. Mitigated by
  the idle rule, `max_machines` and the shares in `provider_max_machines`.
- **K34. The test provider reaches production.** A `static` provider lists hosts by hand and holds no join credential. Mitigated by the
  `--allow-test-providers` flag, which a production deployment does not set, and by a layering test that no production example names the
  kind; the merged `--pool-config` path is the stepping stone until step 54.
- **K35. Agent fixes wait behind the kbf package.** With `js-agent` shipped in the kbf host package (D42), an urgent agent fix reaches
  hosts only at the pace of a kbf host release and the coordinator's slots and gates, and a jobscheduler release faster than kbf's is held
  back by kbf's. Accepted while every agent runs kbf. Mitigated by the server refusing an agent major it does not speak (D44), so a host
  left behind fails closed and visibly; the triggers that end the coupling are in D45 and [rollout.md](rollout.md).
- **K36. The host updater has one signer.** The kbf helper verifies a package against kbf's release signer only. A jobscheduler release
  signed by another key is not installable by it, and widening the trust (a second signer, or one shared signer) widens who can put code
  on every host. Not solved by this design: it is a security decision for the owner before any option that ships jobscheduler packages
  through the helper (D45). Until then the agent ships only inside the kbf package.
- **K37. A host job burns a registered host.** A whole-node lease that allows host jobs taints its host at the grant (D51); a registered
  host stays tainted until an operator reimages it, so a dedicated GPU or performance pool of few machines loses capacity for each such
  lease. Mitigated by host jobs off by default and needing four consents, by GPU and performance runs being `contained`, and by an elastic
  machine being replaced. Not mitigated: a pool that allows host jobs on its only machines.
- **K38. Draining idles capacity, and a long lease holds a reservation.** A reserved host does no new work until it is quiet, up to its
  `drain_deadline`, and a consumer that ignores `host_reserved` keeps its lease to its TTL or max wall time (D48). Bounded by `max_draining`,
  the ceiling and `keep_for_ordinary` (D49), by cost-ordered host choice, by the give-up and back-off, and by acquiring a machine in an
  elastic pool before draining (D50). Residual: a pool of long leases pays most of a max wall time of idle per whole-node lease.
- **K39. Quiet is the agent's claim.** The readiness report is the agent's own (K17, K27): a wrong or hostile agent can say it is quiet while
  something runs, and a performance result taken beside it is wrong. The scheduler's own table (no lease, no placed run) and the
  withheld token cover the work it knows of; a lie costs the agent its taint and deny-file path, and a consumer needing a trustworthy
  measurement leases from a pool whose operator it trusts (whole-node.md).
- **K40. A refusing hook or agent starves a request.** A readiness hook that always refuses, or an agent that refuses falsely, can cycle a
  request through hosts. Bounded: each refusal excludes the host for the request, counts toward `max_replace` (D54), and the request then
  fails `exclusive-not-ready` with the last code and detail, so the consumer sees why. Residual: a hostile agent can waste up to `max_replace`
  drains' worth of other hosts' idle time; it is already able to stall by silence (K39), and the taint path applies to `host-unsafe`.
- **K41. A persisted reservation outlives its use.** A record kept across a restart can hold a host for a consumer that has gone. Bounded by
  `regain_grace` after boot for an unclaimed record, by the drain deadline for a claimed one, and by deletion with the request (D55). Residual:
  a restore from an old snapshot reads old marks; a record naming a missing host or request is dropped at boot.
- **K42. A spot market that flaps churns hosts.** Interruptions taint and replace hosts in a loop, and a replacement may be interrupted
  too. A reclaim taint is excluded from `max_taints_in_a_row` and `max_boot_failures` (D59), so those brakes still mean a bad job class or
  a bad image. The loop is bounded by `max_step`, `cooldown`, `max_machines` and the provider shares, and steered by the capacity signal
  of D60 (a recently interrupted option sorts later) and by a tier that holds another cloud or an on-demand provider. Residual: a pool
  whose every option is interrupted pays boots and reruns until the market settles; the platform-fault budget bounds the reruns (8.1).
- **K43. A shared host is a larger blast radius.** One host loss, reclaim, taint or kernel fault ends many jobs of many consumers at once,
  where a machine per job would lose one. Mitigated by the notice and drain of D59, the platform-fault budget for reruns, taint on any
  finding, and the choice of a host size (the shape is a floor; an operator sizes hosts for the loss they accept). Not mitigated: an
  unannounced loss of a full host.
- **K44. A declared VM capability is wrong.** A launch option that says `virt = nested` and cannot run a VM would waste a boot per
  attempt. Mitigated by the agent's probe (D58: the host reports zero slots and an alert), by the failure being recorded per option so the
  option cools down, and by a conformance case with a lying option. Residual: the first launch of a wrong option is wasted.
- **K45. Stale or wrong price and capacity data choose badly.** Data that is old, or wrong by mistake or by a hostile provider crate,
  picks an expensive or unavailable option. It cannot refuse a request or exceed a cap (D60); the cost is a worse choice or a refused
  acquire that cools the option down. The data is read once per tick on the injected clock, so a bad choice is reproducible in a test.
- **K46. A tier member that launches and never registers.** A cloud whose machines boot but cannot reach the scheduler would take the
  demand of its tier every time it sorted first. Mitigated by the boot timeout and the failure brake of its entry (5.8, providers.md), so after
  `max_boot_failures` in a row that member is passed over (a brake, not a cost limit), and by the alert it raises.
- **K47. A swap throws away work and overshoots.** The victim's running jobs lose their progress and are rerun (D63), and the biggest type
  may be more than the load needed (D62). Accepted by the owner: jobs are retryable, and the swap acts only at the cap with a queue that has
  not eased for a window. Bounded by one swap per window, by the protected hosts (D64), by `max_evictions` per request, and by launching
  before killing (a refused launch loses nothing). Residual: a pool whose queue is long because jobs are long loses their progress, and a
  lasting overshoot is undone only by the scale-down of D65.
- **K48. The cap is crossed by one machine during a swap.** The swap's launch is counted with the victim until the victim reads back Absent,
  so the count is `max_machines + 1` for that time (D62). Bounded by one swap per window (H10); a terminate that reads back Unknown holds
  the extra machine until it is resolved (5.6), and the reaper's release retry applies (12.1).
