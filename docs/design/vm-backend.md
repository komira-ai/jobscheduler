# VM runtime backend, sharing and caps, the whole-node class (5.14, 5.15)

Companion to [docs/design.md](../design.md), sections 5.4, 5.10 to 5.12 and 6.2. Section numbers refer to design.md; 5.14 and 5.15 are
defined here. Capabilities and constraints (what picks a host and an image) are in [capabilities.md](capabilities.md).

## 5.14 The VM backend of `js-agent`

`js-agent` runs a job in a container (the merged `ContainerRuntime`) or in a virtual machine. Two rules govern the VM path and are not
options:

1. **A fresh VM per lease. A VM is never reused.** Every VM that runs a job is created from a base image for that job and destroyed when
   the job ends, whatever its outcome. A VM that has run a job is never returned to a pool, never handed a second job, never "cleaned".
2. **The machine is reused; the VM is not.** The host serves many VM leases (and container jobs) in a row under the rules of 5.10; what is
   fresh each time is the guest.

### Where it fits: one lifecycle, two backend traits

The merged lifecycle (`js-agent::lifecycle`) is a state machine over `ContainerRuntime`, `CheckHook` and `Clock`. `ContainerRuntime`
stays as merged (and as the docker and podman backend in review implements it). The VM path gets a **second trait, `VmRuntime`**, and the
lifecycle reaches both through a small internal seam, `Sandbox`: `start`, `poll`, `stop`, `destroy`, `adopt`. A `ContainerSandbox`
wraps a `ContainerRuntime`; a `VmSandbox` wraps a `VmRuntime`. The job's `runtime` field picks one.

Reasons for two traits and not one widened trait: a VM starts in stages with their own failures (clone, boot, guest ready) and a channel
to talk through, where a container has `create_and_start`; its destroy deletes a disk clone, where a container's `remove` deletes a
container; it has no label-based re-adoption (below), no `pids` limit and no host mounts; and a trait that holds both would be a union of
optional parts that every backend and every fake must half-implement. Changing the merged trait would also change code that is merged
and code in review for no gain. The cost, a second fake and a second conformance suite, is paid once (13.2). The lifecycle's order of
steps, result handling, idempotence on the run id, the wall-time clock and the slot accounting are written once, in the lifecycle.

```rust
trait VmRuntime {                      // blocking calls on the lifecycle's worker thread, as ContainerRuntime (its threading contract)
    fn images(&mut self) -> Result<Vec<BaseImage>, RuntimeError>;               // name, digest, profile (capabilities.md)
    fn clone_and_boot(&mut self, spec: &VmSpec) -> Result<VmId, RuntimeError>;  // returns when the VM process runs, not when the guest is ready
    fn poll(&mut self, id: &VmId) -> Result<VmStatus, RuntimeError>;            // Booting | Up | Exited | Gone
    fn open_channel(&mut self, id: &VmId) -> Result<Box<dyn GuestChannel>, RuntimeError>;
    fn stop(&mut self, id: &VmId, grace: Duration) -> Result<(), RuntimeError>; // power off; kill after grace; gone succeeds
    fn destroy(&mut self, id: &VmId) -> Result<(), RuntimeError>;               // delete the clone; gone succeeds
    fn list_clones(&mut self) -> Result<Vec<Found>, RuntimeError>;              // every clone in the store, for the start-up sweep
}
trait GuestChannel {                   // one stream per VM; frames below
    fn send(&mut self, frame: &HostFrame) -> Result<(), ChannelError>;
    fn try_recv(&mut self) -> Result<Option<GuestFrame>, ChannelError>;         // never blocks, never sleeps (R2)
}
```

`VmSpec` is: run id, base image name, whole CPUs, memory bytes, disk bytes, network (`none` or `nat`), the read-only shares if the
operator allowed any, labels written into the clone's metadata file. The first real backend drives an operator-installed **driver
program** by an absolute path, with its own environment, as `--container-bin` does for containers; on macOS the driver is built on the
platform's virtualization framework, and a Linux driver can follow behind the same trait. The driver's contract is the seven operations
above, nothing else. A fake `VmRuntime` (13.1) is the executable spec.

**Agent flags** (R4; none is read from the environment; the backend is off unless `--vms` is above zero):

| Flag | Meaning |
|---|---|
| `--vms <n>` | the most VMs at once, warm ones included; the `vms` fact |
| `--vm-driver <path>`, `--vm-store <dir>` | the driver program (absolute path, no `PATH` lookup, its own environment) and the directory holding base images and clones |
| `--vm-network none\|nat` | the guest's network; `none` by default |
| `--vm-boot-timeout`, `--vm-log-cap`, `--vm-transfer-cap` | bounds on boot, on spooled log bytes and on archives over the channel |
| `--vm-warm <image>=<n>`, `--vm-warm-max-age`, `--vm-warm-max-failures` | the warm pool (below) |
| `--vm-allow-shares`, `--vm-allow-writable-shares` | read-only virtual shares, and writable ones (both off) |

### `JobSpec`: additive fields (R3)

`runtime` (`container` by default; `vm`), and for `vm` a `vm` object: `base_image` (a name in the host's `base_images`), `outputs`
(guest paths to collect, at most 64) and the guest user to run as. Sizes are the `limits` the message already has: `cpu_millis` is rounded
up to whole CPUs and `memory_bytes` is the VM's configured memory; both are fixed when the VM is created (D26 says who chooses them). `gpu_indices` must be empty for a VM
(`invalid-job`). `isolation = host` with `runtime = vm` is refused `invalid-job`: the VM is the sandbox, a job may do anything inside it,
and `contained` therefore means "confined to its VM". The old agent of the previous release skips both fields, so a server must not send
`runtime = vm` to an agent that did not advertise `vms` (it would run the job in a container image the name does not denote); the
capability `vms >= 1` is the gate, and an agent refuses a `vm` job when its VM backend is off, with `failed_to_start` and `vm-unavailable`.

New `failed_to_start` details, plain constants beside the merged ones: `vm-unavailable`, `image-unavailable` (not in the store, or its
manifest digest differs from the digest the job pinned), `vm-clone-failed`, `vm-boot-timeout`, `vm-guest-protocol`.

### Lifecycle of one VM run (cold start)

| Step | What happens | Held state |
|---|---|---|
| 1. admit | the `Start` is checked as for a container (`valid_for_ms`, the sizes against what is free, mounts resolved under the agent's roots); a VM counts against the `vms` counter | |
| 2. clone | the base image is cloned (copy-on-write where the filesystem offers it) into the store under the run's name; a metadata file records run id, lease id and booking | `starting` |
| 3. boot | the driver starts the VM with the fixed CPU count and memory; no shared directory unless allowed; network `none` unless the operator set `nat` | `starting` |
| 4. guest ready | the agent opens the channel and waits for `Ready`; past `--vm-boot-timeout` it goes to destroy with `vm-boot-timeout` | `starting` |
| 5. inputs | read-only mounts are copied in over the channel (below) | `starting` |
| 6. run | `Exec`; the wall-time limit is counted on the agent's clock from the moment `Exec` is sent, not from boot | `running` |
| 7. collect | on `Exited` the agent pulls the declared outputs and read-write mount contents out, bounded | `running` |
| 8. result | `Result` is built and sent now, before any cleanup, and repeated until acknowledged, as for a container (5.4) | `exited_unacked` |
| 9. destroy | stop the VM (power off, kill after the grace), then delete the clone; the clone's metadata file goes with it | `exited_unacked` |
| 10. scratch | the run's scratch directory is removed | |
| 11. checks | the post-job checks of 5.12, including the VM store check below | |
| 12. free | only now the slot is freed: the run leaves the heartbeat, the booking returns, a capacity change is emitted | |

The merged `HeldState` has `starting`, `running` and `exited_unacked`, and nothing is added. **Cleanup order** is: the VM (stop, destroy),
the scratch directory, the checks, the slot; a step that fails stops the order there, the slot stays booked, a finding is recorded and the
step is tried again at the next lifecycle step, exactly as the merged cleanup order does. A VM that cannot be destroyed taints the host
(5.12: cleanup failure). The outcome mapping: guest exit status gives `succeeded` or `failed`; the agent's stop gives `killed`; the wall
limit gives `timed_out`; a guest that reports its own memory exhaustion gives `out_of_memory`; a VM that vanishes (`Gone`) before `Exited`
is `failed`, and the lease is rerun elsewhere under the platform-fault budget (8.1).

**Restart.** A VM does not survive its agent in this design: on start the agent lists the clones in its store (`list_clones`), stops and
destroys every one, records each as a finding, and reports no held run for them. The scheduler reruns the work (D3(b), N3). A driver
whose VMs outlive the agent could allow re-adoption later as an optional capability, additively; it is not a v1 rule. The sweep runs
before the first heartbeat of the new process and before the warm pool is filled.

### The guest channel

A VM's only path to the host is one stream per VM that the hypervisor ties to that VM (a socket device; no network is needed). Inside the
guest a small program, `js-guest`, part of every base image, listens on it from boot. The frame codec is a crate shared by agent and
guest, `js-guest-wire`, small and dependency-light, versioned like the agent wire (major 1; fields only added).

| Direction | Frame | Meaning |
|---|---|---|
| guest to host | `Ready{guest_version, protocol}` | the guest is up and idle; sent once |
| host to guest | `Put{dest, tar}` (chunked) | unpack an archive below `dest`; read-only mounts arrive this way |
| host to guest | `Exec{argv, cwd, user, env}` | run the job; one `Exec` per VM, a second is refused |
| guest to host | `Log{stream, seq, bytes}` | stdout and stderr, in order, windowed; the agent spools to the run's scratch, capped (`--vm-log-cap`), and marks truncation |
| host to guest | `Signal{term\|kill}` | end the job early |
| guest to host | `Exited{code, signal, oom}` | the job ended |
| host to guest | `Get{paths}` → guest `Data{path, tar}` (chunked) | pull declared outputs |
| host to guest | `Shutdown` | power off after `Get` |
| both | `Error{code}` | a refusal or a guest-side failure |

Rules. **No shared writable host path.** Nothing of the host is mounted into the guest read-write. A read-only mount of the job is
*copied in* by `Put` below `/mnt/<root>/<rel>`; a read-write mount is copied in and its contents copied *back* after `Exited` by the agent,
which writes them with the same open-by-descriptor-beneath-the-root rule as `MountRoots`, so the guest never holds a descriptor into a
host path. Outputs and logs leave only as channel frames. Read-only virtual shares are a flag (`--vm-allow-shares`), off; a writable
share is another flag (`--vm-allow-writable-shares`), off, and documented as defeating the VM boundary for those paths. **The guest is
untrusted.** Every frame is bounded (1 MiB; archive totals by `--vm-transfer-cap`; at most 4096 frames of log per second), a malformed
or out-of-order frame, an unknown frame kind where a known one is required, or a second `Ready` ends the run `failed_to_start` /
`failed` with `vm-guest-protocol`; the codec is fuzzed and has hostile-frame cases like the merged wire's `hostile_*` tests. The guest can
lie about its exit status: a result is exactly as trustworthy as the guest, which is the same as for a container. The agent puts no
credential in a guest and the guest has no route to the metadata endpoint or to the executor's control socket (5.4).

### Integrity and taint

Destroying the VM *is* the cleanup of everything the guest did; nothing in the guest is checked or trusted afterwards. The host checks of
5.12 still run after every VM job, with one addition (a minimum list may grow, 5.12): the **VM store check**: every directory in the store
is either a base image, or the clone of a run the agent holds or a warm VM it holds, and nothing else; anything else, or a driver
process the agent does not account for, is a finding. A finding, a failed destroy or a check that cannot run taints the host as for a
container; a host with a tainted flag takes no VM lease. A guest that breaks the channel protocol is not a taint (the guest is
untrusted by design); its VM is destroyed. A warm VM that shows any anomaly before assignment is destroyed, never assigned. The
scheduler's own taint at grant (5.12) is unchanged and never applies to a VM job, because a VM job cannot be `host`.

### Warm pool (optional, per agent)

A warm pool trades host memory for start time. Off by default. `--vm-warm <image>=<n>` (repeatable; R4) asks the agent to hold `n` warm VMs
of that base image.

- **A warm VM is fresh.** It was cloned from the base image and booted after the agent started, its guest said `Ready`, and it has never been
  handed a job, an input or an `Exec`. It is destroyed, not reused, once it is older than `--vm-warm-max-age` (the agent's clock; stale
  leases and clocks in a long-idle guest) and replaced. A warm VM is not a held run; the scheduler never sees it as a lease.
- **Assigning one is irreversible.** A `Start` for `base_image = X` takes a warm VM of X whose size fits the request (D26) and marks it
  assigned; from then on it is a cold VM at step 5. If none fits, the agent cold-boots (steps 2 to 4). An assigned VM never goes back.
- **Refill** is one VM at a time, after the freed capacity is back and only when all hold: the agent is `serving`; the host's free capacity
  covers the VM's full size (below); no exclusive lease is pending or held (below); the pool is below target. A refill that fails backs off
  (the merged backoff) and after `--vm-warm-max-failures` failures in a row stops until the next image change, with a finding and
  `warm_images` not listing the image.
- **Warm VMs count fully.** A booted VM holds its CPUs and memory whether or not a job runs in it, so a warm VM counts against the host and
  against the `vms` counter at its full size (5.15).
- The agent advertises `warm_images` (a fact; capabilities.md), which Rank may prefer. It is never a constraint: a request is served
  cold if no warm VM exists.

### Base images and priming

A base image is a directory in `--vm-store` holding a disk, platform auxiliary files, and a manifest: name, content digest, size, `js-guest`
version, and the profile facts of capabilities.md. **Names are immutable:** a name never denotes different content. An operator primes a
new image (installs the OS, Xcode, the simulator runtimes and `js-guest`, captures it) and gives it a new name, outside the agent;
creating images is not an agent feature. The agent's own permissions on the image directories are read-only. It verifies a digest when an
image first appears and when its manifest changes, and an image that fails is dropped from `base_images`. `base_images`,
`image_profiles` and `warm_images` are facts (capabilities.md): a new image is a capability change, reaching the scheduler at the next
heartbeat. A `Start` pins the image digest; a mismatch is `image-unavailable`.

## 5.15 Sharing a host, caps, and the whole-node class

**One accounting.** A host's capacity vector (`cpu_millis`, `memory_bytes`, `vms`, `gpus`) is shared by containers, VMs and warm VMs. The
agent offers the scheduler what it is *willing to give* (the existing rule; generalising.md): the host total, less its own reservations,
which are the warm VMs. The scheduler books leases against that offer; the agent's own accounting is the last word and refuses a `Start`
that does not fit with `no-capacity`, as the merged lifecycle does.

- A **VM** books its configured CPUs (whole cores), its configured memory and one `vms`, in full, from the moment it is cloned until it is
  destroyed; no overcommit. A VM is admitted only if each of those fits in what is free, where free counts every *booking* (a container's
  memory booking counts in full here although containers may oversubscribe among themselves) and every warm VM.
- A **container** is admitted as 6.2 says (`Book`), except that the total its memory is checked against is the host total less the memory
  of all VMs, warm ones included. A warm pool therefore shrinks what containers can be admitted to.
- `vms` is both a fact (the ceiling, capabilities.md) and a booked dimension (how many are taken). Warm VMs count in it. The pool's
  lease-count cap (`max_leases`) counts leases; a warm VM is not a lease.
- **Order.** When a warm VM is assigned to a lease the agent moves its size from "reserved" to "booked" and raises its offer in the same
  step; it never raises the offer before the assignment and never lowers it below what is booked. A refill lowers the offer only out of
  free capacity. The error direction is always under-use, never overcommit. A capacity change is emitted with the next heartbeat.

Example. A host with 12 CPUs and 64 GiB, `vms = 2`, one warm VM of 4 CPUs and 16 GiB. The offer is 8 CPUs, 48 GiB, 1 `vms`. A VM lease of
4 CPUs and 16 GiB books against the offer (free: 4 CPUs, 32 GiB) and the agent assigns the warm VM: the reservation drops to zero, the
offer rises to 12 CPUs, 64 GiB, 2 `vms` (booked: 4, 16, 1). The refill needs 4 CPUs and 16 GiB of real free capacity, finds it, boots one,
and the offer drops to 8 and 48 again (free: 4 CPUs and 32 GiB). A container job asking 40 GiB is refused on this host while a VM is leased
and one is warm: its memory is checked against 64 - 32 = 32 GiB.

### Whole-node lease class (exclusive; for performance tests)

A whole-node lease is the class `whole_machine` (`Admission::Whole`, 6.2; kbf's class of that name): it books every dimension and needs the
host to itself. A performance test must see no other job and no idle booted VM, so the host is made quiet before the lease is usable.

1. **Selection.** The scheduler picks a host with no lease and no running job, commits the lease, and places nothing else on the host from
   that moment (the choice and any placement commit under the table lock, first one wins; 5.10).
2. **Preparing.** The scheduler's next heartbeat reply tells the agent an exclusive lease is granted (an additive field, `exclusive`:
   lease id). The agent announces the phase `preparing` (the merged phase; no new leases), **stops the warm pool and destroys every warm
   VM**, stops refilling, finishes anything it still runs (a job placed before the commit completes first) and runs the post-job checks.
   `warm_images` becomes empty.
3. **Ready.** The agent reports `exclusive_ready: lease id` in the heartbeat (additive) and returns to `serving`. Until the scheduler has
   that report the lease stays `pending` and **the lease token is not minted**: the consumer cannot reach the agent, so a consumer that
   knows nothing about readiness (an older client) simply waits. The agent also accepts a `Start` for the lease from nobody but the
   leasee while the lease lasts. If the agent is not ready within the pool's `prepare_timeout` (an injected clock), the scheduler gives
   the lease up as not ready (`exclusive-not-ready`), tells the consumer, and the request is placed again; a finding on the way taints
   the host as usual.
4. **During.** Nothing else is placed. The warm pool stays off. The leasee may run containers or VMs inside its node (a VM is then cold-
   booted, and counts fully as always).
5. **End.** On release the agent removes what remains, runs the checks, announces `restoring`, refills the warm pool toward its target,
   and returns to `serving`; only then may the scheduler place other work on the host. A tainted host is replaced instead (5.12).

The agent's heartbeat is how readiness is signalled; there is no side channel. A whole-node lease on a host that is also in use by
`share` leases waits for them to end (the scheduler does not pick it) or, if the consumer asked for the node by name, queues.

## D26. The macOS VM sizing question (open; another engineer is answering it)

**Question.** Is a VM's CPU count and memory fixed (a few declared size classes) or dynamic (chosen for each lease)?

**Facts both options rest on.**
- On macOS the virtualization framework fixes a VM's CPU count and memory size when the VM is created; neither can be raised or lowered
  while it runs.
- A memory balloon device can reclaim memory from a running guest to the host and give it back, but only up to the configured size: it
  cannot grow a VM past what was configured at creation.
- A VM is cloned from a base image and booted for each lease, so *creating* a VM with any size at lease time is possible; what costs is
  the boot (the cost of steps 2 to 4 of the lifecycle; not measured here), which the warm pool exists to hide, and a warm VM is booted
  before the lease it will serve is known.
- Host memory committed to booted VMs is not available to anything else (5.15); the macOS host cannot overcommit guest memory in the way a
  Linux container host oversubscribes among containers.

**Option A: fixed size classes.** The agent declares a small set of sizes (say `small`, `large`: CPUs, memory), set by flag. A lease
names a class. Warm pool entries are per (image, class). *Warm pools:* every warm VM can serve any lease for its (image, class), so hit
rates are high and a lease never gets a VM bigger than it asked for. *Caps:* bookings are a few quantized vectors, the `vms` counter and
the arithmetic of 5.15 are simple, and the scheduler can reason about how many VMs of each class fit. *Cost:* a job that needs more than
the biggest class cannot run in a VM; a job that needs less pays for a whole class; many classes dilute the warm pool.

**Option B: dynamic, per lease.** A lease states CPUs and memory (within the host and the image). The VM is cloned and booted at that
size. *Warm pools:* a warm VM exists at a size fixed in advance (the pool's declared warm profile), so it serves a lease only if the lease
asks for no more than it has; the lease is then booked at the **VM's** size, not the request's, since the VM cannot shrink (its CPUs
cannot be reduced at all; memory can be handed back to the host by the balloon but the configured size stays the ceiling). Any lease that
no warm VM fits is cold-booted at exactly its size. *Caps:* bookings are arbitrary vectors, so fragmentation is the scheduler's
problem (6.5 packing matters more), and a warm VM's full size is reserved whichever lease may or may not use it. An optional balloon could
return the idle memory of a warm VM to the host and take it back at assignment, never past the configured size. *Cost:* more cold boots
for sizes the pool does not hold; a more complex booking.

**What each implies for the rest of the design.** The wire carries the VM's CPUs and memory in every VM job under both options, so the
choice is policy and not a wire change. In A the warm-pool key is (image, class) and `vm_sizes` is agent configuration; in B the key is
(image) with a declared warm profile and the lease carries its own sizes. The model of 5.15 holds for both: the booking is always the
size the VM was created with. **To be decided by the project owner when the sizing answer arrives; this design needs no change in the
lifecycle, the channel or the integrity rules either way.** Until then the steps that depend on it (the size fields in step 43, and step 45) are not started.

## Tests (13.1, 13.2)

The fake `VmRuntime` is in memory with a controllable clock and scripted outcomes: clone failure, a boot that never says `Ready`, a guest
that sends a bad frame, a second `Ready`, an oversized frame, floods of log frames, an unannounced exit (`Gone`), a destroy that fails,
a leaked clone, an image whose digest changed, and a store that holds an unaccounted directory. Conformance cases (agent suite, Phase F
of the steps): a VM is destroyed before the slot is freed, and a failed destroy frees nothing and taints; **no VM serves two runs** (the
fake counts VM ids per run id and fails on a repeat); a warm VM is destroyed, not reused, at the max age; assignment is irreversible and a
refused assignment cold-boots; refill respects free capacity, one at a time, and the failure brake; warm VMs count in `vms` and in the
offer, with the order of 5.15 (the offer is never above total less booked less warm at any step of a seeded run); a container job is
refused when VM memory leaves too little; the whole-node sequence: warm VMs are gone, no refill, the token is not minted before
`exclusive_ready`, a timeout gives the lease up, release refills; restart sweeps every clone; a `host` VM job and a GPU VM job are
refused; mounts are copied in and out and the guest never holds a writable host path; a hostile frame corpus ends the run and never
taints. No test asserts elapsed time (R2): boots, ages and timeouts are counted on the injected clock.
