# One js-agent on a Mac that launches the Linux VM itself and starts crun containers in it

Status: experiment record of a prototype (`js-agent __linux-vm`, `js-vm-guest`). It states what was built, what was run and what was measured. It decides nothing: every choice made to get something running is listed in section 9 with its alternatives, and what could not be measured is in section 10.

The shape tried: a Mac is one host with one js-agent. That agent launches one long-running Linux arm64 VM through Apple's Virtualization framework from its own code, and starts Linux containers in it with nothing of the scheduler in the guest: the guest runs `crun` and, at most, one small program of ours that takes "start this container" over the VM's virtio socket. The earlier record `linux-vm-on-mac-docker-model.md` ran the same idea with Lima as the launcher and a full Linux js-agent in the guest; its numbers are not repeated here.

## 0. The short answer

- **The signed agent launches the VM itself, as root, from a launchd job.** The release's Developer ID signature with one entitlement added, `com.apple.security.virtualization`, verifies, and the Full Disk Access profile's requirement still matches it (the requirement names no entitlement). Nothing was needed beyond the signature: no profile change, no Apple-side step. Start call to the guest program listening: median 3.35 s as root (n=5).
- **A container start from the Mac costs about 2 ms when the VM runs.** Our guest program over the virtio socket: `crun run` of `/bin/true`, from the Mac's connect to the exit back, median 2.29 ms (n=30). The guest's sshd as the channel: 156 ms with a new connection each time, 15.8 ms over one multiplexed connection.
- **The agent's own container hardening runs in the guest unchanged.** The agent's `CrunRuntime` and egress firewall, linked into the guest program, gave a container the job user in a user namespace, `no_new_privs`, the seccomp filter and an allow-list that let one endpoint through and blocked three others that the guest itself reaches.
- **The VM survives an agent restart only when a second process owns it.** When the agent process owns the VM, a restart or an update kills the VM and the running container. When a second process of the same binary owns it (in a process group of its own, as the Linux agent keeps crun), the VM and the running container survive both, and a start that was waiting for the VM to boot completes.
- **Memory comes back only by stopping the VM; the agent can do that.** After a build the VM process held 5.2 GB with the guest using under 1 GB; a balloon made it grow to 13 GB. Stopping the VM after 60 s with no connection gave about 13 GB back to the Mac; stop took 0.2 to 0.35 s and a container that arrives while the VM is stopped runs after about 4.6 s.
- **Tierstore trees through virtiofs (route C) work and cost what virtiofs costs.** The framework refuses to share tierstore's NFS mount point itself, and accepts a placed tree's directory inside it; the guest reads every file under the agent's Full Disk Access, as fast as a virtiofs share of a plain directory (T2 read in 0.85 s, against 0.38 s from the VM's own disk), and a real build from it takes as long as from the VM's disk (32.2 s against 31.9 s). Outputs written into the put ingest's root through a writable share were stored by the Mac's tierstore. Routes A (the guest's NFS mount through the agent's forwarder) and B (a tierstore in the guest) were not measured to the end (6.2).
- **The guest's network reaches the Mac and nothing past it** on these Macs through the framework's NAT (section 2.3).

## 1. Hosts and software

| | Mac A | Mac B |
|---|---|---|
| Hardware | Mac Studio, Apple M3 Ultra, 32 cores, 96 GiB | Mac Studio, Apple M3 Ultra, 28 cores, 96 GiB |
| macOS | 27.0.1 (26A434) | 26.6 |
| How the prototype ran | as root, the signed agent run by launchd through the signed development loop (`js-agent __dev-exec -- /bin/bash <script>`) | as an ordinary user, the same signed binary copied over; another VM (the earlier record's Lima VM) was running |
| Used for | sections 2.1, 4 (root), 6 | sections 3, 4, 5, 7 |

| Piece | What |
|---|---|
| The agent | js-agent from this branch, release build, signed with the team's Developer ID Application certificate: identifier `com.komira.js-agent`, hardened runtime, secure timestamp, entitlements `com.apple.security.virtualization` only |
| Guest image | Ubuntu 24.04 arm64 cloud image `release-20261017` (`ubuntu-24.04-server-cloudimg-arm64.img`, sha256 `50e24e1f…348e`, the digest the earlier record used), converted to a raw disk and extended (sparse) to 64 GiB; its own GRUB and kernel 6.8.0-147-generic |
| Boot | the framework's EFI boot loader (`VZEFIBootLoader`) from the disk; no kernel or initrd outside the image |
| VM | 8 vCPUs, 16 GiB (a fixed size, a flag), NAT network, virtio socket, traditional memory balloon, entropy, a virtio console written to a file, virtiofs shares as asked |
| Guest packages | `crun` 1.14.1, `busybox-static`, `nftables`, `uidmap`, `socat`, `nfs-common`, `fuse3` (Ubuntu's) |
| Guest program | `js-vm-guest` (this branch, `crates/js-agent/tests/bin/vm_guest.rs`), built in the guest: 5.9 MB, glibc |
| tierstore | release 0.3.168, its macOS arm64 and Linux arm64 archives, checked against the release's `SHA256SUMS` |

Where the VM's files live: one directory (`/private/var/js-linux-vm-poc/vm1` as root, a directory of the user's home as a user) holding `disk.raw`, `efi-vars`, `machine-id`, `mac`, `console.log`, `control.sock` and the test SSH key of section 3.2.

## 2. Step 1: the agent launches the VM itself, as root

### 2.1 Signature and boot

The signed loop's checks on Mac A, for the binary with the entitlement:

| Check | Result |
|---|---|
| `codesign --verify --strict` | ok |
| satisfies the privacy profile's `CodeRequirement` (`codesign --verify -R`) | ok |
| designated requirement is the profile's text | ok |
| Developer ID chain, team, hardened runtime, secure timestamp, identifier | ok |
| entitlements | `com.apple.security.virtualization = true` |
| Full Disk Access of the process tree (the agent's own check) | granted |

Boot, measured by the owner's own monotonic clock from the framework's start call to the guest program's ready line on the virtio socket (the guest program sends it once it listens):

| Host | n | median | p10 | p90 | min | max |
|---|---|---|---|---|---|---|
| Mac A, root launchd job | 5 | 3.35 s | 2.47 s | 3.39 s | 2.47 s | 3.39 s |
| Mac B, user | 20 | 4.67 s | 4.05 s | 5.23 s | 4.02 s | 5.25 s |

The process that runs the VM is the framework's XPC service `com.apple.Virtualization.VirtualMachine`, as root on Mac A. `launchctl procinfo` names its responsible process: the agent (`/Library/Application Support/js-agent/bin/js-agent`), and the service carries `com.apple.private.tcc.check-allow-on-responsible-process`, so the privacy checks of what it opens are made against the agent's grant.

### 2.2 The first boot

The first boot ran cloud-init from a NoCloud seed (an ISO made with `hdiutil makehybrid`): host name, the test SSH key, a user for jobs with subordinate ids, the guest program's unit. Its package installation failed, see 2.3.

### 2.3 The NAT network reaches the Mac only

With the framework's NAT attachment the guest got an address by DHCP, reached the Mac (its gateway) and resolved names, and could not open a TCP connection to anything past the Mac (`apt` timed out; a connect to a public address on port 80 timed out), on both Macs, while the Mac itself reached the same addresses. No system setting was looked into or changed. The guest was provisioned through a user-level HTTP proxy on the Mac's NAT address, a scratch tool of the run. The earlier record's guest had network through Lima, whose `vz` driver by default uses its own user-mode network rather than the framework's NAT (as we understand Lima's documents; not checked here).

## 3. Step 2: a container start with nothing of the scheduler in the guest

Measured on Mac B, from the Mac, with the guest running. A sample is the wall time from the client's connect to the reply (or the program's exit).

| Channel | What | n | median | p10 | p90 |
|---|---|---|---|---|---|
| (a) our guest program, virtio socket | `ping` (the channel alone) | 30 | 0.43 ms | 0.36 ms | 0.51 ms |
| (a) | `crun run` of `/bin/true`, exit back | 30 | 2.29 ms | 2.10 ms | 3.64 ms |
| (a) | `crun run` of `/bin/echo`, exit and output back | 30 | 2.06 ms | 1.95 ms | 2.17 ms |
| (b) the guest's sshd, a new connection each time | `crun run` of `/bin/true` | 30 | 156 ms | 148 ms | 163 ms |
| (b) sshd, one multiplexed connection (`ControlMaster`) | `crun run` of `/bin/true` | 30 | 15.8 ms | 12.6 ms | 18.8 ms |

### 3.1 (a) Our smallest program

- In the guest: `crun`, a root file system for the container (here a static busybox), and `js-vm-guest` with a systemd unit. One JSON line in, one out (`crates/js-agent/src/linux_vm/wire.rs`).
- On the Mac: the VM's owner carries a connection of its control socket to the guest's port (`js-agent __linux-vm call`).
- A kill: a `kill` request runs `crun kill <id> <signal>`; the container ended with 137 for `KILL`.
- A wall-time limit: the request carries it and the guest program enforces it (a timer runs `crun kill <id> KILL`); a 2 s limit on `sleep 60` ended it at 2.02 s with `wall_limit_hit`.
- The channel dropped while the container runs: the container ran on, and a later `wait` for its id returned its exit (0) and output.

### 3.2 (b) A stock channel: sshd

- In the guest: sshd (in the cloud image), `crun`, a bundle per command (here one written by `crun spec` and edited), and a key for the Mac (a test key made on the Mac for the run, removed at the end).
- A kill: a second ssh connection runs `crun kill`; the container ended with 137.
- A wall-time limit: nothing does it; it would be a command such as `timeout` in the remote command line, or a kill sent by the Mac.
- The channel dropped while the container runs: the container ran on (`crun list` shows it running); nobody holds its exit, since its parent was the dropped session.

### 3.3 What Docker Desktop has in its VM

From Docker's documents: Docker Desktop runs a LinuxKit VM that Docker manages, the Linux containers run in it, and "the heart of this is still the Docker Engine" (the engine runs inside the VM; the docs read did not say how the CLI's socket is carried). On a Mac the VM is run through Apple's Virtualization framework (or Docker's own VMM). Its Resource Saver stops the VM when no container runs, and a command that needs it waits "about 3 to 10 seconds" while the VM starts again.

## 4. Step 3: the Linux agent's container hardening in the guest

On a Linux node the agent builds, per container: the OCI config and bundle, the cgroup under the runtime's root, the mounts (image root, scratch, lease and temporary directories), the user-namespace mapping through `newuidmap`, the seccomp filter and the first process (`js-init`), and the egress: a veth with a /30 per container, the nftables tables `js_crun_egress` and `js_crun_egress_nat` and the allow-list set. All of that acts on the kernel that runs the container, so all of it must run in the guest: `js_agent::crun::CrunRuntime` (stage and start), `js_agent::egress::Egress` (the firewall), and `js_contain`'s helper modes (the binary that drives crun must answer them). The guest program links the agent's library for this and opens the runtime as the agent does (`crates/js-agent/tests/runtime_host`). Nothing of the scheduler (server connection, leases, heartbeats) is needed there.

One container started that way from the Mac (`hardened` request), allow-list `[the Mac at the gateway, port 22]`, Mac B:

| Destination | From the guest itself (no container) | From the container |
|---|---|---|
| the Mac, port 22 (allowed) | reached | reached |
| the Mac, port 3128 (a listener of the run) | reached | not reached |
| the guest's own address, port 9300 | reached | not reached |
| a public address, port 443 | not reached (2.3) | not reached |

In the container: `uid=1000 gid=1000`, `uid_map 0 100000 65536`, `NoNewPrivs: 1`, `Seccomp: 2`, and its own `eth0` with the runtime's per-container /30. The run took 3.4 s in the guest (stage, firewall, start, exit).

The runtime's nftables tables were left in the guest after the runtime and its egress were dropped; the agent does not remove them at the end of its life either (on a node they stay for the next start).

## 5. Step 4: the VM across an agent restart

The agent's launchd job was restarted (`launchctl kickstart -k`), and separately its binary was replaced as an update does (a new file renamed over the old) and the job restarted. Each case was run with a container running (`sleep 45`), and with a container start waiting for the VM to boot. Mac A as root (system domain) and Mac B as a user (the user's GUI domain) gave the same results.

| Owner of the VM | Event | The VM | The running container | A start waiting for the VM |
|---|---|---|---|---|
| A. the agent process (`__linux-vm run` is the job's program) | job restarted | killed, booted again by the new process (7 to 8 s) | lost: its client got the connection closed with no reply | failed: the connection closed |
| A | binary replaced, then restarted | the same | lost | failed |
| B. a second process of the same binary in its own process group (`__linux-vm adopt` starts it with `--detach` and adopts it after a restart) | job restarted | kept: the same owner process and VM | kept: it finished and its exit (0, `done`) reached its client | — |
| B | binary replaced, then restarted | kept: the owner runs on the replaced file's old inode | kept | completed: it ran after the boot (2.7 s on Mac A, 4.2 s on Mac B) |

In B the new agent finds the owner by its control socket (`state`) in 0 ms. The client that held the run in these trials was a separate process, not the agent; in the agent, the run would be held by the agent's own connection, which a restart drops: the guest program keeps a run whose channel dropped and answers a later `wait` for it (3.1), which is how a new agent would read its exit.

## 6. Step 5: tierstore trees inside the VM

Mac A as root, the VM at 8 vCPUs and 16 GiB. A throwaway tierstore 0.3.168 on the Mac (its macOS release build): S3 front and mount provider on loopback, its NFSv3 loopback mount made by `tierstore fs`, a cold store in a local directory, a put ingest root, and a test key made for the run. Two trees, put into it and placed by its mount provider through the agent's own provider client (`js-agent __linux-vm place`): T1, the earlier record's tree (46,362 files in 7,845 directories, 512 B to 16 KiB, 277 MB), and T2, this repository's source at the branch (4,003 files, 54 MB), the input of a real build.

Each row is the same script in the guest (`measure.sh`, kept with the run's scripts): n=5 runs, each after dropping the guest's page cache: the first byte of a file not read before (`head -c1`, which includes starting `head`: about 2.5 ms on any route), every file listed (`find -type f`), every byte read (`tar cf - . | wc -c`). Run 1 is the coldest; the median is of all five. Then one `cargo build --release --locked --offline -p js-agent` with T2 as the workspace (the registry and the toolchain on the VM's disk, the target on the VM's disk, made empty first).

| Route | Tree | first byte, run 1 / median | walk, run 1 / median | read all, run 1 / median | build of T2 |
|---|---|---|---|---|---|
| The VM's own disk (the tree copied in: 15.4 s for both) | T1 | 3.2 / 3.8 ms | 498 / 477 ms | 4.80 / 4.57 s | |
| | T2 | 4.0 / 4.0 ms | 47 / 47 ms | 0.41 / 0.38 s | 31.9 s |
| C. tierstore through virtiofs: the placed tree's directory on the Mac's NFS mount, shared into the VM | T1 | 4.5 / 2.6 ms | 1,975 / 1,983 ms | 17.0 / 9.44 s | |
| | T2 | 3.3 / 2.5 ms | 133 / 134 ms | 1.54 / 0.85 s | 32.2 s |
| virtiofs of a plain directory of the Mac (the same trees, local disk) | T1 | 3.0 / 2.7 ms | 2,261 / 2,259 ms | 11.2 / 9.73 s | |
| | T2 | 2.8 / 2.6 ms | 148 / 147 ms | 0.86 / 0.85 s | 31.8 s |
| A. the guest's NFSv3 mount of tierstore through the agent's forwarded port (Mac B, user; see 6.2) | T1 | not measured (6.2) | | | |
| | T2 | not measured by the script; by hand: | 4.2 s (first walk, see 6.2) / 95 ms (n=3) | 2.2 s (n=1, warm on the Mac) | not measured |
| B. a second tierstore in the guest, its own FUSE mount, upstream the Mac's S3 front | T1, T2 | not measured (6.2) | | | |
| A read-only block image of the tree (ext4, made in the guest from the copy, loop-mounted; what a per-tree block device would give, without a device of its own) | T1 | not measured: the image made with `mke2fs -d` ran out of inodes (the size gave too few); not repeated | | | |
| | T2 | 5.3 / 5.1 ms | 85 / 83 ms | 0.32 / 0.31 s | 32.0 s |

The Mac's own reads of the same trees, for comparison (root, n=10 over two runs; `purge` before each):

| On the Mac | Tree | walk, run 1 / median | read all, run 1 / median |
|---|---|---|---|
| tierstore's NFS mount | T1 | 1,428 / 382 ms | 8.75 / 6.40 s |
| | T2 | 191 / 56 ms | 0.93 / 0.79 s |
| the local copy | T1 | 330 / 300 ms | 7.18 / 7.12 s |
| | T2 | 38 / 36 ms | 0.72 / 0.71 s |

### 6.1 Route C: does the guest see the files

- **The mount point cannot be shared.** A share of tierstore's NFS mount point (`fs.mountpoint`) is refused by the framework at the VM's start: "Invalid virtual machine configuration. A directory sharing device configuration is invalid." A share of a placed tree's directory inside that mount (`<mountpoint>/mount-provider/<agent>/<run>/<index>`) is accepted, and the guest reads every file of it.
- **Who reads.** The framework's XPC service `com.apple.Virtualization.VirtualMachine` serves the share, as root. Its responsible process is the agent, and it carries `com.apple.private.tcc.check-allow-on-responsible-process`, so its reads of the network volume are judged against the agent's Full Disk Access, which the profile grants; the guest read every file of both trees with no refusal.
- **One tierstore, one cache, NFS on the Mac's loopback only.** What crosses the VM boundary is the virtiofs protocol (file operations), served from the Mac's NFS client.
- **A share is fixed when the VM starts** in this prototype: a tree placed after the start needs a share added to the running VM. The framework lets the share of a running VM's virtiofs device be replaced (a multiple-directory share); that was not tried.

### 6.2 Route A and route B

**Route A.** One tierstore and one cache, on the Mac; what crosses the VM boundary is NFSv3 (ONC RPC) carried over the virtio socket: the guest's NFS client connects to a TCP listener in the guest (`socat`), which connects to a virtio socket port of the host, which the VM's owner (the agent's code, `--forward 2049=127.0.0.1:<nfs port>`) connects to tierstore's loopback listener. Tierstore's listen rule is unchanged: it still listens on the Mac's IPv4 loopback only. The mount works (Mac A as root and Mac B as a user; NFSv3, `nolock`, read-only). Measured by hand on Mac B for T2: walks of 95 ms (three, warm) and every byte read in 2.2 s (one).

- With the guest's forwarder accepting without `TCP_NODELAY`, the route was not usable: in 90 minutes the measurement of T1 did not finish one run, while tierstore's NFS server stayed busy. With `nodelay` the hand measurements above.
- The first walk of the fresh mount listed 449,141 entries for T2's 4,003 files (4.2 s); the next walks listed 4,003. Not looked into.
- Where it stops: the measurement script (`measure.sh`, which syncs and drops the guest's page cache between runs) ended the guest program's connection without a reply on every try over this mount (three tries, Mac A and Mac B); the cause was not found in the time of the run, so route A has no row of the same measurements as the others.

**Route B.** Two tierstores and two caches: the Mac's, and one in the guest (the Linux arm64 release build, as on a Linux node) with its own FUSE mount, its upstream the Mac's S3 front reached through a forwarded virtio socket port, its keys a test key made in the guest and the Mac trial's test key for the upstream. The guest's tierstore started and its FUSE mount came up (Mac A). Where it stops: the lookup of a tree (`<mount>/trees/<digest>`) answered `Input/output error`, in the run where the upstream forwarder had not started (a fault of the run's script), and the runs after it did not get past route A's failure above. So route B was not measured.

### 6.3 Outputs back to the Mac's tierstore

The put ingest's root on the Mac was shared into the guest writable. The guest committed a file as a job does on Linux (`pack.part`, renamed to `pack.sha256-<hex>.part`, renamed to `pack`, under `cas/b/<build>/<nonce>/`). On the Mac the files are written by the framework's process (root; group `_tierstore` inherited from the directory). The ingest saw it and took it: "the put ingest stored cas/b/vmpoc/n…/pack", and the object was in the cold store 15 s later. The same share works for every route, since outputs do not go through the input path.


## 7. Step 6: memory

Mac B, the VM launched by our code at a fixed 8 vCPUs and 16 GiB. The VM process is the framework's XPC service; its footprint is `footprint -p`'s `phys_footprint`.

| Moment | VM process footprint (RSS) | Mac's free memory |
|---|---|---|
| booted | 897 MB (1,144 MiB) | 44.4 GB |
| idle 30 s | 900 MB (1,151 MiB) | 44.3 GB |
| after a build (`cargo build --release --locked --workspace` of this repository, 79 s) | 5,222 MB (5,929 MiB) | 40.1 GB |
| after the guest freed memory (`drop_caches`, `compact_memory`; guest used 856 MiB) | 5,223 MB (5,947 MiB) | 40.1 GB |
| balloon target 4,096 MiB, after 30 s (the guest gave up the pages: used 12,687 MiB with the balloon) | 13 GB (14,039 MiB) | 31.9 GB |
| balloon target back to 16,384 MiB | 13 GB (14,039 MiB) | 32.0 GB |
| the VM stopped by the owner after 60 s with no connection | the process is gone | 45.2 GB |

So the balloon did not give memory back: inflating it made the guest touch the pages it handed over, and the VM process grew by 8 GB. Only stopping the VM gave the memory back.

Stop and start, the owner's own clock and the Mac's clock (`js-agent __linux-vm call` round trips):

| What | n | median | p10 | p90 |
|---|---|---|---|---|
| stop, through the guest program's `poweroff` (sync, then power off) | 6 | 0.20 s | 0.19 s | 0.35 s |
| stop, through the framework's power button only, the guest up a minute or more | 3 | 0.60 s (values 0.48, 0.60, 3.60) | | |
| stop, through the power button only, the guest a few seconds after its boot | 15 | 30.2 s, every one between 30.2 and 30.4 s: the guest went into Ubuntu's shutdown and was stopped at the 30 s grace | | |
| a container start that arrives while the VM is stopped (boot + start + exit) | 4 | 4.56 s | 4.32 s | 4.90 s |
| ... while the VM is starting (the start asked 1 s before) | 3 | 4.09 s | 3.83 s | 4.21 s |
| ... while the VM is stopping (guest program `poweroff`) | 3 | 4.71 s | 4.55 s | 5.28 s |
| ... while the VM is stopping (power button only, just booted) | 3 | 35.5 s | 34.2 s | 35.5 s |

A start that arrives while the VM is stopped, starting or stopping is accepted and waits; none was refused. The idle stop came 61 s after the last connection ended with `--idle-stop-s 60`.

## 8. Step 7: the CPU a container sees

Mac A and Mac B (M3 Ultra), the same in the guest and in a container: 8 CPUs (as given), MIDR `0x610f0000` (implementer Apple), `AT_HWCAP` `0xefb3ffff`, `AT_HWCAP2` `0x126181`, features:

`fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 asimddp sha512 asimdfhm dit uscat ilrcpc flagm sb paca pacg dcpodp flagm2 frint i8mm bf16 bti afp`

That is the Armv8.6-A level (`i8mm`, `bf16` and the v8.5 `flagm2`, `frint`, `bti`, `sb`) with the crypto extensions, and no SVE or SME. A linux/arm64 ISA level named for a Mac would be v8.6-A without SVE.

## 9. Choices made to get it running, and their alternatives

| Choice | Taken | Alternatives |
|---|---|---|
| Where the VM code lives | a development verb of js-agent (`src/linux_vm`, macOS only), so the signed agent with the entitlement is what launches it | the existing VM helper `js-vmm` (macOS guests only today, a second signed program); a helper program of its own |
| Guest image and boot | Ubuntu's arm64 cloud image as a raw disk, booted by the framework's EFI loader with the image's own kernel | a kernel and initrd outside the image (`VZLinuxBootLoader`), which boots without GRUB; a minimal image of our own (LinuxKit-like, as Docker); Fedora or Debian cloud images |
| Provisioning | cloud-init from a NoCloud ISO the run made with `hdiutil`, then a provisioned disk copied to the second Mac | an image built once and shipped; the agent writing the guest's files itself through a share at first boot |
| The guest's network | the framework's NAT; packages through a scratch proxy on the Mac (2.3) | a user-mode network in the agent (as Lima and Docker do); a bridged attachment (needs an entitlement and a profile); no network for jobs at all |
| Ready signal | the guest program, once it listens, sends one line to a host virtio socket port | a unit ordered after the program (raced its listen: one start refused, fixed); the owner polling the port |
| Channel for "start this container" | one JSON line each way over the virtio socket, one connection per request | gRPC over the virtio socket; sshd (measured, 3b); a long-lived multiplexed connection |
| Container start in the plain path | `crun run` of a bundle written per request from one image root, root in the guest, new pid/ipc/uts/mount/net namespaces | `crun create`/`start` with a detached supervisor; the agent's full runtime for every container (the hardened path, 4) |
| Where the wall-time limit lives | in the guest program (a timer and `crun kill`) | on the Mac, as a `kill` request; both |
| A run whose channel drops | keeps running; its exit is kept for `wait` | killed on drop |
| Owning the VM across a restart | both built: the agent process, or a second process of the same binary in its own process group that the agent adopts by its control socket | a launchd job of its own for the VM; the framework's save and restore of the VM's state across the restart |
| Stopping the VM | the owner asks the guest program to sync and power off, then falls back to the framework's power button, then a force stop after 30 s | the power button alone (30 s on a fresh guest); a force stop after a sync; pausing instead of stopping (keeps the memory) |
| Idle stop | `--idle-stop-s N` (60 in the run): no connection open for N seconds | Docker's default of 5 minutes; stopping only under the Mac's memory pressure |
| VM size | fixed per VM (flags): 8 vCPUs, 16 GiB | sized from the Mac (Docker's default is half the memory); grown by restarting the VM at another size |
| Placing a tierstore tree on the Mac | `js-agent __linux-vm place`, the agent's own mount provider client (`Prepare`, `Mount`) | the provider's protocol from a separate tool |
| Disk image caching | the framework's defaults (`initWithURL:readOnly:`) | `VZDiskImageCachingMode` cached or uncached, other synchronization modes |
| Measuring reads | `find | wc -l` and `tar cf - . | wc -c` after dropping the guest's page cache, as the earlier record | a program of ours that reads with a fixed buffer and many threads |


## 10. What could not be measured, and why

- Routes A and B of the tree measurements (6.2), and the block-image row of T1.
- A share added to a running VM (for a tree placed after the VM started): the framework allows replacing a running virtiofs device's share; not tried.
- The plain-path container start was measured on the user-run Mac only (Mac B); the root-run Mac (Mac A) ran the boots, the restarts and the tree routes. Mac B also ran another VM during the measurements.
- The guest had no network past the Mac (2.3), so jobs that need egress, and an image pull from a registry, were not tried in the guest.
- The idle stop and the memory numbers are of one size (8 vCPUs, 16 GiB) and one build; no other size was tried.
- The hardened path ran one container with one allow-list; its start latency was not sampled.
- A container started from the Mac by the hardened path held the guest program's connection; a restart of the agent while such a run was in flight was tried only on the plain path.
- An agent update arriving while a job waits for the VM was tried with the job's request held by a separate client process, not by the agent itself (5).
- macOS's own TCC prompt or refusal for the share of a network volume could only be read from what the guest saw (6); the system's TCC log was not read.


## 11. What the run left on the Macs

- Mac B: nothing of this run (its directory, the scratch proxy, the throwaway tierstore and the user launchd jobs were removed; the earlier record's VM there is not this run's).
- Mac A: the run's user files were removed. Two root-owned directories remain until a root step removes them (a script for it is kept with the run's scripts): the VM's directory (its disk, about 17 GB allocated, and the test key) and the throwaway tierstore's directory (stopped and unmounted). No process of the run is left.
