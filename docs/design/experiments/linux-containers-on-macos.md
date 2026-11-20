# Linux containers on a Mac: one of our job specs, measured against the Linux job contract

Status: experiment record. It states what was run, what was measured and what did and did not map. It decides nothing: which runtime, if any, a Mac uses for Linux jobs is a later design.

## What was asked and the short answer

Can a Mac Studio run one of our Linux job specs as a Linux container, from a user account without `sudo`, and how close does that come to the contract the Linux `js-agent` gives a job?

- Apple's `container` tool (release 1.7.1) runs Linux arm64 containers on the Mac **without `sudo` and without installing anything**: its signed installer package was expanded into a user directory and started from there. The account was in the `admin` group; a standard account was not tried.
- Our busybox job shape ran by hand with its limits, user and network level. Most contract items map. These do not, as shipped:
  - the job's hardening inside the guest: no user namespace, no seccomp, `no_new_privs` off, `/tmp` not `noexec`, and a writable cgroup tree;
  - egress filtering: there is none in the tool, and the host itself stays reachable on the host-only network;
  - file ownership on shared directories;
  - memory: a running VM does not give back to macOS the memory its guest frees.
- Request to first process is about **0.6 s** at the median, cold or warm, small image or build-sized image. This is measured from spawning the CLI to the job's first stdout byte. About 0.06 s of it is making the container, and 0.54 s is booting its VM.
  - The nearest Linux figure is 26.8 ms (`docs/design/agent-egress.md`): `crun` start to the job's first byte, excluding staging, pull and unpack.
  - The start target is 100 ms at the median, from placement to a running container (`docs/design/start-latency.md`).
  - Neither figure spans exactly the same interval as the Mac one.
- x86_64 through Rosetta could not be measured on this host: Rosetta is not installed and installing it needs administrator rights.

## 1. Host and software

| | |
|---|---|
| Host | Mac Studio, Apple M3 Ultra, 28 cores, 96 GiB, SIP on, `kern.hv_support` 1 |
| macOS | 26.4, build 25E246 (other macOS builds may behave differently; nothing here was repeated on another build) |
| Account | an ordinary user, member of `admin`, with a console login; no `sudo` was used |
| Tool | `container` 1.7.1 (`container-1.7.1-installer-signed.pkg`, sha256 `b6739caf3d47ca53a78f1a7edf32521420a75c7941838c3cefa75f30b80f5a97`, as listed by the GitHub release) |
| Kernel | kata-containers 4.3.1 `kata-static-4.3.1-arm64.tar.zst`, sha256 `2c5bf7cde334ad5307d482f45cceda496f91c9b6b96f5278c47f4f4eac1bbae6`, member `opt/kata/share/kata-containers/vmlinux-6.18.56-208-debug` (sha256 `98a1005f8cc395e99b85775b9dce2b239190520b102bf549f4225104dcd7f511`). This is the URL, digest and member the tool's own defaults name. |
| Guest init | `ghcr.io/apple/containerization/vminit:0.54.0`, index `sha256:ab76bf71fbedcbf67a54dc5f37b0acefd68b1ba090d93f752f8e27c9961a685c`, pulled by the tool on first run (66 MB) |
| Images | busybox 1.37.0 linux/arm64 `docker.io/library/busybox@sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c` (the pin in `crates/xtask/src/crun_tests.rs`, 1.9 MB); alpine 3.22 (4.1 MB) as a second small image; `docker.io/library/rust@sha256:7dabf6d6fc774b4a25df2c10bd97cd9dfd444d1e3667bb607d29b31b4cd54d8c` (`rust:1-bookworm`, linux/arm64, 529 MB compressed, about 1.5 GB unpacked) as the build-sized image |

Each file was downloaded with `curl` and checked with `shasum -a 256` against the release's listed digest before use. `pkgutil --check-signature` reports the package as signed by "Developer ID Installer: Apple Inc. - Containerization" and notarized.

## 2. Running it without administrator rights

The package is a component package: identifier `com.apple.container-installer`, `install-location` `/usr/local`, `auth="root"`, 50 files, about 466 MiB installed, no `Scripts` and no `Distribution`. Installing it needs root. Running it does not:

```sh
pkgutil --expand-full container-1.7.1-installer-signed.pkg pkgx     # no install, no root
R=$PWD/pkgx/Payload
$R/bin/container system start --app-root $PWD/app --install-root $R \
    --log-root $PWD/logs --disable-kernel-install                     # 2.9 s
$R/bin/container system kernel set \
    --tar $PWD/kata-static-4.3.1-arm64.tar.zst \
    --binary opt/kata/share/kata-containers/vmlinux-6.18.56-208-debug  # from the checked local file
$R/bin/container run --rm docker.io/library/alpine:3.22 uname -a
```

- `system start` registers launchd agents in the user's own domain: `stop` names them `user/<uid>/com.apple.container.apiserver`, `…container-network-vmnet.default`, `…container-core-images`, `…machine-apiserver` and others. The plist allows the session types `Aqua`, `Background` and `System`. Every call above was made over SSH, which is a `Background` session, while the same user also had a console login.
- Each container runs in its own VM. The processes are `container-runtime-linux` (one per container, carrying only the `com.apple.security.virtualization` entitlement in Apple's signature) and the system's `com.apple.Virtualization.VirtualMachine` XPC service, all as the user.
- Every file the tool writes is under the three roots given (`--app-root`, `--install-root`, `--log-root`). Nothing appeared under `~/Library` or `~/.config`.
- No second runtime was tried, for example Lima or krunkit over the Virtualization or Hypervisor framework. A second runtime was the fallback if Apple's tool had needed administrator rights, and it did not.

## 3. The job

No job-spec file (JSON, YAML or TOML) was found in the repository's tests, conformance suite or docs; specs exist as Rust values in tests. The two specs closest to a real Linux job are the real-`crun` busybox job (`crates/js-contain/tests/crun/job.rs`, image pinned in `crates/xtask/src/crun_tests.rs`) and the agent-level `RunSpec` of `crates/js-agent/tests/live_podman.rs`. The run here used the latter's shape with the former's arm64 image:

| `RunSpec` field (`live_podman.rs`) | Value | `container run` flag used |
|---|---|---|
| `body` | the image, `sh -c SCRIPT` (the test's script, unchanged) | image by manifest digest, then `sh -c "$SCRIPT"` |
| `cpu_millis` | 1000 | `--cpus 1` |
| `memory_bytes` | 256 MiB | `--memory 256M` |
| `pids` | 64 | none: the tool has no pids flag, `pids.max` stays `max` |
| slot 0 user | 1000:1000 | `--user 1000:1000` |
| `network` | default (`none`) | `--network none` |
| `scratch` | read-write at `/scratch` | `-v <host dir>:/scratch` |
| image root | read-only | `--read-only` |
| `/tmp` | writable, `noexec,nosuid,nodev` | `--tmpfs /tmp` (no flag sets mount options; mounted `rw,relatime`) |
| `isolation` | `Contained` | not applicable: a VM per container |

It ran in 2.2 s, including the first pull of busybox. The runtime's init image was already present by then: the 6.2 s first run in section 6 was an earlier alpine run that also pulled it. The script's probes gave the following. The last three rows are not probed by that test: their Linux column comes from the hardening in `crates/js-contain/src/oci/`, and their Mac column from the same run's `/proc/self/status` and mountinfo.

| Probe | Linux contract | Mac |
|---|---|---|
| `uid_map` | a subordinate range, not the identity | `0 0 4294967295`: no user namespace |
| uid and gid | 1000 | 1000 |
| write under `/usr` | refused, read-only file system | refused, read-only file system |
| copy into `/tmp` | allowed | allowed |
| execute from `/tmp` | refused (`noexec`), exit 126 | **ran** (exit 127 is busybox's "applet not found", printed by the copied binary) |
| network interfaces | `lo` only | `lo` only |
| capabilities | all empty | effective set empty for uid 1000; bounding set `a80425fb` |
| `NoNewPrivs`, `Seccomp` | 1, filter | 0, 0 |
| `/sys/fs/cgroup` | read-only | read-write |

## 4. The contract, item by item

The meaning of each item is the Linux agent's: `crates/js-agent/src/crun`, `crates/js-contain` and `docs/design/agent-egress.md`.

| Item | Linux agent | Mac with `container` 1.7.1 | Mapped |
|---|---|---|---|
| Image as a clean root per job | digest-verified pull, safe unpack once per digest, a read-only image tree plus a per-job overlay upper discarded with the lease | pull by digest; unpacked once per image into an ext4 snapshot; each container gets its own `rootfs.ext4` made from it (61 ms median for the 1.5 GB image, below); a write by one job was not visible to the next | yes for a clean root; the unpack's guarantees (whiteouts, no setuid, path confinement) are the tool's, not ours, and were not tested |
| Input read-only | read-only recursive bind; a read-only mount cannot be written through any path | virtiofs share with `readonly`: writes refused with `EROFS`, for root too; `mount -o remount,rw` refused (no `CAP_SYS_ADMIN`) | yes |
| Output writable | `/scratch` rw, owned by the job's id | virtiofs share rw; files appear on the host owned by the user running the tool | yes, with the ownership caveat below |
| File ownership | host ids mapped by the user namespace; mode 0 refused; unowned files not writable | every file on a share appears owned by **whichever uid looks**: uid 1000 sees `1000:1000`, root sees `0:0`; `chown` inside succeeds and changes nothing on the host; a mode-0 file is unreadable to both | partly: permission bits are enforced, ownership is not modelled |
| CPU limit | `cpu.max` from `cpu_millis` | the VM gets `cpus + 1` vCPUs (`nproc` 2 at `--cpus 1`); the guest cgroup's `cpu.max` is `cpus × 100000 100000`; 4 busy loops used 4.0 s of host CPU per 4 s at `--cpus 1` and 16.0 s per 4 s at `--cpus 4` (the VM process's CPU time from `ps`) | yes |
| Memory limit | `memory.max` = booking, `memory.swap.max` 0, OOM kill reported as 137 with `oom_killed` | guest cgroup `memory.max` = the booking exactly; no swap; VM size about booking + 95 MiB (`free` shows 351 MiB at 256 MiB); a 400 MB allocation exits 137 and `memory.events` shows `oom_kill 1`; guest **root** can raise its own `memory.max` (a 300 MB allocation then passed, a 500 MB one was still killed: the VM size bounds it); uid 1000 cannot | yes for a non-root job; for a root job the VM size is the real cap |
| Usage read | `cpu.stat`, `memory.peak`, `memory.events`, pressure files | inside: all of these, in the guest cgroup; outside: `container stats --format json` gives `cpuUsageUsec`, `memoryUsageBytes`, `memoryLimitBytes`, block and network bytes, `numProcesses`, with no peak | yes inside; outside has no peak |
| Memory back to macOS | not applicable | one sample: a running VM that touched 1.5 GB kept a host footprint of 1816 MB (from `footprint`) 10 s after the guest freed it (guest use 5 MB); the memory goes back when the VM ends with its container | **no** while the VM lives: host memory is the VM's high-water mark |
| Egress `none` | new netns, loopback only | `--network none`: loopback only | yes |
| Egress allow-list, enforced outside | nft forward chain: resolvers, listed IPv4:port, then reject; host input rejected; metadata, IPv6 and spoofing dropped | no filter in the tool (details below the table) | **no** as shipped |
| Root and non-root user | contained jobs run as 1000 in a user namespace; host jobs as root | default user is root (the guest's real root, no user namespace, bounding set `a80425fb`); `--user 1000:1000` runs as 1000 with an empty effective set | partly: the identities map, the hardening differs (section 3) |
| Exit status | code; signal as 128+n (`crates/js-contain/init/init.rs`); 127 not found, 126 not runnable | codes 0, 1, 3 and 255 passed through. With `--init` (the tool's `/.cz-init`, PID 1): SIGKILL 137, SIGTERM 143, SIGSEGV 139. Without `--init` the job is PID 1, and a signal it sends itself is ignored (exit 0). Program not found: exit 1, with a message. Not executable: exit 1, with a message | yes with `--init`, except 127/126 both being 1 |
| stdout and stderr | separate, bounded capture, never blocks | separate streams from the CLI; 64 MiB of stdout in 0.91 s; also kept in `stdio.log` per container | yes; bounds are the caller's |
| Cancellation | SIGTERM, grace, then `cgroup.kill` of the whole lease | `container stop --signal SIGTERM --time <s>`: a trapping job got TERM and ended in 1.15 s; a job ignoring TERM was killed at the grace (3.34 s for 3 s); `container kill` ended a run in 0.23 s and the attached `run` exited 137 | yes |
| Process tree | `cgroup.kill` kills everything; integrity check finds daemonised children | the VM ends when the job's process ends: a double-forked `setsid` child did not keep the container running | yes |
| Agent death | supervisors survive and are adopted | killing the `container run` process left the container running; it was found by name and removed with `rm -f` (re-attaching to it was not tried) | partly: it survives; adoption not tested |
| No leftovers | lease dir, mounts, cgroup, rules removed; integrity check | after `run --rm`: no container listed, `containers/` empty, no VM process, a 500 MB write inside left no disk use. After `system stop`: no tool process, no launchd job, no extra bridge. Images stay cached (`system df`: 4 images, 2.58 GB: alpine, busybox, rust and the init image) | yes |

Egress, in detail:
- **Default network (vmnet NAT).** The guest reached the internet, the LAN gateway, the host's LAN address and the host's SSH port.
- **Host-only network (`network create --internal`).** The internet and the LAN were dropped (connections timed out after 4 s). **Every listener on the host stayed reachable**, both on the bridge address and on the host's LAN address.
- **A relay as the allow-list.** A one-destination TCP relay, run by the same user on the host's bridge address, worked: an HTTP request through it got the upstream's `301`, while a direct connection to the same upstream timed out. The job must dial the relay's address, not the real one.
- **The guest cannot rewire itself.** Guest root could not change its address or routes (`RTNETLINK answers: Operation not permitted`).
- **IPv6.** There was no IPv6 route in either mode.
- **Not tried.** Packet filtering on the host bridge (pf) needs root.

Other observations:
- The hostname is the container id, not `localhost`. `HOME=/root`. Stdin is `/dev/null`. RLIMIT_CORE is 0 and RLIMIT_NOFILE 1048576. `/proc/sys` is read-only.
- A pull by tag fetched every platform of the index (alpine: amd64 and arm64). A pull by platform manifest digest stored that one platform.
- The tool's defaults are 4 CPUs and 1 GiB per container.

## 5. Input trees of tens of thousands of files

The reference tree is the 46,362-file build-input tree of `docs/design/job-rootfs.md`. Here it was 46,362 files in 7,845 directories, sizes cycling from 512 B to 16 KiB, 238 MiB in total, on APFS. It was shared read-only into one container (`--cpus 4 --memory 4G`). Each guest step was run 5 times in that one container, and the host reference 3 times. The table shows the median, with the range in brackets. Guest times are busybox `time` inside the guest; host times are `/usr/bin/time -p`.

| Step | Guest, over virtiofs | Host, same tree on APFS |
|---|---|---|
| walk (`find -type f`) | 1.48 s (1.39–1.96) | 0.33 s (0.32–0.36) |
| read every byte (`tar cf - \| wc -c`) | 8.56 s (8.40–8.91) | 2.31 s (2.27–2.41) |
| copy the tree into the VM's own disk | 8.78 s (8.68–9.73) | |
| walk the copy on the VM's disk | 0.09 s (0.05–0.10) | |
| write the tree's 46,362 files to a read-write share | 26.25 s (23.82–27.45) | |
| remove them from the share | 6.69 s (6.53–7.09) | |

The host's page cache was warm for every guest run. A one-time copy into the VM's disk costs about the same as one full read, after which the guest's walks are faster than the host's.

## 6. Start latency

**Method.** A Python harness on the Mac records `time.perf_counter_ns()` before spawning `container run --rm --progress none …` and again when the first byte of the container's stdout arrives. The job's first act is `printf R`, so the interval is the request to the first process running, and it includes the CLI's own start and the stdout relay. The end of the run (the CLI's exit) is reported too. **Warm** means the tool's services were already running and the image already unpacked. **Cold** means `container system stop` then `container system start` before each sample, with the clock started after `start` returned. That is the first container after the runtime's boot, with the image unpacked. Each set ran 20 samples in a row on an otherwise lightly loaded host (load average about 2 on 28 cores). Times are in ms, with the 10th to 90th percentile in brackets:

| Image | Mode | First byte, median | (p10–p90) | min–max | Exit, median |
|---|---|---|---|---|---|
| alpine 3.22 (4 MB) | warm | 605 | (560–621) | 556–715 | 712 |
| alpine 3.22 | cold | 611 | (589–645) | 580–747 | 722 |
| rust 1-bookworm (1.5 GB) | warm | 599 | (560–613) | 496–614 | 703 |
| rust 1-bookworm | cold | 609 | (592–627) | 563–632 | 721 |
| busybox, the job's flags (`--cpus 1 --memory 256M --user 1000:1000 --network none --init`) | warm | 559 | (526–582) | 504–586 | 576 |

Split, 20 samples with the rust image (`container create`, then `container start --attach` to the first byte):

| Step | Median | (p10–p90) |
|---|---|---|
| `create` (the container's root made from the image's snapshot) | 61 ms | (57–65) |
| `start` to first byte (VM boot, guest init, process start) | 540 ms | (506–552) |

**One-off costs.** These were single samples, not 20:
- The first `container run` after setup took 6.2 s. That covered the alpine pull, the 66 MB init-image pull and the unpacks.
- Pulling and unpacking the rust image took 10.8 s, and its first run afterwards 0.70 s.

**Rosetta (x86_64).** Not measured. `container run --rosetta --platform linux/amd64 …` failed with `VZErrorDomain Code=2 "Rosetta is not installed"`. Installing Rosetta needs administrator rights, which this experiment did not use. The tool itself supports it: there is a `--rosetta` flag, and the `build` defaults have `rosetta = true`.

## 7. What `js-agent` on macOS would call, and what must be installed

**The interface.**
- The CLI: `container create/start/stop/kill/rm/stats/inspect`, with `--format json` where offered. It talks over XPC to the per-user `container-apiserver`.
- The library under it: the Swift `Containerization` package. Its releases are pre-1.0 (0.33.3 latest), and there is no C or Rust interface.
- No OCI runtime binary and no bundle path. `crun` and `js-contain`'s `config.json` are not used. The `config.json` the tool writes per container is its own format.
- So a Rust agent would run the CLI, or a small Swift helper built on the library.

**What a Mac needs.**
- The tool's files (bin and libexec, about 466 MiB). Run as here, they need not be at `/usr/local`, because `--install-root` relocates them.
- A kernel file: the 29 MiB `vmlinux` above.
- The init image, which the tool pulls on first use unless it is preloaded.
- Per user running it: an app root holding images, snapshots and containers. Here that was 2.58 GB for four images.
- Rosetta, for x86_64 images.

**Hostkeeper.** Its `macos-pkg` installer requires, among other things (`docs/design/mac-hostkeeper-package.md`, the list quoted there):
- a `PackageInfo` `identifier` equal to the manifest's package;
- `install-location` `/`;
- a signer whose leaf is in the Mac's signers file;
- a Bom whose entries are all root-owned, with no setuid, setgid or sticky bit.

The design also places a service's payload under `/Library/Application Support/<service>/`, though the installer does not check that.

Apple's package has identifier `com.apple.container-installer`, `install-location` `/usr/local` and Apple's Developer ID leaf. As shipped it therefore fails the identifier and location checks and needs Apple's leaf in the signers file. Its Bom was not checked against the ownership rule. Because the files run from any directory, the same files can be repackaged under our own identifier and path, or shipped inside the `js-agent` payload. Either way, the release's checksum is the pin, and an update replaces the files and restarts the per-user services. Rosetta is installed with `softwareupdate`, as root.

**Pool users instead of the logged-in user.** This was not run as a pool user: creating users needs root. Measured and read:
- The services are per-user launchd agents in `user/<uid>` and accept `Background` sessions. Every call here came from an SSH session, not the GUI session. Whether they start for a uid that has never had a console login was not tested.
- Everything is per user: the app root (images, snapshots, containers, kernel), the API server and the vmnet network service. N pool users would mean N image stores and N service sets, unless one dedicated user runs the tool for every lease.
- A pool user's reset (`docs/design/mac-lease-teardown.md`) kills every process of the uid, boots out its launchd domains and removes and remakes its home. That would stop that user's container services and, if the app root is under the home, delete its image store, so the next lease pulls and unpacks again.
- Jobs started through `NativeRuntime` run under a Seatbelt profile that grants writes to the home only. The VM itself is started by the per-user service over XPC, not by the job's process, so the profile does not cover it. How the two interact was not tested.

## 8. What did not map

These are listed in the order a job would meet them. None is resolved here.

1. **The guest is not hardened the way a Linux job is.**
   - There is no user namespace, seccomp or `no_new_privs`.
   - `/tmp` cannot be made `noexec`.
   - `/sys/fs/cgroup` is writable, so a root job can raise its own memory cap up to the VM size.
   - There is no pids limit.
   - The VM is the isolation boundary instead. No `container run` flag was found that sets a seccomp profile, `no_new_privs`, a pids limit or mount options; `--cap-drop` exists but was not tried.
2. **Egress is not filtered.**
   - NAT reaches everything, including the LAN and the host.
   - Host-only reaches the host's every listener.
   - An allow-list needs a host-side relay with a changed destination, or packet filtering on the vmnet bridge. The latter needs root and was not tried.
3. **Start time.** About 0.54 s is spent booting a VM per container. Neither cold nor warm, nor image size, changes it; nothing in the tool keeps a VM ready.
4. **Shared directories.** Ownership is not modelled on virtiofs: files appear owned by the reader. Writing many small files through a share is slow (26 s for 46,362 files).
5. **Memory accounting.** A VM keeps its high-water mark of host memory after the guest frees it.
6. **Exit codes.** Without `--init`, signals do not end a PID-1 job. "Not found" and "not runnable" are both exit 1.
7. **Rosetta** is absent on this host. Installing it needs root.
8. **Per-user runtime.** Everything the tool keeps is per uid, and a pool user's reset would remove it.

## 9. Reproducing

Everything ran under one directory in the user's home, which was removed at the end along with the tool's services. The commands are the ones in sections 2, 3 and 4. The timing harness and the probe scripts are not committed. The harness is about forty lines: spawn, read one byte, wait, with `system stop` and `system start` before each cold sample. Nothing in this document is built or tested by the repository, so its numbers are reproduced by repeating the run on a Mac, not by any target here.
