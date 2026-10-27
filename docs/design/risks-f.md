# Risks K19 to K34

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
- **K24. A whole-node lease never becomes ready, is shared by mistake, is starved, or idles the pool.** Mitigated by the withheld token, `prepare_timeout` with a retry cap and back-off, the offer invariant, and a drain rule with a drain deadline and at most one draining host per pool (5.15, vm-backend.md).
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
  submission during a restart of the only capable host is refused. If the owner chooses the other reading of D35, this risk becomes the
  opposite one: a busy fleet refuses work.
- **K33. A booting machine is kept after the demand that launched it went.** A host freed while a later entry's machine boots (D40) leaves a
  machine nobody asked for. The waste is bounded to one boot plus one `idle_timeout`, and `cooldown` stops a loop of launching. Mitigated by
  the idle rule, `max_machines` and the shares in `provider_max_machines`.
- **K34. The test provider reaches production.** A `static` provider lists hosts by hand and holds no join credential. Mitigated by the
  `--allow-test-providers` flag, which a production deployment does not set, and by a layering test that no production example names the
  kind; the merged `--pool-config` path is the stepping stone until step 54.
