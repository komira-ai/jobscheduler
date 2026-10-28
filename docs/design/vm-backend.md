# VM runtime backend, sharing and caps, the VM side of the whole-node class (5.14, 5.15)

Companion to [docs/design.md](../design.md), sections 5.4, 5.10 to 5.12 and 6.2. Section numbers refer to design.md; 5.14 and 5.15 are
defined here. Capabilities and constraints (what picks a host and an image) are in [capabilities.md](capabilities.md).

## 5.14 The VM backend of `js-agent`

`js-agent` runs a job in a container (the merged `ContainerRuntime`) or in a virtual machine. Two rules govern the VM path and are not
options:

1. **A fresh VM per run. A VM is never reused.** Every run gets a VM cloned from its base image for that run and destroyed when the run
   ends, whatever its outcome. A lease may hold several runs; each has its own VM and no VM is reused between two of them. A VM that has
   run a job is never handed a second job, never reverted, never "cleaned".
2. **The machine is reused; the VM is not.** The host serves many leases and runs (VM or container), in a row and at the same time, under
   the rules of 5.10 and 5.15; what is fresh each time is the guest. Several fresh VMs exist on one host at once, up to its `vms` slots,
   each booked in full (D57, 5.18); whether a host can run VMs at all is a fact of its instance type (hosts.md, section 4). A warm pool, to hide the boot, is future work (below) and not designed here.

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
| `--vms <n>` | the most VMs at once; the `vms` fact |
| `--vm-driver <path>`, `--vm-store <dir>` | the driver program (absolute path, no `PATH` lookup, its own environment) and the directory holding base images and clones |
| `--vm-network none\|nat` | the guest's network; `none` by default |
| `--vm-boot-timeout`, `--vm-log-cap`, `--vm-transfer-cap` | bounds on boot, on spooled log bytes and on archives over the channel |
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

### Lifecycle of one VM run

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
before the first heartbeat of the new process.

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
is either a base image, or the clone of a run the agent holds, and nothing else; anything else, or a driver
process the agent does not account for, is a finding. A finding, a failed destroy or a check that cannot run taints the host as for a
container; a host with a tainted flag takes no VM lease. A guest that breaks the channel protocol is not a taint (the guest is
untrusted by design); its VM is destroyed. The
scheduler's own taint at grant (5.12) is unchanged and never applies to a VM job, because a VM job cannot be `host`.

### Future work

A pool of pre-booted VMs could hide the boot of each run; it is not designed here, and it would have to keep true that a pooled VM has
never been handed a job, that handing it out is irreversible, that it was cloned from the image digest the `Start` pins, and that it
counts in full in the host's accounting (5.15) while it waits.

### Base images and priming

A base image is a directory in `--vm-store` holding a disk, platform auxiliary files, and a manifest: name, content digest, size, `js-guest`
version, and the profile facts of capabilities.md. **Names are immutable:** a name never denotes different content. An operator primes a
new image (installs the OS, Xcode, the simulator runtimes and `js-guest`, captures it) and gives it a new name, outside the agent;
creating images is not an agent feature. The agent's own permissions on the image directories are read-only. It verifies a digest when an
image first appears and when its manifest changes, and an image that fails is dropped from `base_images`. `base_images`
and `image_profiles` are facts (capabilities.md): a new image is a capability change, reaching the scheduler at the next
heartbeat. A `Start` pins the image digest; a mismatch is `image-unavailable`. When an image leaves `base_images` (digest failure, removal)
or its manifest or digest changes, a run already started keeps the VM it was cloned with and later runs of that name are refused until
the new content verifies.

## 5.15 Sharing a host, caps, and the whole-node class (the class itself is 5.17)

**One accounting.** A host's capacity vector (`cpu_millis`, `memory_bytes`, `vms`, `gpus`) is shared by containers and VMs. The
agent offers the scheduler what it is *willing to give* (the existing rule; generalising.md): the host total less what is booked. The
scheduler books leases against that offer; the agent's own accounting is the last word and refuses a `Start` that does not fit with
`no-capacity`, as the merged lifecycle does.

- A **VM** books its configured CPUs (whole cores), its configured memory and one `vms`, in full, from the moment it is cloned until it is
  destroyed; no overcommit. It is admitted only if each of those fits in what is free, where free counts every *booking* (a container's
  memory booking counts in full here although containers may oversubscribe among themselves). A lease that holds several runs holds the
  size of its VMs for its whole term; between two runs no VM exists, and the next run's VM is cloned inside the same booking.
- A **container** is admitted as 6.2 says (`Book`), except that the total its memory is checked against is the host total less the memory
  of all VMs that exist.
- `vms` is both a fact (the ceiling, capabilities.md) and a booked dimension (how many are taken). The pool's lease-count cap
  (`max_leases`) counts leases, not VMs.
- **Order.** The offer rises only after the VM is destroyed and the slot freed (the cleanup order of 5.14), and falls at admission, before
  the clone. The error direction is always under-use, never overcommit. A capacity change is emitted with the next heartbeat.

Example. A host with 12 CPUs and 64 GiB, `vms = 2`. A lease of 4 CPUs and 16 GiB books against the offer of 12 CPUs, 64 GiB, 2 `vms`;
the offer is now 8, 48, 1. Its first run clones and boots a VM of that size (booked: 4, 16, 1) and destroys it at the end; only then does
the slot free. The lease's second run clones a new VM; the booking is the same. A container job asking 56 GiB is refused on this host
while a VM exists: its memory is checked against 64 - 16 = 48 GiB.

### Whole-node lease class (exclusive; for performance tests and GPU testing)

A whole-node lease is the class `whole-node` (admission `Whole`, 6.2; kbf's `whole_machine`, the merged `gpu-host`): it books every
dimension and needs the host to itself. Its full account is in [whole-node.md](whole-node.md): the class and how the merged names relate to
it, eligible hosts and admission, the reservation of exactly one host, the drain and its bound, the caps, elastic providers, the end of the
lease and host jobs. What belongs to the VM backend and stays here:

- **No VM runs when the host is given.** At `preparing` the agent confirms that no VM is running (the host was empty at the commit, so none
  should be; one found is a finding and is destroyed) and runs the post-job checks, including the VM store check. The host is reported ready
  only after both, and the lease token is minted only then (D30). The fields are `exclusive` (heartbeat reply) and `exclusive_ready`
  (heartbeat), both additive.
- **During the lease** the leasee may run containers or VMs inside its node; each VM is fresh for its run and counts in full as always.
- **At the end** the agent destroys every VM, removes what remains, runs the checks, announces `restoring` and returns to `serving`; only
  then may the scheduler place other work on the host.
- **The tests** of the whole-node sequence on the fake VM runtime are listed below; the reservation, drain and cap tests are in
  whole-node.md.

## D26. The macOS VM sizing question (open; another engineer is answering it)

**Question.** Is a VM's CPU count and memory fixed (a few declared size classes) or dynamic (chosen for each run)?

**Facts both options rest on.**
- On macOS the virtualization framework fixes a VM's CPU count and memory size when the VM is created; neither can be raised or lowered
  while it runs.
- A memory balloon device can reclaim memory from a running guest to the host and give it back, but only up to the configured size: it
  cannot grow a VM past what was configured at creation.
- A VM is cloned from a base image and booted for each run, so *creating* a VM with any size at run time is possible; what costs is the
  boot (the cost of steps 2 to 4 of the lifecycle; not measured here), paid on every run because no VM is reused.
- Host memory committed to booted VMs is not available to anything else (5.15); the macOS host cannot overcommit guest memory in the way a
  Linux container host oversubscribes among containers.

**Option A: fixed size classes.** The agent declares a small set of sizes (say `small`, `large`: CPUs, memory), set by flag. A lease
names a class and every VM of its runs is that size. *Caps:* bookings are a few quantized vectors, the `vms` counter and the arithmetic
of 5.15 are simple, and the scheduler can reason about how many VMs of each class fit. *Cost:* a job that needs more than the biggest
class cannot run in a VM; a job that needs less pays for a whole class.

**Option B: sizing per run.** A lease states CPUs and memory (within the host and the image), and each run's VM is cloned and booted at the
size that run needs, within the lease's booking. *Caps:* bookings are arbitrary vectors, so fragmentation is the scheduler's problem (6.5
packing matters more), and the lease's booking is the largest VM any of its runs will ask for. *Cost:* the VM cannot shrink (its CPUs
cannot be reduced at all; memory can be handed back to the host by a balloon but the configured size stays the ceiling), so a run is
booked at the size it was created with; the booking is more complex.

**The trade-off is boot cost per run.** Under both options every run boots a VM. A is simpler to book and to reason about; B wastes less
memory and refuses fewer jobs but books arbitrary vectors. Neither option changes the cost of a boot.

**What each implies for the rest of the design.** The wire carries the VM's CPUs and memory in every VM job under both options, so the
choice is policy and not a wire change. In A `vm_sizes` is agent configuration; in B the lease carries its sizes. The model of 5.15 holds
for both: the booking is always the size the VM was created with. **To be decided by the project owner when the sizing answer arrives;
this design needs no change in the lifecycle, the channel or the integrity rules either way.** Until then the size fields of step 43 are
not started.

## Tests (13.1, 13.2)

The fake `VmRuntime` is in memory with a controllable clock and scripted outcomes: clone failure, a boot that never says `Ready`, a guest
that sends a bad frame, a second `Ready`, an oversized frame, floods of log frames, an unannounced exit (`Gone`), a destroy that fails,
a leaked clone, an image whose digest changed, and a store that holds an unaccounted directory. Conformance cases (agent suite, Phase F
of the steps): a VM is destroyed before the slot is freed, and a failed destroy frees nothing and taints; **no VM serves two runs** (the
fake counts VM ids per run id and fails on a repeat, and a lease with two runs sees two VM ids); VMs count in `vms` and in the offer, with
the order of 5.15 (the offer is never above total less booked at any step of a seeded run); a container job is refused when VM memory
leaves too little; the whole-node sequence: no VM is running at `preparing`, a VM found there is a finding, the token is not minted before
`exclusive_ready`, a timeout gives the lease up and the re-place is capped, a reserved host places nothing new and its reservation ends at its deadline (whole-node.md); restart
sweeps every clone; a `host` VM job and a GPU VM job are
refused; mounts are copied in and out and the guest never holds a writable host path; a hostile frame corpus ends the run and never
taints. No test asserts elapsed time (R2): boots, ages and timeouts are counted on the injected clock.
