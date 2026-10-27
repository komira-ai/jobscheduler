# Deployment: one service in Kubernetes (5.16)

Companion to [docs/design.md](../design.md), sections 5.4, 5.8, 5.9 and 9.1. Section numbers refer to design.md; 5.16 is defined here. The
decisions are D33 and D34 and the risks K28 to K31 ([risks-f.md](risks-f.md)). What it rests on in the merged code (read from `js-lease`, `js-lease-server` and
`js-agent` on `main`) is marked as merged; everything else is design.

## Target shape

jobscheduler runs as **one service** in a Kubernetes cluster: one process, one workload, the scheduler and both of its APIs.

```
  kbf frontend ──(lease API, TLS)──┐                       ┌── static / aws capacity providers
  js-cli       ──(lease API, TLS)──┤                       │   (standing access of the service, K7)
                                   ▼                       │
                           ┌───────────────────────────────┴───┐        ┌──────────────────────────────┐
                           │ jobscheduler service (one pod)    │──────▶ │ volume: lease file, binding  │
                           │ lease table, reaper, pool scaler, │        │ file, lock file (one writer) │
                           │ registry, agent API               │        └──────────────────────────────┘
                           └───────────────────────────────────┘
                                   ▲
  js-agent on every machine ───────┘  (agent API, TLS; agents dial out and register, 5.8)
  (generic half of the kbf daemons; the Mac Studio machines of the GPU pool, Linux hosts, VM hosts)
```

- **Clients.** The kbf frontend is a client of the service. Its frontend asks for capacity (the lease API, 9.1) and places work inside what
  it is granted; it makes no scheduler call per action. `js-cli` is another client. Neither runs inside the service.
- **Agents.** The generic half of every kbf daemon is `js-agent`, on every machine that runs work. It registers with the service (5.8) with
  the join token of its pool and then heartbeats. Agents only dial out (D22), so a machine outside the cluster needs only a route to the
  service's address.
- **One process.** The service is the merged `js-lease-server`, which already holds the lease table, the reaper tick, the registry of
  agents and both APIs (`/v1/leases...`, `/v1/agent/hello`, `/v1/agent/heartbeat`) behind one lock. The capacity providers, the pool scaler
  and the host registry of 5.7 and 5.8 join the same process as they are built. Nothing about the target needs a second binary.
- **The stepping stone is the same binary.** Today `js-lease-server` runs on a single host with its lease file on local disk. Kubernetes
  changes where the process, the files and the secrets live, and nothing in the flags, the files or the wire. Moving is a copy of two
  files (below), not a migration of formats.

## The Kubernetes objects

All values are placeholders for the operator; no object names a host, an address or an account.

| Object | Choice | Why |
|---|---|---|
| `Deployment` | `replicas: 1`, `strategy: Recreate` | one writer (state store, below); a rolling update would start a second pod beside the first |
| `PersistentVolumeClaim` | `ReadWriteOncePod` where the cluster has it, else `ReadWriteOnce`; a block-backed volume; reclaim policy `Retain`; mounted at the directory of `--state-file` | the volume is the state; `Retain` so that deleting the claim by mistake does not delete the leases |
| `Service` (cluster) | `ClusterIP`, port 7300 | in-cluster clients: the kbf frontend, `js-cli` jobs |
| `Service` (external) | `LoadBalancer`, or the cluster's own L4 entry, passing TCP through | agents outside the cluster; **TLS is not terminated before the pod**, so a bearer token and a join token never cross in clear and the agent pins the server's certificate (5.8) |
| TLS `Secret` | certificate and key, mounted as files; its subject names the address agents and clients use | `--tls-cert`, `--tls-key`; a change is a restart |
| token `Secret` | the bearer token and the join tokens, mounted as files | `--token-file`, `--agent-token-file`; never in `env`, never in an argument (R4) |
| `ConfigMap` | the pool file and the agent-pool file, mounted as files | `--pool-config`, `--agent-pool-config`; no reload in the first slice (D20), so a change is a restart |
| `PodDisruptionBudget` | `minAvailable: 1` | see below |

**The pod.** One container from an image built from the release's static binary (`docs/release.md`), pinned by digest, with every flag in
`args` (R4) and nothing in `env`:

```yaml
args: ["--listen", "<pod-address>:7300", "--boot", "resume",
       "--state-file", "/var/lib/jobscheduler/leases.toml",
       "--token-file", "/run/secrets/js/token",
       "--agent-token-file", "/run/secrets/js/join-tokens",
       "--agent-pool-config", "/etc/jobscheduler/agent-pool.toml",
       "--pool-config", "/etc/jobscheduler/pool.toml",
       "--tls-cert", "/run/secrets/js/tls.crt", "--tls-key", "/run/secrets/js/tls.key"]
```

- **`--boot resume`, always.** The first deployment of an empty volume starts once with `--boot initialise`, by an operator and not by
  the manifest; every later start is `resume`, which refuses a missing file (merged). A volume that came back empty therefore stops the
  pod instead of starting a table at term 1 that would reissue old lease ids.
- **Secrets as files.** The merged server reads a secret only from a regular file owned by its own user, mode `0400` or `0600`
  (it follows the symbolic links Secret volumes use), and reads the join-token file again at every tick. A Secret volume's files are owned
  by root, so the container's user is root with `capabilities: drop: [ALL]`, `allowPrivilegeEscalation: false`, a read-only root
  filesystem, the runtime default seccomp profile and, where the cluster has user namespaces, `hostUsers: false`, so root in the container
  is unprivileged on the node. Secret volumes use `defaultMode: 0400`. The alternative, copying the secrets to a memory volume owned by a
  non-root user, needs a shell or a helper in the image, and would stop the live reading of the join-token file: a join token is then
  rotated by overlap (the file may hold several tokens) with no restart. The service account token is not mounted
  (`automountServiceAccountToken: false`): the service never calls the Kubernetes API.
- **Probes.** Liveness is the merged `/healthz`: open, `200` while the tick loop runs, `503` once it has stopped (a server that no longer
  ticks expires nothing, so the pod should be restarted). Readiness is `/readyz` (step 48): ready only when the files are loaded under
  the lock and the tick has run, and not ready while draining. A startup probe allows a long load. All probes are `HTTPS`.
- **Resources.** Requests of 250m CPU and 256 MiB of memory, a memory limit equal to the request and no CPU limit (a CPU limit throttles
  the tick). These are starting values, not measurements: the table is held in memory and written whole, so the memory follows the lease
  count, and the first deployment records the real numbers.
- **Shutdown.** `terminationGracePeriodSeconds` exceeds `--drain-timeout` (merged, default 10 s) by the length of a tick. On `SIGTERM` the
  server stops accepting, answers the requests in flight and exits; the state is durable before every answer, so nothing waits to be
  flushed.
- **PodDisruptionBudget.** With one replica, any voluntary eviction is an outage. `minAvailable: 1` makes an automated node drain (a node
  upgrade, a scale-down) wait for a person, who deletes the pod at a moment of their choosing, not one in the middle of a whole-node
  lease preparing (5.15). The price is a manual step on every drain, which is the point; leases are durable, so the deletion is safe.
- **Network.** A `NetworkPolicy` admits ingress only on the one port. Egress is to the capacity providers' endpoints once there are any.

## The state store

Today the whole state of the service is two files on local disk, beside each other: the **lease file** (`--state-file`, one TOML document
holding the term, the next sequence and every lease) and the **binding file** (`<state file>.agents`, which credential and instance each
registered host id was admitted with), plus a lock file. The crash-safety rules of `js-lease` are merged and must hold unchanged:

1. **Atomic replace.** A change is written to a temporary file next to the target, fsynced, renamed over it, and the directory is
   fsynced, so a reader sees the old file or the new one and a crash loses neither.
2. **Commit before act.** A lease is in the file before the provider is told to start it; an `acquire` answered Unknown leaves the lease
   pending for the tick to resolve.
3. **One writer.** The table takes an exclusive lock on a sibling file for as long as it lives; a second process over the same file fails
   at the lock instead of issuing the same ids.
4. **Fail closed.** `--boot resume` requires the file; a file whose `lease_count` disagrees with its tables (a partial restore) is refused;
   every boot takes a new term, so ids from before a boot are never reissued.
5. **Observe once at boot.** A lease stored as held is observed once: present stays held, absent is dropped, unknown is kept (the
   three-answer rule, 5.6).
6. **Forward and backward readable.** A reader ignores fields it does not know and keeps them on write (R3).

**Options for Kubernetes.** (a) *Single writer on a PersistentVolume*: the same two files on a block volume. (b) *An external store*: the
lease table and the bindings in a database, with conditional writes (the `js-state` trait of D13).

**Choice: (a), D33.** Reasons:

- It is the merged persistence, with the six rules above already tested. (b) rewrites the part of `js-lease` that every one of those rules
  lives in, and each rule would need proving again against a database.
- One writer is already the design: the table is one lock and ids come from one term and one sequence. An external store would not by
  itself make two schedulers safe; two writers also need fencing and an election, which is a design of its own (below).
- The table is small. It holds machine leases, not requests, so it is hundreds or thousands of rows, and a whole-file rewrite per change
  is cheap at that size. The limit is real and is named (K29): when a rewrite takes a noticeable share of the tick, (b) is the answer.
- It adds nothing to operate: no database, no credentials for one, no second thing whose outage is the service's outage.

**What it means.**

- **The volume must be a block volume with a real local filesystem** (ext4 or xfs behind the volume driver). A network filesystem is
  refused, because the rename, fsync and lock rules above are not safe to assume there. The lease file, the binding file and the lock file
  stay in one directory of one volume, so that the rename is atomic and the two files age together.
- **Restart.** A restart reads both files and takes a new term; nothing is rebuilt from elsewhere. Soft state is rebuilt by the agents:
  the facts of each host (5.13) come back at the next hello.
- **Loss of the volume** is a loss of the leases. With `--boot resume` the pod refuses to start; an operator then restores a snapshot, or
  deliberately runs `--boot initialise`. After either, an agent that announces a lease the table does not hold is answered `Cancel` (the
  per-session rule of 5.4) and a client's renew of an unknown lease fails, so the client leases again and the work reruns under the
  platform-fault budget (8.1). Snapshots of the volume are taken on a schedule; a snapshot holds the old file or the new one for each
  file, and `--verify-state` (step 50) checks the pair against each other before a restore is trusted.
- **The binding file is part of the state.** Without it the first hello for a host id after a restart would take the host's lease from
  anyone holding a join token (K10). The two files are always copied, restored and verified together.

**(b) later.** An external store is the answer when one of these is true: the service must survive the loss of a node or a zone without a
minutes-long outage; the table is large enough that the rewrite shows (K29); or a second replica is wanted. It needs the `js-state` trait
and a database backend (D13), a leader for the tick, the reaper and the scaler (below), and a fence so that a paused old leader's writes
fail. The lease file stays as the export format.

## High availability: what happens, and what is not covered

The stance: **durable, not highly available.** One pod holds the state and acts on it; its loss is a bounded outage and never a loss of
leases.

| Event | What happens | Recovery |
|---|---|---|
| The process exits or is killed (a crash, a failed liveness probe, an out-of-memory kill) | The API is down. Every lease is in the file; nothing was acknowledged that is not. | The kubelet restarts the container on the same node and volume. It locks, loads, takes a new term, observes the held leases once, writes, and becomes ready. |
| The pod is deleted or evicted | As above, and the replacement may land on another node | `Recreate` stops the old pod before the new one starts; the volume follows. |
| The node is lost | The pod is gone with the node; the volume is still attached to a node the cluster cannot reach, so it will not attach to a second one | The cluster's attach controller detaches it after its timeout, then the new pod mounts it: an outage of the detection time plus the detach, in minutes. An operator never force-detaches a volume while the old node might be alive; the node is fenced first. A second writer cannot exist meanwhile: the volume access mode and the lock refuse it. |
| The zone or the volume is lost | The service stays down; a zonal volume cannot follow the pod elsewhere | Restore a snapshot (loss bounded by its age, above). This is the one case with data loss. |
| An agent's connection drops (its own or the service's) | The agent's session ends | The agent re-hellos with its backoff (merged); the server's per-token hello limit (`--agent-hello-burst`, `--agent-hello-interval`) smooths a herd after a restart. The binding file keeps each id bound to its credential and instance, and the heartbeat reconciles its leases by the graded absence rule: silence is not absence. |

**Leases, TTLs and the reaper across an outage.** A lease holds its expiry as an absolute time. While the service is down, nothing
renews and nothing expires, because only the tick expires leases (K13). After a boot, the first tick would expire every lease whose
expiry passed during the outage, including leases of clients that could not renew. Options (D34): (a) as merged, expire at the first tick;
(b) no held lease expires before a boot grace (`--boot-grace`, off by default) has passed after a boot, so clients get a window to renew.
Recommend (b), with the grace at the default TTL, set by the operator. It touches only expiry: an `absent` observation still drops a
lease at once, an `unknown` one is still kept, and the grace opens on the table's injected clock. The reaper and the pool scaler run inside
the tick, so while the service is down no machine is released or launched; a restart resolves the open acquire intents by `list` (K8).
An outage is tolerated up to the smaller of the lease TTL and the agents' fence: a self-fencing agent stops its work after its fixed time
without contact, and a run-on one continues. Work already placed inside a lease keeps running: the consumer places work itself (9.1) and
the scheduler is not in the data path (D22).

**Single active writer, not leader election.** The one writer is made so by three independent things: one replica under `Recreate`; a
volume that only one pod can use; and the lock on the lease file. A Kubernetes coordination lease is not used: with one replica there is
nothing to elect, and such a lease is not a fence anyway, since a paused leader can still write. If (b) comes, the fence is the store's
conditional write on the term (D11), with an election to choose who ticks.

**What is not HA, and why that is acceptable now.** The service is down for the length of a pod start after a crash and for minutes after
a node loss; new leases, renewals and scaling wait, placed work does not stop, and nothing is lost except in a lost volume. The first
users, kbf and the ucdk realization, ask for capacity at the pace of builds and jobs, retry, and hold leases for many minutes, so a bounded
outage costs a delay and not a failure. The price of more (an external store, an election, a fence) is larger than the cost of that
delay today (K28 names the risk).

## Upgrades

- **Servers before agents** (10.3): the service is upgraded first, then the agents on their machines by whatever updates them; a new
  agent opens a v1 session first and moves only when the server advertises the new package.
- **Releases are automatic; deployment is not.** Every merge to `main` that ships code becomes a release (`docs/release.md`), and the
  cluster does not follow it. Deploying a version is an operator act with the explicit go of R7, and the Deployment names the image by digest.
- **`Recreate`, not a rolling update.** With one writer the new pod cannot start before the old one has released the volume and the lock,
  so a rolling update with a surge would only produce a pod that fails at the lock and never becomes ready. The outage of an upgrade is
  the stop of the old pod plus the start of the new one. A change of configuration or of a certificate is the same restart.
- **Rollback is a redeploy of the previous image on the same volume.** The files are readable forwards and backwards (rule 6); a test
  pins that the new server reads a file written before and that an older server skips the new keys (13.6).

## Moving from the single host

Today's `js-lease-server` on one host is the stepping stone. The move needs no new format and is bounded by the smaller of the lease TTL
and the agents' fence (above). Each step is reversible until the last.

1. Prepare the objects with the pod at zero replicas: the volume, the Secrets (the **same** bearer token and join tokens, so no client
   or agent is reissued a credential), the ConfigMap from the existing pool files, and a certificate whose subject names the address that
   agents and clients already use.
2. Stop the old server with `SIGTERM` and **disable** it, so no unit can restart it: two servers over two copies of the state are two
   tables.
3. Copy the lease file and the binding file to the new volume (a one-off pod that mounts the claim), then run `--verify-state` on the copy.
4. Start the pod with `--boot resume`. It takes the next term and observes each held lease once.
5. Move the name: the address agents and clients use now points at the service's external entry. Agents re-hello on their own; a client
   retries.
6. Check that every agent is registered, that the leases held before are held, and that no `Cancel` was sent for a lease the old table
   held. Only then remove the old files from the old host; until then they are the way back (stop the pod, point the name back, start the
   old unit; leases made since are lost).

## Tests (13.1, 13.3)

The state rules are the existing `js-lease` tests, unchanged. New: the readiness and lock cases of step 48; the boot grace of step 49 on
the injected clock; the verify mode of step 50; the manifest test of step 51; the client cases of step 52; and the exercise of step 53,
which is not run on shared CI. No test asserts elapsed time (R2): outages and graces are counted on the injected clock.
