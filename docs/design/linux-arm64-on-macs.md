# linux-arm64 jobs on the farm Macs: one Linux VM per Mac, the Linux nodes' sandbox inside it

Status: design for review; nothing in it is built. It decides the six questions below. Appendices A to G hold the numbers, the
alternatives and the tests. It is Part 2 of [multi-platform-hosts.md](https://github.com/komira-ai/jobscheduler/pull/960)
(komira-ai/jobscheduler#960, in review) worked out in detail. It answers that document's open questions Q2, Q3 and Q4, and keeps
its rulings: one js-agent per Mac, macOS first, the VM's life cycle the agent's, a fixed memory limit and an idle stop. It changes
two of #960's points for the Mac's Linux side. Ruling 9 (firewall rules around each container) and 4.2's one network ceiling per
host do not apply: the guest has no network device (decision 2). It also reads ruling 7 ("no agent inside the VM") as allowing a
run-only mode of the js-agent binary in the guest, #960's Q4 (a). Both are the owner's to confirm.

The release platforms are linux-x86_64, linux-arm64 and macos-arm64. The farm has no linux-arm64 worker, so publishing covers only
the other two. The farm's Mac Studios are arm64. A Linux VM on each Mac, run by Apple's Virtualization framework, runs arm64 Linux
on the Mac's own cores at no emulation cost. That VM becomes the farm's linux-arm64 worker.

## The decisions

1. **The shape: the Mac's own js-agent drives a VM it owns.** The guest is not a host of its own. A Mac stays one host with one
   session and one book. Its set of operating systems grows from `macos` to `macos` and `linux`, with `arch = arm64`. Inside the
   guest runs `js-agent __vm-guest`, a mode of the released linux-arm64 js-agent binary. It holds no server address and no host
   credential, and it runs every Linux start the Mac agent hands it through the Linux agent's own `CrunRuntime`. The VM is elastic
   in time. While it runs, it holds a fixed memory size (default half the Mac's memory) and sees every Mac core. With no Linux job
   for an idle time (default 5 minutes) the agent stops it, and its memory goes back to macOS jobs. A Linux start that arrives
   while the VM is stopped is accepted and waits for the boot. Why this shape and not a VM that registers as its own host:
   appendix A, with the prototypes' numbers.
2. **The sandbox is the Linux node's, the same code on the same kernel line.** crun 1.14 or later, a user namespace mapped onto
   the job user's subordinate range, `no_new_privs`, the pinned seccomp profile (the arm64 arm of `hardening.rs`), the
   cgroup v2 lease and container groups, XFS project quota on scratch, an overlay at `/work`, and the post-job checks of
   `crates/js-agent/src/integrity`. The guest has **no network device**: its only link is the virtio socket to its owner. A job
   at network level `none`, the level kbf's Linux jobs ask for, gets loopback in its own namespace, as on a node. A Linux job
   that asks for `allow-list` or `open` is not matched to a Mac (D.1). What differs in
   a VM, and how the Linux node's tests are run in the guest to show it is the same: appendix B.
3. **Storage: tierstore runs in the guest, as on a Linux node.** Its `local_cache` is a virtio block disk, a file on the Mac's
   `js-agent-scratch` APFS volume. Its cold store is the S3 front of the farm's Linux tierstore nodes, reached through a forward
   the VM's owner carries over the virtio socket. Jobs read trees through the guest's FUSE
   mount and write outputs to its put directory, exactly as on a node. On the prototype's numbers the virtio disk reads at the
   Mac's own disk speed: appendix C.
4. **Placement and kbf: an action with `OSFamily=Linux` and an arm64 ISA becomes a job with `os = linux`, `arch = arm64`.** The
   server matches it to a Mac because `linux` is in the Mac's set (#960, 4.1). Placement prefers a Linux-only arm64 host once
   one exists (#960, section 6). kbf's release already builds the linux-arm64 base tree and task image. The kbf change is one
   that every arm64 host needs, in either shape: the job's `arch`, base tree and task image come from the action's platform, not
   from the frontend-wide `--executor-arch`. Updates: **no hostkeeper and no apt in the guest.**
   The guest program ships in the Mac's package, at the Mac agent's version. The guest's OS is an image pinned by digest. A new
   pin reaches a Mac with a new package, and the VM takes it at its next boot: appendix D.
5. **Kernel and image: Ubuntu 24.04 arm64 with the 24.04 HWE kernel.** Ubuntu 24.04 is the Linux nodes' OS. The nodes run its
   6.8 kernel today, which is below the 6.9 floor for FUSE passthrough and `FUSE_NOTIFY_RESEND`, so the guest runs ahead of them
   on the HWE kernel, at a version pinned from the archive's `Packages` index. The guest program refuses a kernel below 6.9 at
   its start. The guest boots with `VZLinuxBootLoader`, from a pinned kernel and initrd outside the disk, so the root can be
   read-only with no GRUB, no EFI variable store and no cloud-init. A recipe in this repository builds the image from pins (URL and SHA-256, as
   `packaging/machine-image/pins.toml` does), and a lock file names the image's digest: appendix E.
6. **Build plan: eight pull requests on four interfaces, and four measurements before the VM serves.** The first proof is
   `komira//src/json:test` built for `linux-arm64` on the farm, with a summary of
   `Commands: N (cached: N, remote: N, local: 0)`: appendix F.

**What does not change.** The Mac's macOS jobs, its native runtime and its pool. The Linux nodes. A Mac that has no VM configured
is a macOS host exactly as today. An older server sees every Mac as a macOS host (#960, 4.1).

**Tests.** Every one asserts behaviour and none asserts timing (T10): appendix G.

---

## Appendix A. The shape, with the prototypes' numbers

Two prototypes ran the two shapes on the same Mac Studios (M3 Ultra; 28 and 32 cores; 96 GiB):

- **H, the VM as its own host:** Lima with the released linux-arm64 js-agent .deb in the guest, registered as its own host
  ([linux-vm-on-mac-docker-model.md](experiments/linux-vm-on-mac-docker-model.md), on main).
- **D, the Mac's agent drives the VM:** the signed agent launches the VM itself and starts crun containers in it through a small
  guest program (komira-ai/jobscheduler#1019, `docs/design/experiments/linux-vm-inside-agent-on-mac.md` on that branch, not for
  merge).

Each number is measured, with its sample count. An inference is marked as one.

| Question | H: VM as its own host | D: the Mac's agent drives the VM |
|---|---|---|
| Container start in a running VM | 42 ms median, p10 to p90 38 to 46 ms, from the agent receiving the start to the container running (n=20); 8 ms of it is the runtime | the channel: 0.43 ms ping and 2.29 ms for `crun run` of `/bin/true` (n=30 each). With the agent's runtime added it is the same 8 ms runtime plus the channel (inferred; the hardened path was run once, not sampled). Both are under the 100 ms target of [start-latency.md](start-latency.md) |
| Booking a Mac | two hosts on one machine: the Lima VM's 24 vCPUs and 64 GiB on top of the Mac's 28 cores and 96 GiB books up to 52 cores and 160 GiB. Neither host sees the other's bookings. Fixing that needs a fixed split, which wastes the idle side, against the owner's ruling that the split follows demand | one book: every job, of either system, is booked against the Mac's cores and memory, and the agent's ledger takes the VM's limit from macOS only while the VM is up (#960, 5.4) |
| Memory back to macOS | only by stopping the VM (15 GB footprint after builds with under 1 GB used in the guest; the balloon gave nothing back). A stopped VM is a host that left: the next Linux job waits in the job service's queue for a host that nothing starts | only by stopping the VM (5.2 GB footprint after a build with under 1 GB used; the balloon grew it to 13 GB; the idle stop gave about 13 GB back). The agent stops it when idle and starts it for the next Linux start: 4.56 s median from the start arriving to the container's exit, the VM stopped at the start (n=4) |
| Agent restart or update | an agent restart in the guest kept the job, but the host could not be placed for 20 s and 62.5 s (n=2), because the server answered `409 host-live` until the old session lapsed. Killing the VM's owner process lost the job | a second process of the same binary, in its own process group, owns the VM. It kept the VM and the running container through a restart and through a binary replaced as an update replaces it, on both Macs (one trial per cell). A start waiting for the boot completed |
| What the guest must reach | the lease server, the package repository and hostkeeper's coordinator, the tierstore upstream and the image registry. The framework's NAT reached the Mac and nothing past it, on both Macs, so each needs a forward | the tierstore upstream only, through one forward the owner carries. Images come through the Mac agent (appendix B.4) |
| Updating the guest | hostkeeper in the guest: it needs egress, a host identity without a serial or a platform UUID, and a host group of its own | none in the guest: the guest program comes from the Mac's package (appendix D) |
| Idle cost | 3.04 CPU-s per 120 s with the agent, the job service and a registry in the guest; 1.89 CPU-s per 120 s with those stopped | a smaller guest, so no more (inferred). Both are negligible |
| Sameness of the sandbox | the same binary, unchanged | the same library code (`CrunRuntime`, the integrity checks, the quota and the start checks), run by a mode of the same linux-arm64 binary. The hardened run in #1019 showed a user namespace on the job user's subordinate range (`uid_map 0 100000 65536`, the prototype image's block; a node's block starts at 200000), `NoNewPrivs: 1` and `Seccomp: 2` |

D wins on booking, memory, restarts, network and updates. H's one advantage, an unchanged binary, D keeps at the library level,
where the sandbox lives. D is also the shape of the owner's earlier rulings (#960, rulings 1, 2, 4, 12 and 14). It meets
ruling 7 if a run-only mode of the binary in the guest counts as no agent there, which is #960's Q4 (a) and the owner's to confirm.

**Sizing, with numbers.** Take a Mac of 28 cores and 96 GiB, and `memory_schedulable_percent = 90` (86.4 GiB bookable). The VM
has 28 vCPUs, one per core, and a 48 GiB limit, half the Mac's memory as Docker Desktop's default is.

- CPU is shared. Every job books against the Mac's 28 cores, with the server's CPU slack (1.1x), so nothing exceeds the slack
  whichever system the jobs run.
- While the VM is up, macOS jobs fit in 86.4 less 48, which is 38.4 GiB. Linux jobs fit in 90% of the VM's 48 GiB, 43.2 GiB:
  the agent applies the same `memory_schedulable_percent` to the VM's size as to the Mac's, and the other 4.8 GiB is the guest's
  own, for its kernel, tierstore, `tierstore fs` and the guest program, as the 10% is a node's. While the VM is stopped, macOS
  jobs fit in 86.4 GiB.
- The VM's size is the size of the Linux machine, not a limit on any job. It is the agent's, as #960 5.4 has it, and the server
  never sees it. Inside the guest, Linux jobs carry no `memory.max`, as on a node (`LeasePolicy::TrackedMemory`), and the only
  cut of the machine's memory is `memory_schedulable_percent`, so B14 holds.
- A macOS job that waits closes the Mac to the Linux jobs behind it (#960, 6.2). The Linux jobs end, the idle time passes, and
  the 48 GiB come back.

Both settings are agent flags: `--linux-vm-memory-gib` and `--linux-vm-idle-stop-secs`. The server never learns them (#960, 5.4).

## Appendix B. The sandbox inside the VM, and how it is shown to be the Linux node's

### B.1 What runs where

| On the Mac (`js-agent`, macOS) | In the guest (`js-agent __vm-guest`, Linux) |
|---|---|
| the session, the dispatch, the lifecycle and the ledger; `VmRuntime`, an implementation of `trait Runtime` (`crates/js-agent/src/lifecycle/runtime.rs`) that carries each call over the virtio socket | `CrunRuntime`, opened by the same start the crun agent makes (`crun_host::start`: crun, newuidmap, the job user, the subordinate ids, cgroup delegation, the AppArmor profile). Its report goes back in the guest's ready message |
| the VM's owner: `js-agent __vm-owner`, a second process of the same binary in its own process group (#1019 owner B) | the integrity checks after every run; their findings return in the run's end |
| image pulls for `linux/arm64`, with the Mac agent's registry policy (`--image-allow`) | an `ImageSource` (`js_contain::oci::image`) that asks the Mac for the layers and checks each against its pin, as `RegistryImages` does |
| the forward to the tierstore upstream | tierstore and `tierstore fs` (FUSE, `allow_other`), the mount provider kbf's jobs name |

The guest's units are the node's: the guest program under a unit with `Delegate=yes` and `KillMode=process`, as
`js-agent-crun.service` has, so a container outlives a restart of the guest program, and the two tierstore units.

### B.2 What differs inside a VM

| Piece | On a Linux node | In the guest | Effect |
|---|---|---|---|
| Kernel | Ubuntu 24.04's 6.8 | Ubuntu's 24.04 HWE kernel, pinned | the guest is ahead of the nodes: passthrough, resend and `fuse-jobs` (6.14) are available in the guest and not on the nodes. The sandbox suites therefore run on both kernels (B.3) |
| cgroups | v2; the agent's unit delegated | the same, inside the guest | none. The guest's whole is the VM's vCPUs and memory, which the Mac's book already holds |
| CPU | the node's cores | one vCPU per Mac core; macOS schedules the vCPU threads beside macOS jobs | a Linux job sees as many CPUs as a node of that size. Contention is bounded by the one book. Armv8.6-A features, no SVE or SME (#1019, section 8) |
| Memory | the node's RAM | the VM's fixed limit; the guest's page cache stays inside it until the VM stops | jobs are booked by their non-file memory (#1044), as on a node. The page cache never takes macOS memory beyond the limit |
| FUSE | `tierstore fs` on the node's kernel | `tierstore fs` on the guest's kernel; passthrough at 6.9 and later | the same |
| Page cache below the disk | none | the Mac's own cache may hold the disk file's pages a second time | the Mac can reclaim them, since it is file memory. Whether to turn it off (`VZDiskImageCachingMode`) is measurement M2 |
| Network | the node's network; `none` jobs get loopback; `allow-list` and `open` jobs get the per-container nft rules | no network device; `none` jobs get loopback | the same for `none`. The Mac's Linux network ceiling is `none` (D.1), so #960's per-container rules have nothing to filter. A user-mode network in the owner was not tried |
| Leftovers | the integrity checks; some taints clear after two clean re-checks | the same checks; a finding that would taint a node instead makes the agent drop `linux` from the Mac's set, discard the VM's three disks and boot a fresh one | stricter than a node, because a fresh VM is a replaced host. macOS work goes on (#960, 4.2) |
| Root file system of the host | installed once | a copy-on-write clone (APFS `clonefile`) of the pinned image at each boot, read-only in the guest | anything left in the guest root is gone at the next boot |

### B.3 How the Linux node's tests show it is the same

- **The same code, in every pull request (x86_64 Linux).** The guest mode runs on any Linux. Its tests run on the PR gate against
  the existing crun stand-in (`crates/js-agent/tests/crun_stand_in`), with the virtio socket replaced by a Unix socket pair. One
  test asserts that the guest mode opens its runtime through `crun_host::start` with the crun agent's options:
  `the_guest_mode_opens_the_runtime_the_crun_agent_opens`.
- **The real-crun suites, in the guest.** These are the suites that prove the sandbox on a node today: `crates/js-contain/tests/crun_root`
  (`inside.rs`, `cpu.rs`, `limits.rs`, `isolation.rs`, `job_fs.rs`) and the runtime conformance suite
  `crates/js-agent/tests/crun_runtime`, with `fuse.rs` enabled on a guest kernel of 6.14 or later, which the pinned HWE kernel is. They are built for
  `aarch64` and run as root in the pinned image on a Mac, unchanged. Today a hosted runner cannot be shown to boot a VM
  (measurement M4), so the run is an operator's manual run on one farm Mac. It is made for every new image pin and before the
  first Linux job serves. When a runner can boot the VM, the run becomes a beta job of the release pipeline (B9-linux), and that
  pull request changes the rule's list of beta jobs and `pr_linux_only` in the same change.
- **The arm64 arm on Linux arm64.** The same suites on a hosted `ubuntu-24.04-arm` runner show the arm64 seccomp profile and the
  arm64 code paths on an arm64 kernel. They run as a beta job, never on a pull request. B9-linux lists the beta jobs and holds
  none on Linux arm64, so the pull request that adds this job changes the rule's list and `pr_linux_only` in the same change.
- **The node's kernel.** The suites already run on 6.8 on the PR gate and its crun workflows (x86_64). The guest's newer kernel
  adds the guest run above. Together they cover both kernels the sandbox meets.

### B.4 Images without a network in the guest

kbf's Linux jobs name a digest-pinned image. The guest's `ImageSource` asks the Mac agent for the image. The Mac agent fetches it
with its own js-oci client for `linux/arm64` (`Platform::linux`, `crates/js-oci/src/manifest.rs`), under its own registry policy,
and streams the layers over the socket. The guest checks each layer against the manifest's digest, as `RegistryImages` does, and
unpacks it into its image store on the cache disk. The guest never resolves a name or opens a connection.

## Appendix C. Storage

### C.1 The guest's disks

All three are sparse raw files on the Mac's `js-agent-scratch` APFS volume ([mac-hostkeeper-package.md](mac-hostkeeper-package.md)).
That volume's quota therefore bounds them.

| Disk | In the guest | Life |
|---|---|---|
| root | the image's root file system, read-only | cloned from the pinned image at each boot |
| scratch | XFS with `prjquota` at `/var/lib/js-agent/scratch`, as on a node | made empty at each boot |
| cache | tierstore's `local_cache`, and the image store | kept across boots; discarded with the VM after a finding (B.2) |

Sizes are agent flags with no default: `--linux-vm-scratch-gib` and `--linux-vm-cache-gib`. The Macs' disks differ, so the operator
sets them. The agent refuses to configure the VM without them, once, at its start (B20).

### C.2 The cold store and the keys

The guest's tierstore has `[cold_store] kind = "s3"`, with its endpoint on a guest loopback port. The guest program carries that
port over the virtio socket. The owner connects it to the upstream address the agent's flag `--linux-vm-upstream` names, which is
no address in this repository (S1). Both legs set `TCP_NODELAY`: without it, NFS over the same kind of forward did not finish one
measurement in 90 minutes (#1019, 6.2).

The tierstore keys are the ones a Linux node's tierstore holds. The agent reads them from a file named by a flag
(`--linux-vm-upstream-credentials`, a path and never a value). It hands their bytes to the guest program at each boot, and the
guest writes them 0400 on a tmpfs. They are never written to a disk image. The guest holds no other credential.

### C.3 The cost of the virtio disk

| Measured (median of n=5 unless said) | VM's own virtio disk | the Mac's own disk | route C: tierstore's tree through VirtioFS |
|---|---|---|---|
| 46,362-file tree (277 MB), read all, guest cache dropped | 4.57 s (#1019) | 7.12 s, local copy after `purge` (n=10) | 9.44 s |
| the same tree, earlier record | 2.56 s | 2.22 s (APFS) | 11.2 s (plain VirtioFS) |
| 4,003-file tree (54 MB), read all | 0.38 s | 0.71 s (n=10) | 0.85 s |
| `cargo build --release -p js-agent` from the 4,003-file tree | 31.9 s (n=1) | not run | 32.2 s (n=1) |

So on reads, the virtio disk costs between nothing and about 15% against the Mac's own disk (2.56 s against 2.22 s). Small-file
trees read through VirtioFS take 2 to 4.4 times as long. One build did not notice the difference (32.2 s against 31.9 s), on the
one build measured.

That is why tierstore in the guest (#960 Q3 route 2, #1019's route B) is taken over a VirtioFS share of the Mac's tierstore
(#1019's route C):

- a job's tree is read from the guest's own disk;
- a share is fixed when a VM starts (#1019, 6.1), and this route needs no share for trees;
- outputs leave through the guest's own put directory, as on a node;
- the Mac's NFS loopback mount is out of the Linux path.

Route B was not measured to the end in #1019: a run's forward never started, and later runs stopped earlier. It is measurement M1,
on the same table, before the VM serves. If it reads slower than route C on the 46,362-file tree, this choice is reopened.

Writes and `fsync` on the virtio disk were not measured; they are measurement M2.

## Appendix D. Placement, kbf, and updates

### D.1 From a komira action to a container in the guest

1. Buck2 sends an action whose platform is `OSFamily=Linux,ISA=arm-a64`. kbf already reads that as `os = linux`, `arch = arm64`
   (`kbf_caps::Request::from_platform`, and `crates/js-caps/src/platform.rs` here).
2. kbf's executor submits a job with those constraints. Today it puts the frontend's `--executor-arch` there instead
   (kbf `crates/kbf-server/src/executor/config.rs`, `constraints()`), and the frontend fetches the base tree and picks the
   task image of that one arch. kbf's release already builds both for linux-arm64. The kbf change is that the job's `arch`, its
   base tree and its task image follow the action's platform. The job keeps network `none`, the provider mount and `/out`, as
   kbf's Linux jobs have today.
3. The server matches a host whose set holds `linux` and whose `arch` is `arm64`. The Linux runtime's own capabilities, such as
   `layered-rootfs` and the `mount-provider.<name>` key of the guest's tierstore, and its network ceiling (`none`), travel in
   an additive per-system block of the hello. A Linux job is matched against that block, and a macOS job against the host's
   capabilities as today. So a Linux job that asks for `allow-list` or `open` is not matched to a Mac. It goes to a Linux host
   that offers it, or it is refused `no-capable-host` at submit as today when none does. This block is added to #960's
   operating-system set, in the same pull request.
4. The Mac agent's lifecycle picks `VmRuntime` from the start's `os` (#960, 5.2), checks the fit against its ledger, and accepts
   the start. If the VM is stopped, the start waits for the boot. #960 5.5 has the table of every agent, VM and job state.
5. In the guest the start runs as on a node: the image, the tree attached through the guest's tierstore, crun, then the
   integrity checks. The result goes back through the Mac's one session.

### D.2 Updates

| Part | How a new version reaches the Mac | When the VM takes it |
|---|---|---|
| the guest program | the linux-arm64 `js-agent` binary, a file of the Mac's package below `/Library/Application Support/js-agent/`, shared into the guest read-only. At each boot the guest copies it to its tmpfs and runs the copy, so a package that replaces the file never changes a running guest. hostkeeper installs the package as today | at the next boot. The new agent adopts a VM that kept running through the update and drives its old guest program, since the guest wire is additive (B8). It admits no new Linux container into that VM: Linux starts are accepted and wait, as for a stopped VM. Once the VM's running Linux jobs end, it restarts the VM with the new program. The old VM's life after an update is bounded by its jobs' wall-time limits |
| the guest's OS (kernel, crun, tierstore, the job user's setup) | a new image digest in `packaging/vm-image/image.lock`, compiled into the agent, so a new package carries it. The agent fetches the image by digest and checks it. An operator publishes the image when its pins change (appendix E): a kernel, crun or tierstore fix reaches the Macs through a pin change, a publish and a release | as for the guest program: the agent restarts the VM once its running Linux jobs end |

Nothing in the guest updates itself, and the guest needs no egress, no hostkeeper identity and no host group. The guest's
tierstore is pinned by the image, so it can be a release behind or ahead of the upstream's. They speak S3 to each other, the
interface tierstore's build nodes already use with their upstream.

### D.3 A VM that will not boot

A boot fails when the image cannot be fetched or does not match its digest, when the framework refuses to start the VM, or when
the guest gives no ready message within the ready limit. A failed boot fails the starts that were waiting for it; they are
`lost` and run again. A VM that dies under running jobs ends them `lost`, as #960 5.5 says. After three boots in a row that fail
or die before a run ends, the agent drops `linux` from the Mac's set (a new hello), so the Mac attracts no more Linux jobs. It
tries a boot with no job waiting at most once each idle time, and it adds `linux` back after the first boot whose start report
passes. macOS work goes on throughout.

## Appendix E. Kernel and image

- **Base.** The Ubuntu 24.04 arm64 root file system from the archive, at pinned package versions. The image adds crun (at least
  1.14), uidmap, xfsprogs and the AppArmor profile crun needs on 24.04, the job user `jsjob` with 65536 subordinate ids (the steps
  of `packaging/js-agent/crun_host_steps.toml`), tierstore's linux-arm64 release archive at a pinned version, and the units of B.1.
- **Kernel.** `linux-image-generic-hwe-24.04` at a pinned version, at least 6.14 so that `fuse.rs` and `fuse-jobs` run. The framework's Linux boot loader takes an uncompressed arm64
  kernel, so the recipe stores the image's `vmlinuz` decompressed. That is to be shown in M3. The command line is the agent's:
  root read-only from the first disk, and a console written to a file.
- **Pins.** `packaging/vm-image/pins.toml`: each input by URL and SHA-256, the SHA-256 taken from the archive's `Packages` index
  or the release's `SHA256SUMS`, never from a file that was downloaded, as `packaging/machine-image/pins.toml` does.
- **Build.** `cargo xtask vm-image build` assembles the root file system without booting anything (`mmdebstrap` for `arm64`,
  then `mke2fs -d`). It runs natively on a hosted `ubuntu-24.04-arm` runner, from a manual workflow. The earlier image job failed
  there because the runner has no KVM, so QEMU fell back to emulation; this build boots nothing. The output is an OCI artifact of three blobs: root, kernel and initrd.
- **Publish and lock.** Pushing the artifact to the project's registry is an operator's step (S3), as publishing a machine image
  is. The pull request that changes `pins.toml` also changes `image.lock`, which records the pins file's SHA-256 and the
  artifact's digest. `cargo xtask vm-image check`, run on the PR gate, fails a lock that does not match the pins.
- **On the Mac.** The agent fetches the digest, checks every blob, and decompresses the root once per digest below its scratch
  volume. Each boot then clones it.

## Appendix F. Build plan

Four interfaces land first; the work behind each runs in parallel. Every pull request carries its own tests, and every count of
lines and branches in `coverage-baseline` stays at 0.

| # | Pull request | Repository | Interface it lands or uses | After |
|---|---|---|---|---|
| 1 | `js-vm-guest-wire`: the guest protocol. Length-delimited messages that mirror the `Runtime` calls, the ready message (version and start report), images, the forward, and power off | jobscheduler | **I1, lands it** | none |
| 2 | the operating-system set and the per-system capability block (#960's `os-set` change, with D.1 step 3) | jobscheduler | **I2, lands it** | #960 approved |
| 3 | `packaging/vm-image`: pins, the recipe, `xtask vm-image build` and `check`, `image.lock`, the manual workflow | jobscheduler | **I3, lands it: the image's layout and the boot's command line** | none |
| 4 | kbf: the job's `arch` from the action's platform; the linux-arm64 base tree; an arm64 job image | kbf | **I4, lands it: the job's constraints** | none |
| 5 | `js-agent __vm-guest`: serves I1 with `CrunRuntime`, the integrity checks, tierstore attach, the image source and the forward; refuses a kernel below 6.9 | jobscheduler | uses I1, I3 | 1 |
| 6 | `js-agent __vm-owner` and the boot: the state machine of #1019, the three disks, the boot loader, the shares, the idle stop, adoption after a restart | jobscheduler | uses I1, I3 | 1, 3, M3 |
| 7 | `VmRuntime`, the ledger (#960, 5.4), one test per cell of #960 5.5, the Mac's set and block, and the package carrying the guest binary | jobscheduler | uses I1, I2 | 1, 2, 5, 6 |
| 8 | komira: register buildrules' `linux_aarch64` execution platform (`extra_linux_arm64` beside `extra_darwin` in `tools/build/platforms/default/BUCK`); the farm's property set `OSFamily=Linux,ISA=arm-a64`, with no `pool=` or `label.` key | komira | uses I4 | none |

**Measurements, before pull request 7 serves a Linux job** (each on one farm Mac, with every wait bounded):

- **M1.** Route B on the trees of #1019 section 6 (walk, read all, first byte; run 1 and the median of n=5), and the build of the
  4,003-file tree.
- **M2.** The cache disk: write and `fsync` throughput, and reads with `VZDiskImageCachingMode` automatic against uncached, with
  the Mac's memory recorded.
- **M3.** The HWE kernel booting under `VZLinuxBootLoader`: boot to the guest's ready message, n=20. Also the owner surviving the
  update hook's `launchctl bootout`, which #1019 did not run (it ran `kickstart -k`).
- **M4.** Whether a hosted Apple silicon runner can boot the VM, which decides where B.3's guest suites run.

**The first proof.** On the farm, with `[buildrules_re] linux_arm64_enabled = true` and the property set of pull request 8 in the
farm's configuration (not committed):
`buck2 build --target-platforms komira//tools/build/platforms:linux-arm64 komira//src/json:test`. This target is a Mojo package
with no dependencies and no natives, and its lock already holds a `linux-aarch64` graph. Its compile and its tests are arm64
actions, so they run in the guest. The summary must read `Commands: N (cached: N, remote: N, local: 0)`. The run's record names
the Mac and the job ids that ran it.

## Appendix G. Tests

All of these run on Linux x86_64 on the PR gate, on fakes and the injected clock (`js_state::ManualClock`), except those marked
beta or manual.

- **I1 (`js-vm-guest-wire`).**
  - `every_message_reads_back_from_its_bytes`.
  - `an_unknown_field_is_skipped_and_an_unknown_message_is_refused_by_name`.
  - `a_frame_longer_than_the_limit_is_refused`.
  - `a_guest_of_the_previous_release_is_driven_by_this_agent` (B8, against a copy of the previous release's wire kept as a fixture).
- **I2.** #960's Part 2 tests, plus:
  - `a_linux_job_is_matched_against_the_linux_block`.
  - `a_macos_job_ignores_the_linux_block`.
  - `the_previous_release_reads_a_host_with_a_linux_block_as_a_macos_host` (B8).
  - `an_allow_list_linux_job_is_not_matched_to_a_mac`.
- **I3.**
  - `a_pin_without_a_sha256_is_refused`.
  - `a_lock_that_does_not_match_the_pins_fails_check`, the negative case of the new check (T15).
  - `the_boot_command_line_names_a_read_only_root`.
- **Guest mode.**
  - `the_guest_mode_opens_the_runtime_the_crun_agent_opens`.
  - `a_kernel_below_6_9_is_refused_and_the_report_says_so`.
  - `a_start_report_with_a_missing_piece_reaches_the_mac`.
  - `an_image_layer_that_does_not_match_its_digest_is_refused`.
  - `the_forward_carries_bytes_both_ways_and_closes_when_either_side_closes`.
  - `the_guest_runs_a_copy_of_its_program_from_the_tmpfs`.
  - `a_run_whose_channel_drops_keeps_running_and_its_end_is_read_later`.
  - `integrity_findings_return_with_the_runs_end`.
  - `the_credentials_are_written_0400_on_the_tmpfs_and_nowhere_else`.
- **Owner.**
  - `a_start_while_stopped_boots_and_waits`.
  - `a_failed_boot_returns_the_vm_to_stopped_and_fails_the_waiters`.
  - `a_guest_that_never_says_ready_is_stopped_at_the_ready_limit`.
  - `the_root_is_cloned_and_scratch_made_empty_at_each_boot_and_the_cache_kept`.
  - `a_finding_discards_all_three_disks`.
  - `a_stale_stop_notice_of_an_earlier_boot_is_ignored`.
  - `the_idle_stop_comes_after_the_idle_time_with_no_connection`.
  - `a_guest_of_another_version_is_restarted_at_the_next_idle_point`.
- **`VmRuntime` and the ledger.** One test per cell of #960 5.5, named `<agent>_<vm>_<job>`; a cell that cannot occur asserts
  the agent never reaches it. Also #960 5.4's ledger tests, and:
  - `a_mac_without_the_vm_flags_reports_macos_only`.
  - `a_mac_whose_vm_failed_its_start_report_drops_linux`.
  - `three_failed_boots_in_a_row_drop_linux_and_a_passing_boot_adds_it_back`.
  - `a_vm_that_dies_under_running_jobs_ends_them_lost`.
  - `after_an_update_no_new_container_enters_the_old_vm_and_it_restarts_once_its_jobs_end`.
  - `linux_jobs_book_memory_schedulable_percent_of_the_vms_size`.
- **Beta and manual.** The guest suites of B.3, and the first proof.

**The failure and boundary cases, and why the list is complete.** Each crossing of a boundary has a failure test:

- the wire: an unknown field, an unknown message, a frame over the limit, the previous release;
- the image: a bad pin, a lock out of date, a layer with the wrong digest;
- the guest's start: a low kernel, a missing piece;
- the boot: a failure, no ready message, a stale notice, a version mismatch, three failures in a row, a VM that dies under jobs;
- the update: an old VM with jobs running;
- the channel: a drop while a run is running;
- the forward: a close from either side;
- the credentials: anywhere but the tmpfs;
- the ledger and the life cycle: every cell of #960 5.5;
- the host's set: no flags, a failed report, a job asking for a network the Mac's Linux side does not offer.

The sandbox's own failure cases are the existing suites', run in the guest.
