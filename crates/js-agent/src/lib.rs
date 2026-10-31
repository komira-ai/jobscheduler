//! `js-agent`: runs on a machine, tells a lease server what the machine offers, and
//! keeps telling it that the machine is alive, so the machine can be leased through the
//! server's ordinary lease API.
//!
//! The library is the program: [`run_with`] takes the arguments, the streams to print to
//! and the [`Services`] (the commands and files it reads the host from, the transport to
//! the server, the sleeper and the jitter, and the [`Machine`] its jobs run on), and `main`
//! calls [`main`], which supplies the machine's own. The usage sampler reads time only
//! through [`usage::MonotonicClock`], which the caller hands it; apart from that and the
//! runtime's own timestamps, time passes only in the sleeper. The one clock of the jobs is
//! the monotonic one the [`Machine`] gives, which the worker and the session share.
//!
//! # Verbs
//!
//! `js-agent image-pull <repository>@sha256:<hex>` (Linux, run as root) pulls an image into
//! the store the crun runtime reads, without starting the agent and without a container
//! engine; see `image::pull`.
//!
//! # Flags
//!
//! Configuration is flags only: `js-agent` reads no environment variable.
//!
//! | flag | meaning |
//! |---|---|
//! | `--server <url>` | required: `https://host:port`; `http://` only for `localhost`, `127.0.0.0/8` and `::1` |
//! | `--token-file <path>` | required unless `--state-root` holds a usable `host-credential` file: the pool's join token, in a regular file owned by the user running the agent, mode 0600 or 0400 |
//! | `--ca-cert <path>` | optional for `https://`: the PEM file with the certificate to trust, and then only that is trusted. Without it the public roots bundled in the agent (`webpki-roots`, pinned by `Cargo.lock`) are trusted, so a publicly trusted endpoint works with no certificate pushed; the platform's roots are never read. Ignored for `http://` |
//! | `--pool <name>` | required: the pool to join |
//! | `--host-id <id>` | the id to register under; by default `h-` and 32 hex digits of a salted SHA-256 of the machine id, never the machine id |
//! | `--label k=v` | a label on the host; repeatable |
//! | `--capability k=v` | a capability to report; repeatable; `os`, `arch`, `runtime`, `exec`, `gpu-toolkit`, `run-files`, `disk-gib`, `scratch-quota`, `scratch-default-gib`, `bundle` and `bundle-root` are detected and cannot be set |
//! | `--cpu-millis`, `--memory-bytes`, `--gpus` | offer less than the host has; never more |
//! | `--gpu-class <class>` | the class the offered GPUs are reported under, instead of the detected one |
//! | `--allow-host-jobs` | not available yet: refused at start, because the scheduler does not record a host job's taint yet; without it a start with `isolation = host` creates nothing and is refused |
//! | `--state-root <path>` | the directory the taint marker is kept in; owned by the agent's user, no permission for group or others; required with `--container-bin`. A host that a post-job check found unclean is tainted for good: the marker survives a restart and no code of the agent removes it. It may not be, or lie in, a mount root or the scratch root, and it must be on a disk of its own (a job that fills a shared disk makes the marker unwritable). The agent refuses a state root whose `st_dev` equals that of any mount root or the scratch root; that is a best-effort guard, not the guarantee: subvolumes that share free space have distinct device numbers, and the engine's data root and container layers are not compared. The taint belongs to this directory, not to the machine: a second agent, another state root or a state root that is lost at a reboot (`tmpfs`) does not know of it |
//! | `--watch-dir <path>` | a directory the post-job checks list (its own entries: names, sizes, modification and status change times, inodes, owners, modes; a write to an existing file inside a subdirectory is not seen) and compare with the listing taken when the host was last known clean; one level deep: a name added to a subdirectory is seen, a write to a file in it is not, so watch the subdirectory itself; repeatable; needs `--container-bin`; may not hold, or lie in, a mount root, the scratch root or the state root; more than 4096 entries cannot be checked and taint the host. With none, only `/etc/passwd`, `/etc/group`, the processes, the mounts and the kernel modules are checked, and the agent warns at start; suggested: `/etc`, `/etc/cron.d`, `/etc/sudoers.d`, `/etc/systemd/system`, `/etc/ld.so.conf.d`, `/root/.ssh`, `/usr/local/bin`, `/usr/local/sbin`. What a job leaves in a read-write `--mount-root` is never examined |
//! | `--mount-root <name>=<path>` | a directory jobs may mount from, by name; repeatable; canonicalised when the agent starts; no root, no mount |
//! | `--mount-root-ro <name>=<path>` | like `--mount-root`, in the same namespace of names, but a start with a read-write mount from it is refused (`invalid-mount`) and nothing is created; it may be on the scratch root's file system under `--scratch-quota`; the `st_dev` comparison sees a root's own device only, not a scratch device mounted below a root (a submount a recursive bind brings along), so the operator must guarantee there is none |
//! | `--scratch-quota` | bound each run's disk with an XFS project quota on the scratch root's file system. The agent probes the root at start and refuses to start unless the file system enforces project quota (`prjquota`, ids settable), saying why; it then advertises `scratch-quota=project` and `scratch-default-gib` (a detected pair an operator cannot set), books the default for a run that names no `disk_bytes`, and before each run's scratch directory is given away or written to sets its project id, a block limit of the bytes the run books and an inode limit; after the tree is removed it clears them. It refuses to start when a `--mount-root` is on the same file system as the scratch root (`st_dev`; give such a root as `--mount-root-ro`). Needs `--scratch-root`; a Linux host only |
//! | `--scratch-default-gib <n>` | the GiB a run without `disk_bytes` books and is bounded to under `--scratch-quota`, and the value of `scratch-default-gib`; from 1 to 65535, default 2; needs `--scratch-quota` |
//! | `--scratch-max-inodes <n>` | the inode limit of each run's scratch directory under `--scratch-quota`; from 1 to 4294967295, default 1000000; needs `--scratch-quota` |
//! | `--scratch-quota-failures <n>` | the quota calls (`apply`, a usage read, `clear`) that may fail in a row under `--scratch-quota` before the agent probes the scratch root again; when it no longer enforces project quota the agent starts no run (each start is refused `quota-unavailable`, which taints nothing) and reports it every minute, until a probe passes; from 1, default 3; needs `--scratch-quota` |
//! | `--scratch-usage-wait-secs <n>` | the seconds a finished run's slot is held for its project's usage to read zero under `--scratch-quota`; after that the slot is parked (it takes no run and is read at every step), the run's place and booked bytes are given back, and the quota counts one failure; from 1 to 86400, default 600; needs `--scratch-quota` |
//! | `--scratch-abandon-limit <n>` | the slots the agent may give up on (see `--scratch-usage-wait-secs`) over its life under `--scratch-quota`; a run that ends well does not take one back; at the limit no run starts (each start is refused `quota-unavailable`) and the finding `quota-unhealthy` repeats every minute until the agent is restarted, so the operator must watch `quota-usage-abandoned` and look for files held open on the scratch volume; from 1, default 8; needs `--scratch-quota` |
//! | `--scratch-low-water-gib <n>` | GiB of the scratch root's free disk kept back from jobs; the disk offered (`shape.disk_bytes`, and `disk-gib` in GiB) is the free space read at start minus this, 0 when this is more; needs `--scratch-root` |
//! | `--scratch-root <path>` | where each run's private (`0700`) scratch directory goes; owned by the agent's user, no permission for group or others; required with `--container-bin` |
//! | `--run-files-root <path>` | where each run's files (the `files` of a start: a URL or a token the task reads) are written, in a directory of their own per run, `0555` with `0444` files, mounted read-only at `/run/js-files`. Made `0700` when missing; it must be on a `tmpfs` (the secrets never reach a disk) and the agent refuses to start when it is not. A reboot clears it, which is fine: a run does not survive one. Default `/run/js-agent/files` when that can be made; otherwise the agent runs without run files and does not report the `run-files` capability. tmpfs pages can be swapped to disk when the host has swap: run hosts that handle these secrets with swap off or encrypted. Needs `--container-bin` |
//! | `--container-bin <path>` | the absolute path of the `docker` or `podman` program that runs container jobs; never looked up on the `PATH`; started with an environment of its own (a fixed `PATH` and `--container-host`, nothing inherited); without it no container runs; a job that names a GPU needs the NVIDIA container toolkit with Docker and, with Podman, a CDI specification (`nvidia.com/gpu=<index>`) the operator has generated; rootless engines are not supported (the engine gets no `XDG_RUNTIME_DIR`, `HOME` or D-Bus session, and a rootless Podman may then run containers without their limits); every job runs in a user namespace mapped onto the `--container-subuid` range (Podman is given the maps for each container; a Docker daemon must run with `userns-remap` set to that user, or the agent does not start); the agent must share the pid and mount namespaces of `init` and be able to read `/proc/1/ns` (root, or `CAP_SYS_PTRACE`), or it does not start |
//! | `--native-helper-socket <path>` | the Unix socket of the root helper `js-mac-session`: with it a Mac agent runs native jobs, each as a user the helper makes for the run, under a Seatbelt profile, and reports `runtime` and `exec` as `sandbox-exec` (see [`native`]); macOS only; the agent must run as root and checks at start that `sandbox-exec` applies a profile; needs `--scratch-root` and `--state-root`; not with `--container-bin`; the helper must be dedicated to this agent on a Mac used for native jobs only, started with its own explicit `--socket`, `--state-dir`, `--uid-range` and `--homes` (never the defaults of another helper), because the agent asks it for its boot reset, which ends the lease users of whoever used it before and clears the Mac's auto-login; native runs are NOT stopped when the agent stops, restarts or dies: the lease journal in the state root ends them when the agent next starts, or when the rescue of an agent whose worker died runs; until then they run on, and their wall limit is enforced only while an agent runs; the profile does not deny System V or POSIX shared memory, semaphores or message queues, and the helper reuses uids, so what a run leaves in them can be read by a later run that gets the same uid: an open channel between runs on one Mac |
//! | `--native-homes <path>` | the directory the helper makes its lease users' homes in (its `--homes`); by default `/Users`; it must exist; needs `--native-helper-socket` |
//! | `--native-network <level>` | the most network a native run gets: `off` (the default), `loopback` or `on`; a start that asks for more is held to it; `on` has no egress control (a run can reach the scheduler, the cloud metadata address and the rest of the network) until a packet filter keyed on the lease users exists, so keep `off`; `loopback` is shared by every run of the Mac; needs `--native-helper-socket` |
//! | `--native-tool-root <path>` | a directory a native start's program must lie under; repeatable; by default `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin`; needs `--native-helper-socket` |
//! | `--native-deny-read <path>` | a path no native run may read, beside the agent's roots, token, instance and CA files, the helper's socket directory and `/etc/kcpassword`; repeatable; name the helper's `--state-dir`; must exist; needs `--native-helper-socket` |
//! | `--native-max-file-mib <n>` | the most MiB any one file a native run writes may reach, its output included (the run's `RLIMIT_FSIZE`); by default 4096; a run that writes more is ended by `SIGXFSZ`; needs `--native-helper-socket` |
//! | `--native-max-processes <n>` | the most processes a native run's user may have (the run's `RLIMIT_NPROC`); by default 512; the helper's kill is not held by it; needs `--native-helper-socket` |
//! | `--container-network <name>` | the user-defined network containers join; by default none at all; `host` is refused; a named network needs at least one `--container-egress-allow` (the agent does not start without it), and before every contained job is created the agent checks that the network is a bridge with DNS and IPv6 off and that the host's nftables table `inet js_egress` limits it to those endpoints, or refuses the job (`egress-unchecked`); Podman only; docs/egress.md sets the network up; needs `--container-bin` |
//! | `--container-user <uid:gid>` | the base of the users containers run as, container ids both above zero; by default `1000:1000`. Every run holds a slot, numbered from 0, and runs as the base plus its slot number, uid and gid (`1000:1000`, `1001:1001`, ...), so two runs held at once never share a host uid; the slot is given again only after its last run's cleanup passed (container and scratch gone, no process left), and never if a check found something. Each of the 4096 slots needs an id, so both numbers of the base are at most 61440 (the agent refuses to start otherwise). The agent gives each run's scratch directory to the host ids of its slot's user (the start of the `--container-subuid` range plus each id); needs `--container-bin` |
//! | `--container-subuid <user>` | required with `--container-bin`: the user whose range in `/etc/subuid` and `/etc/subgid` every job's user namespace (container ids 0 to 65535) is mapped onto, so no container id is a host id. The agent refuses to start unless both files give the user at least 65536 ids, not from 0, holding no id of `/etc/passwd` or `/etc/group`; Podman gets `--uidmap` and `--gidmap` for every container, a Docker daemon must run with `userns-remap` set to the user and onto that range; a second agent on the host with an overlapping range is refused (a lock under `/run/js-agent/subid`); the agent must run as root |
//! | `--container-host <uri>` | the engine to talk to: a socket on this machine, `unix:///<path>`, passed as `DOCKER_HOST` and `CONTAINER_HOST` to the engine's calls only; by default the engine's own default; `tcp://` and `ssh://` are refused (the post-job checks read this machine, and a job on another one would pass them all); needs `--container-bin` |
//! | `--container-pull <never\|missing>` | when a job's image is pulled; by default `never`, so a job cannot make the host reach a registry of its choosing: the operator puts an image on the host first (`docker pull`, or `docker load` from an archive), and a job whose image is not there ends as `failed_to_start`; needs `--container-bin` |
//! | `--container-tmp-size <size>` | the size of each container's `/tmp` (`noexec,nosuid,nodev`; on Podman also of `/var/tmp` and `/run`), such as `64m` (the default) or `1g`; needs `--container-bin` |
//! | `--container-add-host <name>:<ip>` | a static host entry for every container, passed at create as `--add-host`; repeatable, at most 16; the name is a DNS name, the address IPv4 or IPv6 (`host-gateway` and anything else is refused, a name twice too); a network made without DNS resolves nothing, and a presigned URL names its host (its signature covers the Host header), so every endpoint a job may reach is given this way; needs `--container-bin` |
//! | `--container-egress-allow <ip:port>` | an endpoint a named network may reach, `IPV4:PORT` or `CIDR:PORT` (prefix 24 to 32, outside 169.254.0.0/16 and the network's own subnet); repeatable, at most 16; required with a named `--container-network` (the agent does not start without it); before every job is created the agent reads back the network (a bridge, DNS off, no IPv6, one IPv4 subnet), `br_netfilter` and the nftables table `inet js_egress`, and refuses the job (`egress-unchecked`) unless the table holds exactly the rules for these endpoints; Podman only; see docs/egress.md; needs `--container-bin` |
//! | `--container-nft <path>` | the absolute path of `nft` for that check; by default `/usr/sbin/nft`; needs `--container-egress-allow` |
//! | `--egress <none\|allow-list>` | the network a job's `crun` container gets: `none` (the default) loopback only, no route; `allow-list` one interface to the host, which forwards only what `--egress-allow` and `--egress-dns` name; the agent, as root, wires each container from outside and keeps the firewall (docs/design/agent-egress.md); only the `crun` runtime reads it and this build cannot select that runtime yet, so `allow-list` is refused at start, as is `--container-network` beside it |
//! | `--egress-allow <ip:port>` | an endpoint an allow-list may reach, `IPV4:PORT` or `CIDR:PORT` (TCP; prefix 24 to 32, outside 169.254.0.0/16 and the pool); repeatable, at most 16; at least one is required; needs `--egress allow-list` |
//! | `--egress-dns <ip>` | a resolver an allow-list may ask (UDP and TCP port 53) and the only nameserver of a job's `resolv.conf`; repeatable, at most 4; without one a job resolves no name; needs `--egress allow-list` |
//! | `--egress-pool <cidr>` | the IPv4 network the jobs' addresses come from, one /30 each, `/16` to `/24`; by default `198.18.0.0/16`; needs `--egress allow-list` |
//! | `--egress-ip <path>`, `--egress-nft <path>`, `--egress-nsenter <path>` | the absolute paths of the `ip`, `nft` and `nsenter` programs the egress setup runs, by default `/usr/sbin/ip`, `/usr/sbin/nft` and `/usr/bin/nsenter`; need `--egress allow-list` |
//! | `--actions-cgroup <path>` | the cgroup version 2 directory below `/sys/fs/cgroup` that holds every container (made and delegated by the operator): each container is created with `--cgroup-parent` naming it (the slice name for one `.slice` directly below `/sys/fs/cgroup`, the systemd driver; the path with a leading `/` otherwise, the cgroupfs driver), and the heartbeat carries the usage of the directory and of each lease's cgroup (see [`usage`]); without it the heartbeat carries no load, and the agent says so at start; needs `--container-bin` |
//! | `--gpu-toolkit-hook <path>` | the absolute path of a program that checks the GPU container toolkit, run once at start when the host offers a GPU; exit 0 makes the hello report `gpu-toolkit`, anything else leaves it out and warns; needs `--container-bin` |
//! | `--runtime <docker\|crun>` | what runs a job: `docker` (the engine program of `--container-bin`, Docker or Podman, which is also what no flag means) or `crun` (js-contain over the distribution's `crun`, each command run as `--job-user`; Linux only). The packaged unit of the Docker cloud hosts names `--runtime docker`; the unit for the Linux farm (`packaging/js-agent/js-agent-crun.service`) names `--runtime crun`. With `crun` the agent checks the host before it registers and exits 2 naming every piece that is missing: `crun` at `--crun-bin` and its version (1.14 or newer), `newuidmap` and `newgidmap`, the job user and its subuid and subgid ranges (65536 ids or more each), the state root (`0700`, owned by root) and the scratch root (`0711`, owned by root, every directory above it searchable by the job user), `crun`, `newuidmap` and `newgidmap` executable by the job user, the cgroup version 2 controllers `cpu`, `memory` and `pids` in the agent's own cgroup (`Delegate=yes`), and, where `kernel.apparmor_restrict_unprivileged_userns` is 1, a loaded AppArmor profile for `crun`. Then the worker runs jobs through `CrunRuntime`: images are pulled by the agent's registry client from the `--image-allow` entries into a store beside the scratch root (`oci`), and the agent takes no `--mount-root` or `--mount-root-ro` with it, because the runtime does not read a job's binds back yet |
//! | `--crun-bin <path>` | the `crun` of `--runtime crun`; by default `/usr/bin/crun`, the path the distribution's AppArmor profile attaches to; any other path starts the agent with a warning (and is refused by the kernel where user namespaces are restricted) |
//! | `--job-user <name>` | the non-root user every `crun` command runs as, whose subuid and subgid ranges every job's user namespace is mapped onto; required with `--runtime crun`, and needs it |
//! | `--image-allow <host[:port]/prefix>` | a registry and namespace an image may be pulled from, such as `ghcr.io/komira-ai/` (whole path components: `komira-ai/` admits `komira-ai/x`, not `komira-ai-x/y`); repeatable; a bare host is refused at start. An image on none of the entries is never pulled, so a job cannot make the host reach a registry of its choosing. Read at start and kept for the runtime that pulls with js-oci; the Docker and Podman runtimes pull as `--container-pull` says and do not read it |
//! | `--stop-grace <seconds>` | how long a container gets to stop on its own before it is killed, whenever the agent stops one; by default 10, at most 3600; needs `--container-bin` |
//! | `--drain-wait <seconds>` | how long SIGTERM lets the running jobs finish before the agent stops them; by default the stop grace, at most 3600; needs `--container-bin` |
//! | `--bundle-store <path>` | the store of unpacked toolchain bundles (a Mac only): absolute, on the volume of `--scratch-root`, which must be APFS, mounted with ownership honoured and not noexec, with the kernel's securelevel at most 0, or the agent does not start; needs `--scratch-root` and `--bundle-budget-gib`. The hello then carries `bundle-root` (this path) and `bundle` (the held manifest digests, comma-separated, at most 16 of them), and the disk offered is reduced by the part of the budget the store has not used (see [`bundle`]). The hello carries `bundle` and `bundle-root` only when the agent can run a bundle: it runs native jobs (`--native-helper-socket`) and the store opened; without the helper it carries neither. A job that pins a bundle runs `@bundle/<entrypoint>` from the store's trees, its writable trees cloned for the run, under the profile of any native job; and, until a server says it takes a longer value, `bundle` holds one digest (two are 143 bytes, over the 128 an older server accepts and then refuses the hello). Roll out in this order: servers first (they accept the long value and the new codes), then agents, then `--bundle-store`, then placement by `bundle`; no job that names a bundle may be placed on a host that did not report `bundle` |
//! | `--bundle-budget-gib <n>` | the most GiB the store's trees and trash may take; required with a store, above 0 |
//! | `--bundle-low-water-gib <n>` | fetches are refused while the volume has less than this free; default 0 |
//! | `--bundle-pull <never\|missing>` | whether a bundle the store lacks is fetched; default `never` (a job that names one is refused `bundle-not-held`); `missing` needs trust `list` and a registry flag: the first start that names a bundle begins a fetch in the background (`bundle-pending` until it is held; every byte is checked against its digest; a fetch that fails is reported once as `bundle-fetch-failed`) |
//! | `--bundle-registry-allow`, `--bundle-registry-allow-private <host[:port]/prefix>` | the registries and namespaces a bundle's name may point to; repeatable; a bare host is refused; when any is given, a name outside them is refused `bundle-registry-not-allowed` |
//! | `--bundle-min-age-min <n>` | minutes a bundle's trees stay after their last use; default 30 |
//! | `--bundle-keep-file`, `--bundle-prefetch-file <path>` | files of manifest digests (`sha256:<hex>` a line, `#` comments), checked at start; the eviction and the fetcher that read them arrive later |
//! | `--bundle-deny-file <path>` | digests refused `bundle-denied`, read at every start of a job |
//! | `--bundle-trust <list\|digest>` | which digests may run: `list` (default) those of `--bundle-allow-file`, which must be given and hold a digest; `digest` any, for developer hosts, and never with `--bundle-pull missing`; a digest off the list is refused `bundle-not-allowed` |
//! | `--bundle-allow-file <path>` | the allow list, read at every start of a job |
//! | `--read-model <open\|allow>` | the read model of a run's profile; `allow` is refused at start for now |
//!
//! The job flags are checked at start and a wrong one ends the agent with exit status 2.
//! A marker that is already in the state root is reported on stderr at start.
//! Container jobs are Linux-only (the checks read `/proc`): `--container-bin` on a Mac ends
//! the agent with exit status 2.
//!
//! # Jobs
//!
//! Without `--container-bin` the agent registers and heartbeats and starts nothing; a
//! `Start` the server sends it is ignored (and counted in the log). With it, the agent
//! builds the container runtime (Docker or Podman by their command line), takes the
//! baseline of the host for the post-job checks, opens the taint marker, adopts the
//! containers an earlier process of this agent left, and starts the [`Worker`], the thread
//! that owns the [`lifecycle`]: it runs every call into the engine, so a slow engine never
//! delays a heartbeat. The first hello lists what was adopted.
//!
//! From then on each heartbeat carries the runs the agent holds, the results the server has
//! not acknowledged and the taint to report; each reply's commands go to the worker in
//! order (a command it refuses is logged by its place in the reply), its result
//! acknowledgements let the worker drop the results it sent, and its `taint_ack` counts
//! only when the heartbeat it answers carried the taint. See [`Session`] and [`Worker`]
//! for the details and [`lifecycle`] for what the agent guarantees per job.
//!
//! The agent that cannot start its jobs' half (the engine is not Docker or Podman or does
//! not answer, the agent cannot see the host, the containers of an earlier run cannot be
//! listed) ends with exit status 2 before it registers.
//!
//! # What is detected
//!
//! The CPU count (`getconf _NPROCESSORS_ONLN`), memory (`/proc/meminfo`, or `sysctl -n
//! hw.memsize` on a Mac), the machine id (`/etc/machine-id`, or `ioreg` on a Mac), and
//! the GPUs: `nvidia-smi` on Linux (class derived from the model name, for example
//! `nvidia-a100-sxm4-80gb`; every GPU must be of one model), and
//! `system_profiler SPDisplaysDataType -json` on a Mac, where each Apple GPU counts as
//! one and the class is `apple-m`. A GPU probe that fails, hangs, prints too much or
//! prints something unreadable offers no GPU and says why on stderr; so do GPU texts the
//! wire refuses (a model or UUID that is too long or holds a control character). A GPU
//! is never invented. The capabilities `os` and `arch` are the platform's. `--gpus 0`
//! offers none.
//!
//! The host contract (see `contract`): once the job half has started, which ran the engine
//! program, the hello carries `runtime=container` and `exec=container`; with GPUs offered
//! and a `--gpu-toolkit-hook` that exits 0 it also carries `gpu-toolkit=true`. An agent that
//! runs no jobs reports none of them, as an older agent does not. A pool that declares an
//! execution mode refuses a host without them (`host-contract`, `execution-mismatch`).
//!
//! On Linux the CPU count and memory are the lowest of the machine's total and what the
//! process may use: the CPU quota and memory limit of its cgroup (version 2) and of the
//! cgroups above it, and the CPUs it is allowed to run on (`taskset`, a cpuset). An agent
//! in a container offers the container's share and not the node's. On a Mac the totals are
//! offered. The flags above lower the result further and never raise it.
//!
//! Every helper program is started by its absolute path (`/usr/bin/getconf`,
//! `/usr/bin/nvidia-smi`, `/usr/sbin/sysctl`, `/usr/sbin/system_profiler`,
//! `/usr/sbin/ioreg`), never by a name looked up on the inherited `PATH`. The trust
//! assumption is that those system directories are writable by root only. Each helper is
//! waited for at most [`PROBE_LIMIT`] (and as long again for its exit), at most
//! [`PROBE_OUTPUT_CAP`] bytes of its output are read, and a helper that overruns is killed
//! by its pid and collected. SIGTERM and SIGINT that arrive meanwhile are kept: the agent
//! stops at its first wait after the probes.
//!
//! # The session
//!
//! See [`Session`]. The agent says hello, heartbeats at the interval the server names
//! and sends the session secret the server issued in every heartbeat. A server that lost
//! the session (it restarted) gets one hello at once.
//!
//! A refusal that asking again cannot change ends the agent with exit status 2 and the
//! server's code on stderr (with `--container-bin` the agent first stops the containers it
//! holds, bounded, and the results it could not send are lost; the same when the job worker
//! stops, which ends it with status 1, except that the stop does not go through the
//! worker; a container that could not be stopped is named on stderr, and the agent relies
//! on its supervisor and on the next process adopting it, so the supervisor must restart
//! the agent after every exit, status 0 and 2 included: with systemd that is
//! `Restart=always` and no `RestartPreventExitStatus=2`, because a refusal exits with 2
//! and a SIGTERM with 0, and `Restart=on-failure` would leave the leftovers of either
//! unsupervised; see `run`): 401 (the token), 403 `unknown-pool`, `shape-not-accepted`,
//! `host-contract`, `execution-mismatch`, `host-bound` and `host-listed` (things only the operator can change), and the 4xx that
//! say the message cannot be read or served (`invalid-hello`, `invalid-heartbeat`,
//! `unsupported-protocol`, `body-too-large`). A 403 `pool-full` is not among them: a pool
//! has room again when a host leaves, so the agent waits and asks again. Everything else
//! (409 `host-live`, 408, 429, 5xx, a server that cannot be reached) waits with a capped,
//! jittered backoff ([`Backoff`](crate::Backoff)), longer when the server sent
//! `Retry-After`, up to a bound.
//!
//! SIGTERM and SIGINT make the agent send a heartbeat with the phase `draining` and exit
//! 0, so the server leases the host no more. With jobs the agent first drains (see
//! [`drive`] for the whole of it): it takes no new job, announces `draining` at once, lets
//! the running jobs finish for `--drain-wait`, then stops them, and goes on heartbeating
//! until the server has acknowledged every result and the host holds no run, or `--stop-grace`
//! and [`DRAIN_SETTLE`] more have passed. A second signal ends it at once. A result that
//! is not acknowledged is never dropped: its run stays listed, and a container that is
//! still there is adopted by the next process.
//!
//! # A whole-node lease
//!
//! A session given a [`Quiesce`] ([`Session::with_quiesce`]) follows the `exclusive` lease a
//! heartbeat reply names: `preparing`, then `exclusive_ready` once the host is quiet, or
//! `exclusive_refused` (`host-unsafe` for a finding, `not-quiet` when a run is still held
//! after `prepare_timeout`, the hook's own code when the readiness hook refuses), then
//! `restoring` when the reply stops naming the lease; see the `exclusive` module for the
//! sequence and [`lifecycle::Lifecycle::prepare`] for what quiet means.
//!
//! The agent started by [`run_with`] with `--container-bin` gives its session a
//! [`WorkerHost`], so it follows the lease. The checks and the hook block, so they run on
//! the job [`Worker`]'s thread: each heartbeat that is waiting for the host sends the worker
//! an order without waiting and reads the answer the worker last published for that lease
//! (an answer about another lease is never taken for this one). While the host is not quiet
//! the heartbeats say `preparing`; the starts of the lease run once the host is found quiet
//! (and the hook, if any, agreed), and a start for any other lease is refused (`no-capacity`)
//! from the reply that names the lease until the one that stops naming it. The readiness
//! hook (`--readiness-hook`, see `readiness`) runs once for each lease, after the agent's
//! own checks passed, and its verdict is kept for the lease; an agent that restarts while it
//! holds no run of the lease prepares it again, and the hook runs again. The deadline is
//! [`PREPARE_TIMEOUT`] until a flag sets it, counted on the worker's clock. An agent without
//! `--container-bin` runs no job and has no host to prepare, so it ignores `exclusive`. The
//! VM check of the design is a later change. [`Hooked`] is the same hook rule as a wrapper
//! around any [`Quiesce`], for a host driven in-thread.
//!
//! # Secrets
//!
//! The join token is read from its file by `js-client`, held there, and sent in the
//! `Authorization` header only. The session secret lives in the [`Session`] and in the
//! body of the heartbeats. Neither is in argv, in a log line, in an error or in a `Debug`
//! output.
//!
//! # Exit status
//!
//! 0 after a stop request, 1 when the program cannot install its signal handlers or its
//! job worker stops, 2 for a flag or file that is wrong, a job engine that cannot be used
//! and a server that refuses the host for good.

mod backoff;
pub mod bundle;
mod cli;
pub mod cli_runtime;
mod container;
mod contract;
mod credential;
#[cfg(target_os = "linux")]
pub mod crun;
#[cfg(target_os = "linux")]
pub mod crun_host;
pub mod egress;
mod exclusive;
mod facts;
mod host;
#[cfg(target_os = "linux")]
pub mod image;
pub mod integrity;
mod jobs;
pub mod lifecycle;
pub mod native;
pub mod quota;
mod readiness;
mod run;
mod session;
mod signals;
mod system;
mod transport;
pub mod usage;
mod worker;

use std::ffi::OsString;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

pub use backoff::{BASE, Backoff, CAP, Jitter, RETRY_AFTER_CAP, SystemJitter};
use bundle::{Admission, BundleHost, Settings as BundleSettings, SystemHost};
pub use cli::{Cli, RuntimeKind};
use cli_runtime::Settings;
use contract::ToolkitHook;
pub use credential::{CREDENTIAL_FILE, CREDENTIAL_TEMP_FILE, CredentialFile, CredentialStore};
pub use exclusive::{PREPARE_TIMEOUT, Quiesce};
pub use facts::{
    APPLE_GPU_CLASS, CommandRunner, Detected, FileReader, Gpus, PROBE_LIMIT, PROBE_OUTPUT_CAP,
    Platform, Ran, Streams, SystemCommands, SystemFiles, detect, host_id, machine_id,
};
pub use host::{Offer, StoreView, offer};
pub use jobs::DRAIN_SETTLE;
use jobs::Seams;
use lifecycle::{Local, StateRoot};
pub use readiness::{
    EXIT_NOISE_OVER_BOUND, EXIT_NOT_QUIET, EXIT_SETUP_FAILED, EXIT_UNSUPPORTED, HookRunner,
    HookWindow, Hooked, ReadinessHook, Verdict, WorkerHook, hook_runner,
};
pub use run::{Exit, Sleeper, Wake, drive};
pub use session::{
    CredentialState, Drain, Fatal, HEARTBEAT_PATH, HELLO_PATH, Note, Reason, Session, Step,
};
pub use signals::SystemSleeper;
pub use system::{Backend, Machine, RuntimeBuild, SharedClock, System};
pub use transport::{Answer, Connector, Dialer, REQUEST_LIMIT, Transport};
pub use worker::{
    Delivery, Handle, LINES_CAP, LeaseAnswers, Plan, QUEUE, RESCUE_BOUND, RESCUE_MAX, Rescue,
    STALL_CALLS_MAX, STALL_MARGIN, STEP_EVERY, Snapshot, WIND_DOWN_MARGIN, Worker, WorkerHost,
    stall_bound,
};

/// The exit statuses of `js-agent`.
pub mod exit {
    /// The agent was asked to stop and announced it.
    pub const OK: u8 = 0;
    /// The program could not install its signal handlers.
    pub const FAILED: u8 = 1;
    /// A flag or a file it names is wrong, or the server refused the host for good.
    pub const USAGE: u8 = 2;
}

/// The directories worth a `--watch-dir` on a Linux host (each is listed one level deep):
/// where a job that reached the host could leave a way back.
pub const SUGGESTED_WATCH_DIRS: [&str; 8] = [
    "/etc",
    "/etc/cron.d",
    "/etc/sudoers.d",
    "/etc/systemd/system",
    "/etc/ld.so.conf.d",
    "/root/.ssh",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// Everything the agent touches outside its own memory.
pub struct Services<'a> {
    /// The platform whose probes to run.
    pub platform: Platform,
    /// Starts the probes' programs.
    pub commands: &'a dyn CommandRunner,
    /// Reads the probes' files.
    pub files: &'a dyn FileReader,
    /// Makes the transport the flags name.
    pub connector: &'a dyn Connector,
    /// Randomness for the backoff.
    pub jitter: &'a mut dyn Jitter,
    /// Lets time pass and reports stop requests.
    pub sleeper: &'a mut dyn Sleeper,
    /// The clock, the container runtime and the host view of the jobs, when
    /// `--container-bin` is given.
    pub machine: Arc<dyn Machine>,
    /// What the bundle store reads from the machine; used only with `--bundle-store`.
    pub bundle: Arc<dyn BundleHost>,
    /// The server takes a `bundle` capability value longer than the common 128 bytes. The
    /// machine's own is false: a server cannot say so yet.
    pub long_bundle_values: bool,
    /// Told of the job worker once it has started (a test's way to wait for it).
    pub worker_hook: Option<&'a dyn Fn(Handle)>,
    /// The bound of the rescue of the jobs in place of [`RESCUE_BOUND`]: for a test.
    pub rescue_bound: Option<Duration>,
    /// The effective user id of the agent: a native agent must be root.
    pub euid: u32,
}

/// Why the program ends without a stop request.
enum Failure {
    /// The command line, as clap reports it (this includes `--help` and `--version`).
    Clap(clap::Error),
    /// A flag or file is wrong.
    Usage(String),
    /// The server refused the host for good.
    Fatal(Fatal),
    /// The job worker is gone.
    Worker(String),
}

/// Checks `--runtime` and the flags that belong to it, and for `crun` the host: a wrong
/// flag or a missing piece of the host ends the agent with status 2 before it registers.
/// For `crun` it returns the settings of the crun runtime, which stand where
/// `--container-bin` stands for the engines.
fn runtime_flags(
    cli: &Cli,
    platform: &Platform,
    services: &Services<'_>,
    err: &mut dyn Write,
) -> Result<Option<Settings>, Failure> {
    let usage = |text: &str| Err(Failure::Usage(text.to_owned()));
    match cli.runtime {
        Some(RuntimeKind::Crun) => {
            if platform.is_macos() {
                return usage("--runtime crun: Linux only: the post-job checks read /proc");
            }
            #[cfg(target_os = "linux")]
            {
                let host = services.machine.crun_host();
                let ready = crun_host::start(cli, &host, services.commands);
                let ready = ready.map_err(Failure::Usage)?;
                for warning in &ready.warnings {
                    let _ = writeln!(err, "js-agent: warning: {warning}");
                }
                let allow = lifecycle::image_allow(&cli.image_allow).map_err(Failure::Usage)?;
                Ok(Some(Settings::for_crun(
                    ready.crun,
                    &ready.job_user,
                    ready.job_ids,
                    allow,
                )))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (services, err);
                usage("--runtime crun: this build has no crun runtime")
            }
        }
        Some(RuntimeKind::Docker) if cli.container_bin.is_none() => {
            usage("--runtime docker: needs --container-bin")
        }
        _ if cli.crun_bin.is_some() => usage("--crun-bin: needs --runtime crun"),
        _ if cli.job_user.is_some() => usage("--job-user: needs --runtime crun"),
        _ => Ok(None),
    }
}

/// The program, with the machine's own services.
///
/// A helper container of the OCI runner runs this binary as `<binary> <mode> <arguments>`
/// (descriptor-relative ownership changes, and the unpack of an image's layers). Such a
/// start is answered before anything else, so the flags are never parsed and no signal
/// handler is installed: the mode words are the runner's default
/// `HelperModes`, the value the runner is configured with (`js_contain::oci::helper`). Only Linux runs containers.
pub fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    if let Some(code) =
        js_contain::modes::dispatch(&js_contain::oci::helper::HelperModes::default())
    {
        return ExitCode::from(code);
    }
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    main_with(
        &SystemSleeper::install,
        std::env::args_os().collect(),
        &mut out,
        &mut err,
    )
}

/// [`main`] with the installation of the signal handlers given, so a test can make it
/// fail. The machine's own commands, files, transport and jitter are used.
pub fn main_with(
    install: &dyn Fn() -> std::io::Result<SystemSleeper>,
    args: Vec<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    #[cfg(target_os = "linux")]
    if args
        .get(1)
        .is_some_and(|verb| verb.to_str() == Some(image::pull::VERB))
    {
        return image::pull::run(&args, out, err, &image::pull::Host::system());
    }
    match install() {
        Err(why) => fail(err, &why, exit::FAILED),
        Ok(mut sleeper) => {
            let services = &mut Services {
                platform: Platform::current(),
                commands: &SystemCommands::default(),
                files: &SystemFiles,
                connector: &Dialer,
                jitter: &mut SystemJitter,
                sleeper: &mut sleeper,
                machine: Arc::new(System::new()),
                bundle: Arc::new(SystemHost::system()),
                long_bundle_values: false,
                worker_hook: None,
                rescue_bound: None,
                euid: rustix::process::geteuid().as_raw(),
            };
            run_with(args, out, err, services)
        }
    }
}

/// The file the transport first reads its bearer from: `--token-file` when given, and
/// otherwise the host credential file of `--state-root`, which has the same form (one run
/// of printable ASCII, owner-only). The agent adopts the credential from the same file
/// again below, so a file that is there and not a credential stops it either way.
///
/// # Errors
///
/// [`Failure::Usage`] naming `--token-file` when there is neither, and the path of the
/// credential file when it is there and cannot be used.
fn bearer_file(cli: &Cli) -> Result<PathBuf, Failure> {
    if let Some(token_file) = &cli.token_file {
        return Ok(token_file.clone());
    }
    let kept = match &cli.state_root {
        Some(root) => {
            let file = CredentialFile::in_dir(root);
            file.load().map_err(Failure::Usage)?.map(|_| file.path())
        }
        None => None,
    };
    kept.ok_or_else(|| {
        Failure::Usage(
            "--token-file: required unless --state-root holds a host credential".to_owned(),
        )
    })
}

/// Runs the agent on `args` (the program name first) until it is stopped or refused.
pub fn run_with(
    args: Vec<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: &mut Services<'_>,
) -> ExitCode {
    let result = execute(args, err, services);
    finish(result, out, err)
}

fn execute(
    args: Vec<OsString>,
    err: &mut dyn Write,
    services: &mut Services<'_>,
) -> Result<(), Failure> {
    let cli = Cli::try_parse_from(args).map_err(Failure::Clap)?;
    let ca_cert = cli.ca_cert.as_deref();
    let bearer_file = bearer_file(&cli)?;
    let mut transport = services
        .connector
        .connect(&cli.server, &bearer_file, ca_cert)
        .map_err(|why| Failure::Usage(why.to_string()))?;
    let version = env!("CARGO_PKG_VERSION");
    let (platform, commands, files) = (&services.platform, services.commands, services.files);
    // The bundle flags are checked before the host registers, so a store that cannot work
    // stops the agent now and not at the first job.
    let host = &services.bundle;
    let bundle = BundleSettings::from_cli(&cli, platform, &**host).map_err(Failure::Usage)?;
    // An agent that runs native jobs opens its store now, before the hello: the hello says
    // it holds bundles only when the store is open, and the runtime is made over the same
    // store. An agent that runs no native job could never run a bundle, so it opens none.
    let opened = match (&bundle, cli.native_helper_socket.is_some()) {
        (Some(settings), true) => host
            .open(settings)
            .map_err(|why| Failure::Usage(format!("--bundle-store: {why}")))?,
        _ => None,
    };
    let store = bundle.as_ref().map(|settings| StoreView {
        settings,
        host: &**host,
        long_values: services.long_bundle_values,
    });
    let mut offer =
        offer(&cli, platform, commands, files, version, store).map_err(Failure::Usage)?;
    offer.warnings.iter().for_each(|warning| {
        let _ = writeln!(err, "js-agent: warning: {warning}");
    });
    let checked = offer.hello.validate();
    checked.map_err(|why| Failure::Usage(format!("the hello is not acceptable: {why}")))?;
    // The hook is checked now for the same reason; the job worker runs it, on its own
    // thread, for a whole-node lease whose host it found quiet. `hook_runner` bounds one
    // run to twice `prepare_timeout`, which the worker's watchdog is told of.
    let hook = ReadinessHook::from_cli(&cli).map_err(Failure::Usage)?;
    let readiness = hook.map(|hook| {
        let runner = Box::new(hook_runner(PREPARE_TIMEOUT));
        WorkerHook::new(hook, runner, PREPARE_TIMEOUT.saturating_mul(2))
    });
    let toolkit = ToolkitHook::from_cli(&cli).map_err(Failure::Usage)?;
    // The post-job checks read /proc, which a Mac does not have: a container job there
    // would run unchecked, so none is accepted (see `integrity`). This comes before the
    // container flags are read, so a Mac is told this rather than what a Linux host needs.
    if platform.is_macos() && cli.container_bin.is_some() {
        return Err(Failure::Usage(
            "--container-bin: container jobs are Linux-only: the post-job checks read /proc, \
             which a Mac does not have"
                .to_owned(),
        ));
    }
    let crun = runtime_flags(&cli, platform, services, err)?;
    // Native jobs need the Seatbelt and the helper of a Mac.
    if cli.native_helper_socket.is_some() && !platform.is_macos() {
        return Err(Failure::Usage(
            "--native-helper-socket: native jobs run on macOS only".to_owned(),
        ));
    }
    // The job flags are checked now so a wrong one stops the agent before it registers;
    // the worker is built from them below, before the first hello.
    let mut local = match crun {
        Some(settings) => Local::from_cli_crun(&cli, services.euid, settings),
        None => Local::from_cli(&cli, services.euid),
    }
    .map_err(Failure::Usage)?;
    local.native = match (local.native.take(), opened) {
        (Some(native), Some(store)) => Some(native.with_bundles(store).map_err(Failure::Usage)?),
        (native, _) => native,
    };
    if let Some(why) = &local.files_warning {
        let _ = writeln!(err, "js-agent: warning: {why}");
    }
    // The store and its claim files go in the directory above the scratch root, which must
    // be on the scratch root's file system (a lease's `/work` and home are written there, and
    // the run's disk is bounded by that file system), and one crun agent may hold each scratch
    // root and each host id: the start-up sweep and the lease names go by them.
    #[cfg(target_os = "linux")]
    let _claims = match local.container.as_ref().and_then(Settings::crun) {
        Some(plan) => {
            crun::prepare_store(&plan.claim_dir, &plan.store).map_err(Failure::Usage)?;
            let (dir, root) = (&plan.claim_dir, &plan.scratch_root);
            if !crun::same_filesystem(services.files, dir, root).map_err(Failure::Usage)? {
                return Err(Failure::Usage(format!(
                    "--scratch-root: {} and {}, where the image store and the leases' /work and \
                     home are kept, are on different file systems: a job could fill the file \
                     system of the store; mount the scratch file system at the directory that \
                     holds the scratch root",
                    root.display(),
                    dir.display()
                )));
            }
            let host = crun::HostClaim::take(dir, &offer.hello.host_id);
            let host = host.map_err(|why| Failure::Usage(why.to_string()))?;
            let scratch = crun::HostClaim::scratch_root(dir, root);
            Some((
                host,
                scratch.map_err(|why| Failure::Usage(why.to_string()))?,
            ))
        }
        None => None,
    };
    local
        .check_filesystems(services.files)
        .map_err(Failure::Usage)?;
    // What the scratch root's file system can bound a run's disk with. With
    // `--scratch-quota` the operator asked for enforcement, so a file system that cannot
    // give it ends the agent, with the probe's reason; without the flag the answer is only
    // logged and nothing is enforced or advertised.
    let scratch_quota = match &local.scratch {
        Some(root) => {
            let found = services.machine.scratch_quota(root.path());
            let probe = found.probe();
            let _ = writeln!(err, "js-agent: scratch quota: {probe}");
            match (local.scratch_quota, probe.mode) {
                (true, quota::Mode::None) => {
                    return Err(Failure::Usage(format!(
                        "--scratch-quota: the scratch root cannot enforce a project quota: {}",
                        probe.reason
                    )));
                }
                (true, quota::Mode::Project) => Some(found),
                (false, _) => None,
            }
        }
        None => None,
    };
    let default_gib = scratch_quota.as_ref().map(|_| local.scratch_default_gib);
    if default_gib.is_some_and(|gib| (gib << 30) > offer.hello.shape.disk_bytes) {
        let _ = writeln!(
            err,
            "js-agent: warning: --scratch-default-gib: {} GiB is more than the {} bytes of disk \
             this host offers, so a run without disk_bytes is refused here",
            local.scratch_default_gib, offer.hello.shape.disk_bytes
        );
    }
    // The agent must see what its jobs do: a container job runs where the agent's own
    // `/proc` and mounts show it.
    if local.container.is_some() {
        let (link, read) = (
            |path: &str| services.files.link(path),
            |path: &str| services.files.read(path),
        );
        integrity::require_host_namespaces(&link, &read)
            .map_err(|why| Failure::Usage(format!("--container-bin: {why}")))?;
    }
    // An agent that runs containers, or native jobs, says so in its hello. The hello is sent
    // only after the job half below has started, and that ends the agent first if the program
    // (or the helper) does not run, so no hello that names a runtime is sent for one that was
    // not found.
    let reported = match (local.container.is_some(), local.native.is_some()) {
        (true, _) => Some(contract::report(
            &mut offer.hello,
            toolkit.as_ref(),
            commands,
            local.files.is_some(),
            default_gib,
        )),
        (false, true) => Some(contract::report_native(&mut offer.hello)),
        (false, false) => None,
    };
    if let Some(found) = reported {
        found.iter().for_each(|warning| {
            let _ = writeln!(err, "js-agent: warning: {warning}");
        });
        let checked = offer.hello.validate();
        checked.map_err(|why| Failure::Usage(format!("the hello is not acceptable: {why}")))?;
    }
    // A host that was given a credential in an earlier life uses it from the first hello:
    // its token is dead. The transport was connected with the token file, or with this very
    // file when no token file was given (above).
    let credentials = local
        .state
        .as_ref()
        .map(|state| CredentialFile::in_dir(state.path()));
    if let Some(file) = &credentials {
        let found = file.load().map_err(Failure::Usage)?;
        if let Some(credential) = found {
            transport
                .adopt_credential(credential.expose())
                .map_err(|why| Failure::Usage(format!("{}: {why}", file.path().display())))?;
        }
    }
    let taint = local.state.iter().filter_map(StateRoot::read);
    taint.for_each(|taint| {
        let _ = writeln!(
            err,
            "js-agent: warning: this host is tainted ({}): the agent starts no job on it",
            taint.reason
        );
    });
    if local.container.is_some() && local.watch.is_empty() {
        let _ = writeln!(
            err,
            "js-agent: warning: no --watch-dir given: after a job only the processes, mounts, \
             kernel modules, /etc/passwd and /etc/group are checked, so a cron entry, a unit, a \
             key or a sudoers line a job leaves behind is not seen; suggested: {}",
            SUGGESTED_WATCH_DIRS.join(", ")
        );
    }
    let seams = Seams {
        machine: &services.machine,
        hook: services.worker_hook,
        rescue_bound: services.rescue_bound,
        readiness,
        bundle: bundle.map(|settings| Admission::new(settings, host.clone())),
        quota: scratch_quota,
    };
    let started = jobs::start(local, &offer.hello, platform, seams, err);
    let jobs = started.map_err(Failure::Usage)?;
    // The session asks the worker about a whole-node lease; the worker answers on its own
    // thread (see `WorkerHost`), so the heartbeat loop never waits for a check or the hook.
    let mut host = jobs
        .as_ref()
        .map(|(worker, ..)| WorkerHost::new(worker.handle()));
    let clock = jobs.as_ref().map(|(worker, ..)| worker.clock());
    let mut session = Session::new(offer.hello, &mut *transport, &mut *services.jitter);
    if let Some((worker, drain, outbox)) = &jobs {
        session = session.with_jobs(worker, *drain).with_usage(outbox.clone());
    }
    if let (Some(host), Some(clock)) = (host.as_mut(), clock.as_ref()) {
        session = session.with_default_quiesce(host, clock);
    }
    if let Some(file) = &credentials {
        session = session.with_credential_store(file);
    }
    match drive(&mut session, &mut *services.sleeper, err) {
        Exit::Stopped => Ok(()),
        Exit::Fatal(fatal) => Err(Failure::Fatal(fatal)),
        Exit::Failed(why) => Err(Failure::Worker(why)),
    }
}

fn finish(result: Result<(), Failure>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::from(exit::OK),
        Err(Failure::Clap(usage)) => {
            let _ = match usage.use_stderr() {
                true => write!(err, "{usage}"),
                false => write!(out, "{usage}"),
            };
            ExitCode::from(u8::try_from(usage.exit_code()).unwrap_or(exit::USAGE))
        }
        Err(Failure::Usage(why)) => fail(err, &why, exit::USAGE),
        Err(Failure::Fatal(fatal)) => fail(err, &fatal, exit::USAGE),
        Err(Failure::Worker(why)) => fail(err, &why, exit::FAILED),
    }
}

/// Prints `message` as the program's last line and returns its exit status.
fn fail(err: &mut dyn Write, message: &dyn Display, status: u8) -> ExitCode {
    let _ = writeln!(err, "js-agent: {message}");
    ExitCode::from(status)
}
