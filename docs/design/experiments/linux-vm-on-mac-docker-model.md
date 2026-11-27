# Linux build jobs on a Mac the way Docker does it: one long-running Linux VM, our crun inside it

> Historical: this experiment registered with a join token and `--agent-token-file`; G3 removed both, and a host is admitted by its host certificate alone.

Status: experiment record. It states what was run, what was measured and what did and did not work. It decides nothing. The open questions it raises are listed in section 9 and left open.

## What was asked and the short answer

Docker Desktop runs Linux containers on a Mac in one long-running Linux VM, with the container runtime inside it, as Docker describes Docker Desktop (not re-checked here). The question was whether we can copy that shape with our own launch path:

- boot one long-running arm64 Linux VM on a Mac with Apple's Virtualization framework, as an ordinary user, with no root and nothing installed system-wide;
- inside it, run what a Linux build node runs: the Linux arm64 `js-agent` with `crun`, registered with the job service as a Linux arm64 host;
- send it a real Linux arm64 build as a job, and measure.

The answer, in short:

- **It works.** A job submitted to the job service was placed on the VM's agent and run by our crun path. The job built this repository's workspace in release mode inside a container in the VM. The job got the same hardening as on a Linux node: a user namespace, `no_new_privs` and the seccomp filter (section 4).
- **A container start inside the running VM pays no VM boot.** From the agent receiving a start to the container running took a median of 42 ms (section 5.2). Booting a VM costs 7.9 s, once (section 5.1). Apple's `container` tool in the earlier record (`linux-containers-on-macos.md`) paid about 0.54 s of VM boot per container.
- **Idle CPU is near nothing; idle memory is not.** An idle VM with the agent running costs the Mac about 2.5% of one core. But the VM process does not give back memory its guest touched until the VM stops: after the builds it stayed at a 15 GB footprint, 23.2 GB resident, with the guest using under 1 GB. A memory balloon that the host drives did not return memory to macOS either (section 5.3).
- **Inputs read through a shared folder are about 4.5 times slower than from the VM's own disk** for a tree of 46,362 small files (section 5.4).
- **The VM lives as long as one Mac process.** Killing Lima's host agent killed the VM, and its running job ended `lost`. Restarting the agent inside the VM kept its running job (section 5.5).
- Two defects of ours were found on the way and are filed: komira-ai/jobscheduler#1012 and komira-ai/jobscheduler#1013 (section 7).

## 1. Host and software

| | |
|---|---|
| Host | Mac Studio, Apple M3 Ultra, 28 cores, 96 GiB, macOS 26.6 (build 25G5043d), `kern.hv_support` 1 |
| Account | an ordinary user, with no `sudo` on the Mac |
| Launcher | Lima 2.3.0 (`lima-2.3.0-Darwin-arm64.tar.gz`, checked against the release's `SHA256SUMS`), `vmType: vz`. `limactl` carries the `com.apple.security.virtualization` entitlement in its release signature |
| Guest | Ubuntu 24.04 arm64 cloud image (`release-20261017`, the digest Lima's template pins), kernel 6.8.0-147-generic, systemd 255, cgroup v2 |
| VM size | 24 vCPUs, 64 GiB memory, 200 GiB sparse disk; one virtiofs share, writable |
| Agent | `js-agent_0.1.332_arm64.deb` from the v0.1.332 release, with its `js-agent-crun` unit as shipped |
| crun | Ubuntu's `crun` 1.14.1-1 (`/usr/bin/crun`, with the distribution's AppArmor profile) |
| Job service | `js-lease-server` 0.1.332, aarch64 musl archive from the same release, inside the VM on loopback (section 3, shortcut 1) |
| Client | `js` 0.1.332, aarch64 musl archive from the same release |
| Images | `busybox` 1.37.0 linux/arm64 `sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c` (the pin in `crates/xtask/src/crun_tests.rs`); `rust:1-bookworm` linux/arm64 `sha256:7dabf6d6fc774b4a25df2c10bd97cd9dfd444d1e3667bb607d29b31b4cd54d8c` (the image of the earlier record) |

Every release file was checked against the digest its release lists before use.

## 2. The launcher, and why that one

The launcher is Lima with its `vz` driver.

- **It is a launcher that is known to work.** Its `vz` driver drives Apple's Virtualization framework directly: EFI boot, virtio block, virtio network over the framework's NAT, virtiofs and vsock. As we understand their documents, Colima and Rancher Desktop run Docker on a Mac on top of Lima; that was not checked here.
- **It runs as an ordinary user and installs nothing.** It is one signed tarball that runs from any directory.
- **It keeps one VM up for as long as asked.** The VM is owned by one per-instance process, `limactl hostagent`, which starts the VM and keeps it running. Stopping and starting it are commands.
- **What was not chosen, and why:**
  - Our own `js-vmm` boots macOS guests only (`VZMacOSBootLoader` and `VZMacPlatformConfiguration` in `crates/js-vmm/src/vz/config.rs`). A Linux guest would need a new boot path, so it is not an existing launcher for this.
  - Apple's `container` tool runs one VM per container, which is the opposite of the shape asked for. The earlier record measured it.
  - podman's machine uses vfkit, as we understand podman's documents, a launcher much like Lima's; it was not tried. Lima was chosen because one tarball holds both the launcher and the image handling.

**Everything under one folder.** Lima keeps its instances under `LIMA_HOME`, but it downloads images into the Go user cache directory, `~/Library/Caches/lima`, and no setting moves that. The run therefore started `limactl` through a two-line wrapper that sets `HOME` and `LIMA_HOME` to directories inside one folder:

```sh
#!/bin/sh
# limactl with every file it writes kept under <root>
exec env HOME=<root>/home LIMA_HOME=<root>/lima-home <root>/lima-bin/bin/limactl "$@"
```

`<root>` is one folder in the user's home directory. After this, `~/Library/Caches` held nothing of Lima's, and deleting `<root>` removes everything. The VM's configuration:

```yaml
minimumLimaVersion: 2.0.0
base:
- template:_images/ubuntu-24.04
vmType: vz
arch: aarch64
cpus: 24
memory: 64GiB
disk: 200GiB
mountType: virtiofs
mounts:
- location: "<root>/share"
  mountPoint: /mnt/share
  writable: true
containerd:
  system: false
  user: false
```

The first `limactl start` took 11.3 s, including cloud-init's first run (one sample). The guest has a `virtio_balloon` device that Lima attaches. Its feature bits are 0 (`MUST_TELL_HOST`), 2 (`DEFLATE_ON_OOM`) and `VERSION_1`. Free page reporting (bit 5) is not offered.

## 3. Inside the VM: a Linux arm64 build node, and the shortcuts taken

What already exists for arm64, used as released:

- the agent: `js-agent_0.1.332_arm64.deb`, which installs the binary, the units, tmpfiles and sysusers and makes the `jsjob` user with its subordinate id ranges;
- the server and the client: aarch64 musl archives of `js-lease-server` and `js`;
- the agent detects its platform itself and reports `os=linux`, `arch=arm64` (through `js_types::normalise_arch`, called in `crates/js-agent/src/facts.rs`).

The host was then set up as on a Linux node: `apt-get install crun uidmap apparmor xfsprogs nftables`, `systemd-tmpfiles --create`, the flags file, then `systemctl enable --now js-agent-crun`.

What does not exist for arm64, or not on a Mac, and the shortcut taken for each:

1. **A job service the VM can register with.** Registering with the production job service needs a pool whose capabilities say `arch = "arm64"` (the server refuses a joined pool that names no `arch`), and a join token for that pool. Both are operator steps. Instead, the released `js-lease-server` ran inside the VM, on loopback, as a systemd unit, with one pool:

   ```toml
   name = "linuxvm"
   class = "burst-host"
   max_machines = 1

   [shape]
   cpu_millis = 20000
   memory_bytes = 51539607552

   [capabilities]
   os = "linux"
   arch = "arm64"
   egress = "open"
   ```

   The server's unit ran it as in `docs/runbooks/fuse-layers-and-mounts.md`, with agents on port 7300 and the job service on 7302:

   ```text
   js-lease-server --listen 127.0.0.1:7300 --job-listen 127.0.0.1:7302 --boot resume \
     --state-file <state>/server/state --token-file <state>/client.token \
     --agent-token-file <state>/join.token --agent-pool-config <state>/pool.toml
   ```

   The two tokens were 32 random bytes in hex, made in the VM, mode 0600. The first start used `--boot initialise`.

   With `--agent-pool-config`, admission at submit reads the pool's own capabilities. A pool that named only `arch` refused a job constrained to `os = linux` as `no-capable-host`. One that did not name `egress = "open"` refused a job with `network open` the same way.

2. **A separate scratch disk with XFS project quotas.** The agent's unit requires `/var/lib/js-agent/scratch` to be a mount point, and each job's `/tmp` gets a project quota. Here the scratch was a 120 GiB sparse file on the VM's disk, formatted XFS and mounted by fstab with `loop,prjquota`. The agent reported `scratch quota: project (xfs project quota on /dev/loop0)`.

3. **Images.** The agent pulls only from registries on its `--image-allow` list. A registry's token realm must be on the registry's own host, and no flag allows another. Docker Hub's realm is `auth.docker.io`, so a Docker Hub image is refused at pull time, after placement:

   ```
   the token realm https://auth.docker.io/token is not the registry's host or an allowed host
   ```

   No arm64 Linux image of ours is published on a registry the agent may use. Instead, a registry ran on the VM's loopback (`distribution` 3.1.2, `registry_3.1.2_linux_arm64.tar.gz` checked against its release's sha256). The two pinned images were copied into it by digest with `crane copy --platform linux/arm64` (go-containerregistry 0.22.1, checked against its `checksums.txt`). The agent took the registry with `--image-allow-loopback=127.0.0.1:5000/poc`, as in `docs/runbooks/fuse-layers-and-mounts.md`. The digests in the registry are the pins above.

4. **Job output.** The server had no log store (`[logs]` needs an S3 store), so `js job run` could not follow output (`no-log-sink`). Output was read from `js job result`, whose `stdout_tail` and `stderr_tail` carry the end of each stream.

5. **Network.** The agent ran with `--egress open` and the VM's resolver as `--egress-dns`, so the build could clone the repository and fetch crates. Nothing in the VM's own network was changed. Its NAT is the framework's NAT, reached through the Mac.

The agent's flags file, `/etc/js-agent/flags`:

```text
--server=http://127.0.0.1:7300
--pool=linuxvm
--host-id=<host id>
--image-allow-loopback=127.0.0.1:5000/poc
--egress=open
--egress-dns=<the VM's resolver>
```

The unit's own `ExecStart` adds `--crun-bin /usr/bin/crun --job-user jsjob --state-root ... --scratch-root ...`, as shipped.

## 4. The jobs

**The first job**, busybox, network `none`:

```text
js job run --server http://127.0.0.1:7302 --token-file <client token> --id smoke-3 \
  --spec.image 127.0.0.1:5000/poc/busybox@sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c \
  --spec.constraints.os linux --spec.constraints.arch arm64 \
  --spec.command /bin/sh --spec.args -c \
  --spec.args 'uname -m; id; cat /proc/self/uid_map; grep -E "NoNewPrivs|Seccomp:" /proc/self/status' \
  --spec.wall-limit-seconds 5m
```

It succeeded with exit 0. Its `stdout_tail`:

```text
aarch64
uid=1000 gid=1000
         0     200000      65536
NoNewPrivs:	1
Seccomp:	2
```

So the job ran as uid 1000 in a user namespace mapped onto the `jsjob` range, with `no_new_privs` set and a seccomp filter. This is the Linux job contract, which Apple's `container` tool did not give in the earlier record.

**The proof: a Linux arm64 build of this repository, as a job.** This used the `rust:1-bookworm` arm64 image, network `open`, and 20 CPUs and 40 GiB booked. The job's script:

```sh
set -e
git config --global advice.detachedHead false
cd /scratch
git clone -q --depth 1 --branch v0.1.332 https://github.com/komira-ai/jobscheduler src
cd src
rustup show active-toolchain     # the pinned 1.95.0, installed by rustup on first use
cargo fetch --locked
cargo build --locked --release --workspace
```

It was submitted with `--spec.env CARGO_HOME=/scratch/cargo --spec.env RUSTUP_HOME=/scratch/rustup`, so that rustup and cargo write under the job's scratch directory. The image's own homes are on its read-only root.

| | |
|---|---|
| Outcome | `succeeded`, exit 0; `Finished release profile [optimized] target(s) in 49.60s` |
| Clone | 2.8 s |
| Toolchain install and `cargo fetch` | 9.8 s |
| `cargo build --release --workspace` | 49.4 s by the script's own clock, 49.60 s by cargo's line (24 vCPUs in the guest; 20 booked), 25 executables in `target/release` |
| Usage the agent reported | 625 CPU-seconds, peak memory 6.3 GB |
| `js-agent` alone (`cargo build --locked --release -p js-agent`, earlier job) | `Finished` in 24.55 s; the output is `ELF 64-bit LSB pie executable, ARM aarch64` |

One earlier attempt of the workspace job exited 1 with nothing printed, because the script sent rustup's and cargo's output to `/dev/null`. It was not looked into further: the same script with the output kept succeeded.

## 5. The numbers

All times are in ms or s as marked. "n" is the number of samples, and every sample is shown in the tables.

### 5.1 VM boot

`limactl stop`, then `limactl start` timed on the Mac. Then the agent's health route was polled inside the guest every 0.1 s until it answered `placeable`. The job service, the registry and the agent are systemd units in the guest, so they come up with it. n = 5.

| | runs |
|---|---|
| `limactl start` returns (Lima's readiness: SSH and its guest agent) | 8046, 7918, 7881, 7999, 7898 ms |
| until the agent is `placeable` | 8135, 8008, 7969, 8087, 7987 ms |
| the agent's `registered` line, seconds after the guest kernel started (`journalctl -o short-monotonic`) | 4.88, 5.65, 4.98, 5.41, 5.63 s |
| guest `systemd-analyze` (earlier set of 5) | kernel 1.57 to 1.59 s, userspace 3.84 to 4.38 s |
| `limactl stop` (earlier set of 5) | 1108 to 1123 ms |

The agent registers about 5 s into the guest's boot. Assuming the guest kernel starts close to when `limactl start` begins (the two clocks are different, so this is not measured), the other 2.5 to 3 s before `limactl start` returns is Lima's own readiness wait, which the agent does not need.

### 5.2 Container start inside the running VM

20 busybox jobs (`/bin/true`) were run one after another with `js job run`. The intervals come from the agent's own `StartTimeline` log line for each job (`placed_at_ms`, `received_at_ms` and the runtime's `begin` and `running` stamps). The VM and the image were warm.

| interval | median | p10 to p90 | min to max |
|---|---|---|---|
| placed by the server to received by the agent | 6 ms | 5 to 7 | 4 to 7 |
| received by the agent to the container running | 42 ms | 38 to 46 | 33 to 46 |
| the runtime's part of it (`begin` to `running`: cgroup, bundle, supervisor, crun start) | 8 ms | 7 to 9 | 6 to 11 |

The job service's own `started_at` comes 2.8 to 4.3 s after `placed_at`, and `js job run` returns about 4.3 s after it starts. Both are reporting, not launching. The agent reports a start in its next heartbeat (every 5 s), and `js job wait`, which `js job run` uses, polls the job service every 2 s (`POLL_EVERY` in `crates/js-client/src/jobs/loops.rs`). For the first job on the 1.5 GB rust image, which the agent pulled from the loopback registry, `placed_at` to `started_at` was 7.1 s, heartbeat delay included (one sample).

For comparison, the nearest Linux figure is 26.8 ms from `crun` start to the job's first byte (`docs/design/agent-egress.md`). It is a different interval: it leaves out the cgroup and bundle steps counted here, and it was measured on other hardware. The earlier record measured about 600 ms per container for Apple's `container` tool, of which 540 ms was a VM boot.

### 5.3 What the Mac sees while the VM is idle

**CPU.** The CPU time of the VM's process (`com.apple.Virtualization.VirtualMachine`, the framework's XPC service) and of `limactl hostagent` was read with `ps -o time` before and after 120 s, at least 5 min after boot.

| state | VM process | `limactl hostagent` |
|---|---|---|
| agent, job service and registry running, no job | 3.04 CPU-s per 120 s (2.5% of one core) | 0.09 CPU-s (an earlier sample) |
| the three stopped | 1.89 CPU-s per 120 s (1.6% of one core) | |
| within 10 minutes of a boot (Ubuntu's boot-time timers) | 10.1 CPU-s per 120 s | |

The guest's load average was 0.00 to 0.07 in these samples.

**Memory, without a balloon (Lima's VM).** Read with `footprint -p` and `ps -o rss` of the VM process, and `free -m` in the guest.

| state | VM process on the Mac | guest `used` / `buff/cache` |
|---|---|---|
| after the builds of section 4, idle | footprint 15 GB, rss 23.2 GB | 1198 / 7078 MiB |
| after `echo 3 > /proc/sys/vm/drop_caches` in the guest, +20 s | footprint 15 GB, rss 23.2 GB | 898 / 145 MiB |
| after `limactl stop` and `start` (5 boots) | footprint 2.10 to 2.15 GB, rss 2.53 to 2.59 GB | |

Memory the guest touched stays charged to the Mac until the VM stops. Stopping and starting the VM is the only thing seen here that gives it back (an 8 s boot).

**Memory, with a balloon driven by the host.** Lima does not let the host set the balloon's target, so a separate small launcher was written for this test. It is about one page of Swift: `VZGenericPlatformConfiguration`, `VZEFIBootLoader`, virtio block, NAT and `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`, ad-hoc signed with the virtualization entitlement. It reads `target <MiB>` lines from a FIFO and sets `targetVirtualMachineMemorySize`. It booted a copy of the same Ubuntu image with 4 vCPUs and 12 GiB. The guest touched 8 GiB (`bytearray(8<<30)` with every 4 KiB page written) and freed it. Each reading was taken after the stated wait:

| step | VM process rss on the Mac | guest `used` | guest `free` |
|---|---|---|
| booted, idle | 1.33 GB | 399 MiB | 11394 MiB |
| holding 8 GiB | 9.69 GB | 8593 MiB | 3200 MiB |
| freed, +15 s | 9.69 GB | 383 MiB | 11410 MiB |
| +60 s, no balloon action | 9.69 GB | 390 MiB | 11403 MiB |
| guest `drop_caches` | 9.70 GB | 309 MiB | 11697 MiB |
| balloon target 2 GiB, +30 s | 11.45 GB | 10571 MiB (the balloon is counted as used) | 1436 MiB |
| balloon target 12 GiB again, +30 s | 11.46 GB | 346 MiB | 11653 MiB |
| the same sequence once more | 11.47 to 11.49 GB throughout | | |

The guest obeyed the balloon: its free memory fell to 1.4 GiB. But the VM process on the Mac did not shrink at any step, and it grew by 1.76 GB while the balloon inflated. In this run, with the traditional balloon and macOS 26.6, the balloon does not return a guest's memory to macOS. The balloon device this launcher gave the guest offered the same feature bits as Lima's (no free page reporting).

### 5.4 Input files: a shared folder against the VM's own disk

The tree is the 46,362-file tree of the earlier record: 46,362 files, sizes cycling from 512 B to 16 KiB, 238 MiB of data. It was made on the Mac's APFS in the shared folder. In the guest, the walk and the read were run 5 times each, and the copy, write and remove 3 times each, and the guest's page cache was dropped before each step, so every guest read goes through virtiofs or the disk. The Mac's own page cache was warm. The Mac's column ran the same commands on the Mac, 3 times.

| step | over virtiofs | on the VM's disk (ext4) | on the Mac (APFS) |
|---|---|---|---|
| walk (`find -type f`), median and range | 2514 (2480 to 2555) ms | 400 (345 to 412) ms | 329 (328 to 398) ms |
| read every byte (`tar cf - . \| wc -c`), median and range | 11212 (11146 to 11305) ms | 2560 (2150 to 2656) ms | 2217 (2200 to 2364) ms |
| copy the tree from the share to the VM's disk (`cp -a`) | 17741, 17582, 17520 ms | | |
| write the tree into the share (`cp -a`) | 22435, 18541, 21077 ms | | |
| remove it from the share | 7632, 7655, 7566 ms | | |

Reading every file through the share takes about 4.4 times as long as from the VM's disk (medians), and walking it about 6 times. The VM's own disk reads the tree about as fast as the Mac's APFS. Copying the tree in once costs about 1.6 reads through the share. These reads ran in the VM, not in a job's container. Reading a shared directory bound into a container was not timed.

### 5.5 Restarts

Each case ran once, with a busybox job (`sleep` of 40 or 60 s, then `echo survived`) started about 6 s before the event. The Mac has no `js-agent` in this run. The process that owns the VM, `limactl hostagent`, stands in for a Mac agent restart.

| event | what happened to the VM | what happened to the job |
|---|---|---|
| R1: `systemctl restart js-agent-crun` inside the VM | nothing | **survived and succeeded**. `systemd-cgls -u jobscheduler.slice`, taken just before the restart, showed the job's `ctr` cgroup with its own supervisor process (`daemon __supervise-run`) beside the agent's. The job's result (`survived`, exit 0) reached the job service after the restart. The agent was `placeable` again after 62.5 s: the server answered its hello `409 host-live` (retries at 0.6, 1.4, 3.0, 5.9 and 10.9 s) until the old session lapsed |
| R2: `kill -9` of `limactl hostagent` | the VM process was gone within 5 s, and Lima listed the instance `Stopped`. The next `limactl start` took 8.0 s | **`lost`**. At its next start the agent removed the job's lease (`its record does not hold: the cgroup is gone`) |
| R3: `limactl stop` (a clean stop request) | the stop took 30.6 s | **`lost`**. The guest's journal for that boot has no entry after the job's start, so the guest's shutdown left no record |

R3, read without proof: the agent's unit stops with `--drain-wait 3600`, so a guest shutdown waits on the running job. The 30.6 s looks like Lima's wait for a clean shutdown, followed by a forced stop. Lima's own log of that stop was rotated by the next start, so this was not confirmed.

So with this launcher, the VM lives exactly as long as one Mac process: `limactl hostagent`. A Mac agent that restarts must not own that process, or every running Linux job is lost. A restart of the agent inside the VM keeps its jobs, as on a Linux node.

## 6. Leftovers and removal

Everything on the Mac is under `<root>`, one folder in the user's home: Lima's binaries, its instance (disk, EFI store, sockets, logs), its download cache, the shared folder and the balloon launcher. Nothing was written to `~/Library`, `/usr/local` or any system location, and no launchd job was registered. Deleting `<root>` after `limactl stop` removes all of it. Lima's port forwarder does listen on the Mac while the VM runs: it forwarded the guest's mDNS port, UDP 5353, on `0.0.0.0`.

## 7. Defects found on the way

- **komira-ai/jobscheduler#1012.** `SubmitJob` accepts a job whose `spec.env` names a variable the agent owns (`HOME` here). The wire refuses it (`invalid-env`), and the dispatcher logs `a dispatched start is not sent` every 10 s. The job stays `placed`, with no end, until a client cancels it. A cancel ended it `cancelled`.
- **komira-ai/jobscheduler#1013.** The `.deb` ships `/usr/lib/tmpfiles.d/js-agent.conf` (mode 0700) and `js-agent-crun.conf` (0711) for the same directories. The 0700 modes were the ones applied (systemd-tmpfiles warned `Duplicate line ... ignoring` for the crun file), and `js-agent-crun` refuses to start until `chmod 0711 /var/lib/js-agent /var/lib/js-agent/scratch`.
- **An agent restart waits for its own old session.** After `systemctl restart js-agent-crun`, the server answered `409 host-live` to the new hello for about 20 s once and 62.5 s once (R1 in section 5.5), until the old session lapsed. Jobs are not placed on the host during that time. This is not filed: it may be the intended lease behaviour.

## 8. Reproducing

Nothing in this document is built or tested by the repository. Its numbers are reproduced by repeating the run on a Mac. The commands are in sections 2 to 4. The measurement scripts are short shell loops over `limactl`, `js` and `ps`. They and their raw output stayed on the Mac with the run and are not committed. The balloon launcher is not committed either: it was a measuring tool, not a proposal.

## 9. Open questions this raises

These are not settled here. Each names the measurement behind it. Where the run had to pick, it picked Docker's shape: one VM, a daemon inside it, a share for files, NAT for network.

1. **What runs inside the VM.** This copies Docker's daemon-in-the-VM shape: a full Linux `js-agent` is the guest's daemon, holding its own session with the job service and registering as its own Linux arm64 host. Is the Mac's Linux side a second host (the default kept here), or a part of the Mac's one agent that drives a guest program over vsock? Measured: the agent in the VM is placeable 8 s after `limactl start`, and the agent, job service and registry together cost 1.2 CPU-s per 120 s of idle (3.04 against 1.89).
2. **The VM's fixed size, and memory that does not come back.** The VM process stayed at 23.2 GB resident (15 GB footprint) after the builds, with the guest using under 1 GB, until it stopped. Neither the guest freeing memory nor a host-driven balloon gave any back (section 5.3). Is the VM sized at a ceiling that is booked away from macOS jobs, or stopped when idle and booted again on demand (8 s; Docker describes its Resource Saver as doing this)?
3. **Who owns the VM on macOS.** Here an ordinary login user owned it, through a per-instance `limactl hostagent` process, and the VM died with that process (section 5.5). Is it owned by a service user, by root under launchd, or by the Mac's agent itself?
4. **Network.** The VM reaches the network through the framework's NAT. Our egress rules run inside the VM (`--egress open` worked as on Linux), and nothing filters on the Mac. Is in-guest filtering enough for a Mac's Linux side, or must the Mac filter too?
5. **Inputs.** A shared folder reads small-file trees 4.5 times slower than the VM's disk (section 5.4). Do inputs come over a share, get copied into the VM's disk, or come from a store inside the VM?
6. **Images.** The agent cannot pull from Docker Hub (token realm) and no arm64 Linux image of ours is published. Where do the Linux arm64 job images come from?
