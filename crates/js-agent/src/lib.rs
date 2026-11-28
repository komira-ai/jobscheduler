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
//! `js-agent cordon --server <url> --state-root <dir> --for <duration>` (Linux and macOS, run as
//! the user who can read the host's credential) keeps the lease server from placing a lease on
//! this host for a window, and lifts the cordon at the end or on SIGTERM, SIGINT or SIGHUP; see
//! [`cordon`] and docs/runbooks/cordon-a-node.md. It is for an operator's measurement script,
//! which needs one node to itself for an hour or two:
//!
//! ```text
//! js-agent cordon --server https://leases.example:7300 --state-root /var/lib/js-agent \
//!     --for 2h > /run/measure/cordon.log &
//! until grep -q '^idle ' /run/measure/cordon.log; do sleep 5; done   # no lease, not releasing
//! ... measure ...
//! kill -TERM %1; wait %1                                             # prints `lifted`
//! ```
//!
//! `js-agent clear-taint --reason <text> --by <operator>` (Linux, run as root) is an operator's
//! clear of the agent's taint: with no run that lives on the host, it stops the crun unit, keeps
//! `taint.json`, `taint.pinned` and the `owed-*` records aside as `.cleared-<time>-by-<operator>`,
//! records the clear in the state root and starts the unit again, and says how it ended as the
//! update hook does (exit 0, 1 or 75 and one JSON line), so hostkeeper can run it; see
//! `clear_taint`.
//!
//! `js-agent crun-gpu-dropin --out <file>` (Linux, run as root by `js-agent-crun-gpu.service`
//! before each start of `js-agent-crun.service`) writes the systemd drop-in that adds the
//! `--gpu-device`, `--gpu-mount` and `--gpu-env` flags for the host's NVIDIA nodes to the crun
//! unit's `ExecStart`, after checking them as the agent's start does; it writes none on a host
//! without a GPU node; see `gpu_dropin` and docs/runbooks/crun-host-setup.md.
//!
//! `js-agent prepare|cutover|verify|rollback|cleanup [<artifact>] --params <file>` (Linux and
//! macOS, run as root by hostkeeper), also written `js-agent update-hook <step> ...`, is the
//! agent's side of hostkeeper's hook contract: it cordons the host, stops the agent and starts
//! the staged release, and says how it ended in one JSON line. It reads the package manifest's
//! `[params]` table from the file and the host's `--flags-file` for the lease server; see
//! [`update_hook`].
//!
//! # Flags
//!
//! Configuration is flags only: `js-agent` reads no environment variable.
//!
//! | flag | meaning |
//! |---|---|
//! | `--server <url>` | required: `https://host:port`; `http://` only for `localhost`, `127.0.0.0/8` and `::1` |
//! | `--server-address <ip>` | optional, `https://` only: the address of the `--server` host, so a start needs no lookup of the name; the certificate is still checked against the name (see `js_client::give_server_address`) |
//! | `--token-file <path>` | required unless `--host-cert` and `--host-key` are given or `--state-root` holds a usable `host-credential` file: the pool's join token, in a regular file owned by the user running the agent, mode 0600 or 0400 |
//! | `--host-cert <path>`, `--host-key <path>` | both or neither: the host's own certificate and key, which hostkeeper keeps at fixed paths and renews in place (the host group's config package names the two paths in the flags file). The certificate is presented in the TLS handshake in place of a token (the server must be `https://` and run with `--agent-client-ca`); no `Authorization` header is sent, `--token-file` is not read, and no host credential is kept. Both files are read again before each request and a consistent new pair is used from the next one. The key, after following links, must be a regular file owned by root or by the user running the agent, with no access for others and not writable by its group (mode 0600, 0400, 0640 or 0440) |
//! | `--ca-cert <path>` | optional for `https://`: the PEM file with the certificate to trust, and then only that is trusted. Without it the public roots bundled in the agent (`webpki-roots`, pinned by `Cargo.lock`) are trusted, so a publicly trusted endpoint works with no certificate pushed; the platform's roots are never read. Ignored for `http://` |
//! | `--pool <name>` | required: the pool to join |
//! | `--host-id <id>` | the id to register under; by default `h-` and 32 hex digits of a salted SHA-256 of the machine id, never the machine id; `--host-id-from-facts <path>` takes it from the `host_id` of hostkeeper's facts file instead (not with `--host-id`; a file that cannot be used stops the agent) |
//! | `--label k=v` | a label on the host; repeatable |
//! | `--capability k=v` | a capability to report; repeatable; `os`, `arch`, `runtime`, `exec`, `gpu-toolkit`, `gpu-passthrough`, `run-files`, `disk-gib`, `scratch-quota`, `scratch-default-gib`, `bundle`, `bundle-root`, `per-job`, `host-jobs`, `fuse-jobs`, `fuse-uring`, `layered-rootfs`, `egress`, `egress-ports`, `mount-provider.<name>` (a name after that prefix), `vm.slots`, `vm.max_cpus` and `vm.max_mem_gib` are detected and cannot be set |
//! | `--cpu-millis`, `--memory-bytes`, `--gpus` | offer less than the host has; never more |
//! | `--gpu-class <class>` | the class the offered GPUs are reported under, instead of the detected one |
//! | `--allow-host-jobs` | run a start with `isolation = host`: an OCI container run by `crun` as root in the host's own namespaces (pid, network, mount, IPC, UTS, cgroup; no user namespace), with the host's `/` as its root and every capability, in a cgroup of its own below its lease's, which accounts and limits it and whose kill ends it whole on a stop or a cancel. Linux build only; taken with the crun runtime only, and refused at start with `--allow-container-engine` (the container engine runs contained jobs only); the agent then reports `host-jobs`. The server taints a host for good when it places a host job, and the machine must be reimaged before it is used again (docs/runbooks/host-jobs.md). Without the flag a start with `isolation = host` is refused (`host-jobs-not-allowed`) and creates nothing |
//! | `--allow-fuse-jobs` | give a contained job that asks for it (a start with `fuse`) the FUSE device: `/dev/fuse` and `CAP_SYS_ADMIN` of its own user namespace (never the host's), with `io_uring_setup`, `io_uring_enter` and `io_uring_register` allowed in its seccomp filter; its network, `noNewPrivileges` and masked paths are unchanged. Linux build only; taken with the crun runtime only, and refused at start with `--allow-container-engine` (the container engine does not give the device). The agent reports `fuse-jobs` when the kernel is at least 6.14 and `/dev/fuse` is the FUSE device (char 10:229), and `fuse-uring` besides when the fuse module's `enable_uring` is on and `kernel.io_uring_disabled` is 0; otherwise it warns at start, naming what is missing, and reports neither. Without the flag such a start is refused (`fuse-jobs-not-allowed`) and creates nothing (docs/runbooks/fuse-jobs.md) |
//! | `--state-root <path>` | the directory the taint marker is kept in; owned by the agent's user, no permission for group or others; required unless `--no-jobs`. A host that a post-job check found unclean (a process of a job's user still running) or could not check is tainted: the marker survives a restart, and the agent removes it only for a taint that clears itself (see `--taint-recheck-ms`), keeping it aside as `taint.json.cleared-<unix seconds>`; any other taint ends with an operator's `js-agent clear-taint` or a reimage. A change that no contained job can cause (a package install while jobs run: a group or user added, a mount, a started service, a loaded kernel module, a changed watched directory) taints nothing: the post-job checks log it as one line. It may not be, or lie in, a mount root or the scratch root, and it must be on a disk of its own (a job that fills a shared disk makes the marker unwritable). The agent refuses a state root whose `st_dev` equals that of any mount root or the scratch root; that is a best-effort guard, not the guarantee: subvolumes that share free space have distinct device numbers, and the engine's data root and container layers are not compared. The taint belongs to this directory, not to the machine: a second agent, another state root or a state root that is lost at a reboot (`tmpfs`) does not know of it. It also holds the hold file `hold`, written by the step that drains the host for an update: while it is there the agent starts no work (each start is refused `host-held`) and keeps its host cordoned on the lease server, from its first hello on, across restarts (see [`lifecycle::hold`] and [`hold_cordon`]); and the handover file `handover`, written by the update hook just before it stops the agent, which makes that stop leave the runs to the next process instead of draining (see [`lifecycle::handover`]); and the process file `handover-process`, the agent process id and the session's grace that a stop (a hand-over or a drain) leaves, which the next process takes at its start and, within that grace, says hello as, so the server takes the host over at once instead of refusing it `host-live`; and, under an `--egress` ceiling, the egress store, the directory `egress` (mode 0700) with one record `<run>.json` per wired run (its block, level and ports), written when the run is wired and removed when its wiring is taken down, from which the next process takes the wiring over (see [`egress::store`]) |
//! | `--watch-dir <path>` | a directory the post-job checks list (its own entries: names, sizes, modification and status change times, inodes, owners, modes; a write to an existing file inside a subdirectory is not seen) and compare with the listing taken when the host was last known clean; one level deep: a name added to a subdirectory is seen, a write to a file in it is not, so watch the subdirectory itself; repeatable; Linux build only, and not with `--no-jobs`; may not hold, or lie in, a mount root, the scratch root or the state root; more than 4096 entries cannot be checked and are noted as `watch-over-bound` and taint nothing; a change in a watched directory is logged as one `watch-dir-changed` line, never a taint (no contained job can write it, and a package install changes `/etc`). With none, only `/etc/passwd`, `/etc/group`, the processes, the mounts and the kernel modules are looked at, and the agent warns at start; suggested: `/etc`, `/etc/cron.d`, `/etc/sudoers.d`, `/etc/systemd/system`, `/etc/ld.so.conf.d`, `/root/.ssh`, `/usr/local/bin`, `/usr/local/sbin`. What a job leaves in a read-write `--mount-root` is never examined |
//! | `--mount-root <name>=<path>` | a directory jobs may mount from, by name; repeatable; canonicalised when the agent starts; no root, no mount. With the crun runtime it is a prefix of the host paths a job start's layers and mounts name (the longest wins; a link is followed only inside it), opened when the agent starts, and a run start's mount of a named root is refused (`invalid-mount`). With the native runtime (a Mac) it is the root a job start's mounts of host paths lie beneath once their links are followed (checked when the host paths are looked up, so a link inside a root that leads out is refused `input-not-allowed`), and every root is a read deny of every run: a run reads only the mounts it was given, which its profile grants back; keep nothing a job reads without mounting it (a tool) under a root, and a `--bundle-store` or a files root may not lie under one; the trees root (`--native-trees-root`) is the one root that is not a read deny: a run's profile denies reads below it and grants back the run's own trees |
//! | `--mount-root-ro <name>=<path>` | like `--mount-root`, in the same namespace of names, but a start with a read-write mount from it is refused (`invalid-mount`; a job start's, `input-not-allowed`) and nothing is created; it may be on the scratch root's file system under `--scratch-quota`; the `st_dev` comparison sees a root's own device only, not a scratch device mounted below a root (a submount a recursive bind brings along), so the operator must guarantee there is none; on a Mac it is a read deny of every run as `--mount-root` is (but for the trees root, `--native-trees-root`), and a read-write mount whose canonical path lies beneath it is refused `input-not-allowed` |
//! | `--mount-provider <name>=<socket>` | a node-local mount provider (`docs/design/mount-providers.md`): a process on this host that makes the tree a job's layer or mount names as `{provider, ref}`, and serves the protocol `jobscheduler.provider.v1.MountProvider` on the Unix socket `<socket>`; repeatable, each name once. The name is 1 to 32 bytes of `a-z`, `0-9` and `-`, the first a letter; the socket's directory must exist when the agent starts (it does not wait for the provider to start; a provider that restarts does not stop the agent), and the socket must be as `--mount-provider-user` says (looked at before each connect). The hello reports `mount-provider.<name>` where it reports `layered-rootfs`, so a server places a job that names the provider only on such a host. A start that names a provider waits for it in a phase of its own, beside its images, and a run's sources are released after its file system is taken apart on every end path; the directory a provider answers must lie beneath a `--mount-root-ro` (a `--mount-root` for a read-write mount). On Linux taken with the crun runtime only; on macOS a job mounts a provider's tree read-only only, as a link to the directory the provider answered; not with `--no-jobs` |
//! | `--mount-provider-user <name>=<user>` | the user a `--mount-provider` runs as, a user name (looked up when the agent starts, in `/etc/passwd` on Linux and in the directory service on macOS) or a uid; root when not given; repeatable, once for each provider. The provider's socket must belong to that user or root and be writable by no other user (its group may write it: the agent's user reaches it through the socket's group), and the socket's directory must belong to root or that user and be writable by no one else, so only they can have put the socket there. |
//! | `--mount-provider-wait <secs>` | how long a start may wait on its mount providers, from its admission: every `Prepare` and `Mount` call is inside it, and a start that has waited this long fails `input-unavailable` naming the provider and the source. From 1 to 900; default 120. Needs `--mount-provider`. |
//! | `--scratch-quota` | bound each run's disk with an XFS project quota on the scratch root's file system. The agent probes the root at start and refuses to start unless the file system enforces project quota (`prjquota`, ids settable), saying why; it then advertises `scratch-quota=project` and `scratch-default-gib` (a detected pair an operator cannot set), books the default for a run that names no `disk_bytes`, and before each run's scratch directory is given away or written to sets its project id, a block limit of the bytes the run books and an inode limit; after the tree is removed it clears them. It refuses to start when a `--mount-root` is on the same file system as the scratch root (`st_dev`; give such a root as `--mount-root-ro`). Needs `--scratch-root`; a Linux host only |
//! | `--scratch-default-gib <n>` | the GiB a run without `disk_bytes` books and is bounded to under `--scratch-quota`, and the value of `scratch-default-gib`; from 1 to 65535, default 2; needs `--scratch-quota` |
//! | `--scratch-max-inodes <n>` | the inode limit of each run's scratch directory under `--scratch-quota`; from 1 to 4294967295, default 1000000; needs `--scratch-quota` |
//! | `--scratch-quota-failures <n>` | the quota calls (`apply`, a usage read, `clear`) that may fail in a row under `--scratch-quota` before the agent probes the scratch root again; when it no longer enforces project quota the agent starts no run (each start is refused `quota-unavailable`, which taints nothing) and reports it every minute, until a probe passes; from 1, default 3; needs `--scratch-quota` |
//! | `--scratch-usage-wait-secs <n>` | the seconds a finished run's slot is held for its project's usage to read zero under `--scratch-quota`; after that the slot is parked (it takes no run and is read at every step), the run's place and booked bytes are given back, and the quota counts one failure; from 1 to 86400, default 600; needs `--scratch-quota` |
//! | `--scratch-abandon-limit <n>` | the slots the agent may give up on (see `--scratch-usage-wait-secs`) over its life under `--scratch-quota`; a run that ends well does not take one back; at the limit no run starts (each start is refused `quota-unavailable`) and the finding `quota-unhealthy` repeats every minute until the agent is restarted, so the operator must watch `quota-usage-abandoned` and look for files held open on the scratch volume; from 1, default 8; needs `--scratch-quota` |
//! | `--job-tmp-gib <n>` | the GiB of each contained job's `/tmp` and `/var/tmp` with the crun runtime: a directory of the job's own (`.tmp-<lease>` beside the run's directory in the scratch root, its `tmp` bound at `/tmp` and its `var-tmp` at `/var/tmp`, `noexec`, `nosuid`, `nodev`, and `TMPDIR=/tmp`, which a job may not set), on the scratch disk, not in memory, bounded by an XFS project quota of its own (an id per slot, apart from the run's scratch project), so a job that writes more is refused `ENOSPC` and never fills the disk; the directory is removed with the run's. The bytes are not booked from the disk the host offers. With or without `--scratch-quota`: a crun agent whose scratch root cannot enforce a project quota (XFS mounted without `prjquota`) says so at start, reports `quota-unhealthy` every minute and refuses every start `quota-unavailable` until a probe passes. From 1 to 65535, default 1; a Linux host only |
//! | `--scratch-low-water-gib <n>` | GiB of the scratch root's free disk kept back from jobs; the disk offered (`shape.disk_bytes`, and `disk-gib` in GiB) is the free space read at start minus this, 0 when this is more; needs `--scratch-root` |
//! | `--scratch-root <path>` | where each run's private (`0700`) scratch directory goes; owned by the agent's user, no permission for group or others; required unless `--no-jobs`. On a Mac whose scratch root is on the `--state-root`'s file system (as after a boot), the start first mounts the scratch volume the update hook made (`js-agent-scratch`, in the Data volume's container) on it, or ends saying why (see [`lifecycle::scratch_volume`]) |
//! | `--run-files-root <path>` | where each run's files (the `files` of a start: a URL or a token the task reads) are written, in a directory of their own per run, `0555` with `0444` files, mounted read-only at `/run/js-files`. Made `0700` when missing; it must be on a `tmpfs` (the secrets never reach a disk) and the agent refuses to start when it is not. A reboot clears it, which is fine: a run does not survive one. Default `/run/js-agent/files` when that can be made; otherwise the agent runs without run files and does not report the `run-files` capability. tmpfs pages can be swapped to disk when the host has swap: run hosts that handle these secrets with swap off or encrypted. With the native runtime (a Mac, which has no tmpfs) the root is used only when the flag is given, is made `0711`, and is not checked for a tmpfs: the files are on disk unless the flag points at a RAM-backed volume; each run's directory and files are given to the run's user (`0500`, files `0400`) before its program starts, its profile denies writes there, and `JS_FILES_DIR` names it. It may not be, hold or lie in the state, scratch or a mount root, the homes of the native users, or a path native runs may not read. Not with `--no-jobs` |
//! | `--container-bin <path>` | the absolute path of the `docker` or `podman` program that runs container jobs, with `--allow-container-engine` (Linux build only; without `--allow-container-engine` it is refused, since the runtime is then `crun`, which takes no engine flag); never looked up on the `PATH`; started with an environment of its own (a fixed `PATH` and `--container-host`, nothing inherited); without it no container runs; every container gets network none (loopback only: a job that needs a network runs on the crun runtime, see `--egress`); a job that names a GPU needs the NVIDIA container toolkit with Docker and, with Podman, a CDI specification (`nvidia.com/gpu=<index>`) the operator has generated; rootless engines are not supported (the engine gets no `XDG_RUNTIME_DIR`, `HOME` or D-Bus session, and a rootless Podman may then run containers without their limits); every job runs in a user namespace mapped onto the `--container-subuid` range (Podman is given the maps for each container; a Docker daemon must run with `userns-remap` set to that user, or the agent does not start); the agent must share the pid and mount namespaces of `init` and be able to read `/proc/1/ns` (root, or `CAP_SYS_PTRACE`), or it does not start |
//! | the macOS build's flags | the `--native-*` flags, `--vm-driver` and `--vm-store` (macOS build only, `platform::MACOS_ONLY`) have their own table in the module doc of `src/platform/macos.rs` |
//! | `--container-user <uid:gid>` | (Linux build only) the base of the users containers run as, container ids both above zero; by default `1000:1000`. Every run holds a slot, numbered from 0, and runs as the base plus its slot number, uid and gid (`1000:1000`, `1001:1001`, ...), so two runs held at once never share a host uid; the slot is given again only after its last run's cleanup passed (container and scratch gone, no process left), and never if a check found something. Each of the 4096 slots needs an id, so both numbers of the base are at most 61440 (the agent refuses to start otherwise). The agent gives each run's scratch directory to the host ids of its slot's user (the start of the `--container-subuid` range plus each id); needs `--container-bin` |
//! | `--container-subuid <user>` | (Linux build only) required with `--container-bin`: the user whose range in `/etc/subuid` and `/etc/subgid` every job's user namespace (container ids 0 to 65535) is mapped onto, so no container id is a host id. The agent refuses to start unless both files give the user at least 65536 ids, not from 0, holding no id of `/etc/passwd` or `/etc/group`; Podman gets `--uidmap` and `--gidmap` for every container, a Docker daemon must run with `userns-remap` set to the user and onto that range; a second agent on the host with an overlapping range is refused (a lock under `/run/js-agent/subid`); the agent must run as root |
//! | `--container-host <uri>` | (Linux build only) the engine to talk to: a socket on this machine, `unix:///<path>`, passed as `DOCKER_HOST` and `CONTAINER_HOST` to the engine's calls only; by default the engine's own default; `tcp://` and `ssh://` are refused (the post-job checks read this machine, and a job on another one would pass them all); needs `--container-bin` |
//! | `--container-pull <never\|missing>` | (Linux build only) when a job's image is pulled; by default `never`, so a job cannot make the host reach a registry of its choosing: the operator puts an image on the host first (`docker pull`, or `docker load` from an archive), and a job whose image is not there ends as `failed_to_start`; needs `--container-bin` |
//! | `--container-tmp-size <size>` | (Linux build only) the size of each container's `/tmp` (`noexec,nosuid,nodev`; on Podman also of `/var/tmp` and `/run`), such as `64m` (the default) or `1g`; needs `--container-bin` |
//! | `--container-add-host <name>:<ip>` | (Linux build only) a static host entry for every container, passed at create as `--add-host`; repeatable, at most 16; the name is a DNS name, the address IPv4 or IPv6 (`host-gateway` and anything else is refused, a name twice too); needs `--container-bin` |
//! | `--egress <none\|allow-list\|open>` | (Linux build only) the most network a job's `crun` container may ask for (its `network`), the agent's ceiling, reported in the hello as the capability `egress`: `none` (the default) loopback only, no route, for every job; `allow-list` a job may ask for one interface to the host, which forwards only what `--egress-allow` and `--egress-dns` name; `open` a job may also ask for any IPv4 address but the metadata range `169.254.0.0/16`, the host itself and another job (IPv6 and a spoofed source are refused as under `allow-list`); a start above the ceiling is refused `network-above-ceiling` before anything is made and is never run with less; the agent, as root, wires each container that asked for `allow-list` or `open` from outside after its start, while the container's init holds the job until its default route is there, keeps the firewall and checks it before every create of a wired job (`egress-unchecked` when it changed), and takes the wiring down when the run ends or is removed; a `none` job is not wired and waits for nothing (docs/design/agent-egress.md); started once, after the claims of the scratch root and the host id, holding the host's egress (the abstract socket `@js-agent-egress`: one agent with `allow-list` or `open` per host, a second is refused), and a ceiling that cannot be set up ends the agent with status 2; only the crun runtime reads it, so the container engine is refused `allow-list` and `open` at start (its jobs get `none` only) |
//! | `--egress-allow <ip:port>` | (Linux build only) an endpoint an `allow-list` job may reach, `IPV4:PORT` or `CIDR:PORT` (TCP; prefix 24 to 32, outside 169.254.0.0/16 and the pool); repeatable, at most 16; at least one is required by `--egress allow-list`, none by `--egress open`; needs `--egress allow-list` or `open` |
//! | `--egress-dns <ip>` | (Linux build only) a resolver a job with a network may ask (UDP and TCP port 53) and the only nameserver of its `resolv.conf`; repeatable, at most 4; without one a job resolves no name; needs `--egress allow-list` or `open` |
//! | `--host-port <port\|lo-hi>` | a port of the host's loopback a job may reach (`host_ports`), each from 1 to 65535; repeatable, at most 16; a start that names a host port not listed (any, without the flag) is refused `input-not-allowed`; not with `--no-jobs`. Linux: a job reaches each host port it names at a port of its own loopback (its job port), at every network level, through a listener the agent holds in the job's network namespace, and its program starts once each listens; a run adopted after a restart whose listener cannot be made again is stopped, its result's `input_failure` saying `host-port-lost`; needs the crun runtime. macOS: a native job shares the host's loopback and reaches a host port at its own number (one asked at another job port is refused `host-port-remap-unsupported-here`); its Seatbelt profile allows a connect to each host port it names whatever its network level, under `--native-network off` included, and a run adopted after a restart keeps the profile it started with |
//! | `--egress-forward-port <port\|lo-hi>` | (Linux build only) a host port a job may declare (`ports`), each from 1 to 65535; repeatable, at most 16; a start that declares a port not listed is refused `port-above-ceiling`; each declared port of an admitted `allow-list` or `open` job is forwarded from the host's port to the job's (a `none` job that declares one is refused `ports-not-supported`); the hello reports them as `egress-ports`; needs `--egress allow-list` or `open` |
//! | `--egress-pool <cidr>` | (Linux build only) the IPv4 network the jobs' addresses come from, one /30 each, `/16` to `/24`; by default `198.18.0.0/16`; needs `--egress allow-list` or `open` |
//! | `--egress-ip <path>`, `--egress-nft <path>`, `--egress-nsenter <path>` | (Linux build only) the absolute paths of the `ip`, `nft` and `nsenter` programs the egress setup runs, by default `/usr/sbin/ip`, `/usr/sbin/nft` and `/usr/bin/nsenter`; need `--egress allow-list` or `open` |
//! | `--actions-cgroup <path>` | (Linux build only) the cgroup version 2 directory below `/sys/fs/cgroup` that holds every container (made and delegated by the operator): each container is created with `--cgroup-parent` naming it (the slice name for one `.slice` directly below `/sys/fs/cgroup`, the systemd driver; the path with a leading `/` otherwise, the cgroupfs driver), and the heartbeat carries the whole host's CPU busy time (`/proc/stat`), the usage of the directory and of each lease's cgroup (see [`usage`]); without it the heartbeat carries no load, and the agent says so at start; needs `--container-bin` |
//! | `--host-second-samples` | sends in each heartbeat the busy CPU (thousandths of a core) of the whole host for each second of the beat's window and, on Linux, its CPU pressure (`some`, thousandths of the interval), at most 16 values of each, newest last: the worker reads the host's CPU counters (`/proc/stat` and `/proc/pressure/cpu` on Linux, the host's CPU load on a Mac) at most once a second and the heartbeat carries the rates between the readings (see [`usage`]); a host with no readable `/proc/pressure/cpu`, and a Mac, sends the busy values alone, a beat that fails loses its seconds, and the readings ride with the load, so on Linux without an actions control group (no `--actions-cgroup` for the container engine, none delegated for crun) there is no load and the flag does nothing; telemetry only, nothing places or sizes by it; off by default; not with `--no-jobs` |
//! | `--gpu-toolkit-hook <path>` | (Linux build only) the absolute path of a program that checks the GPU container toolkit, run once at start when the host offers a GPU; exit 0 makes the hello report `gpu-toolkit`, anything else leaves it out and warns; needs `--container-bin` |
//! | `--no-jobs` | start an agent that runs no job: it registers, says hello and heartbeats, reports none of the runtime capabilities, and ignores `exclusive`; it is refused with any flag of the runtime this build runs (the flags marked "Linux build only" or "macOS build only"). Without it a Linux agent runs `crun`, and a Mac agent requires `--native-accounts` |
//! | the crun runtime (no flag) | what runs a job in the Linux build: js-contain over the distribution's `crun`, each command run as `--job-user`, unless `--allow-container-engine` is given. There is no `--runtime` flag: the runtime is the build's, and `--runtime` is refused by name. With the crun runtime the agent checks the host before it registers and exits 2 naming every piece that is missing, and never runs the engine instead (the refusal also says that the runtime of this build is crun, points to docs/runbooks/crun-host-setup.md, and names the opt-in to the engine): `crun` at `--crun-bin` and its version (1.14 or newer), `newuidmap` and `newgidmap`, the job user and its subuid and subgid ranges (65536 ids or more each), the state root (`0700`, owned by root) and the scratch root (`0711`, owned by root, every directory above it searchable by the job user), `crun`, `newuidmap` and `newgidmap` executable by the job user, the cgroup version 2 controllers `cpu`, `memory` and `pids` in the agent's own cgroup (`Delegate=yes`), and, where `kernel.apparmor_restrict_unprivileged_userns` is 1, a loaded AppArmor profile for `crun`. Then the worker runs jobs through `CrunRuntime`: images are pulled by the agent's registry client from the `--image-allow` entries into a store beside the scratch root (`oci`), and the roots of `--mount-root` and `--mount-root-ro` bound a job start's host paths, each opened beneath its root and bound by what was opened, never by its path; the hello reports `layered-rootfs` when the kernel has `openat2`, `open_tree`, `move_mount` and `mount_setattr` (5.12 or later; a warning names any it lacks) |
//! | `--allow-container-engine` | Linux build only: run the jobs through the container engine at `--container-bin`, which it needs, instead of `crun`: a fallback for a host that cannot run `crun` and for GPU jobs; every job it runs gets network none, and the agent prints one warning at start that says so. `--container-bin` and the other container flags without it exit 2 (`--container-bin: needs --allow-container-engine`), and it without `--container-bin` exits 2 (`--allow-container-engine: needs --container-bin`). The packaged unit of the Docker cloud hosts names `--allow-container-engine --container-bin /usr/bin/docker`; the unit for the Linux farm (`packaging/js-agent/js-agent-crun.service`) names neither. When the fallback is dropped is an open question of the design (question 7 of komira-ai/jobscheduler#648) |
//! | `--crun-bin <path>` | (Linux build only) the `crun` of the crun runtime; by default `/usr/bin/crun`, the path the distribution's AppArmor profile attaches to; any other path starts the agent with a warning (and is refused by the kernel where user namespaces are restricted) |
//! | `--image-budget-gib <n>` | (Linux build only) the most GiB the unpacked images of the crun runtime may take together: a pull that would go over it first evicts the images used least recently (never one a run holds), and one that still does not fit fails `no-capacity`, evicts nothing and removes the image it brought (only a removal the store refuses partway, another agent on the store having taken the image meanwhile, leaves what was evicted before it evicted); the disk offered is reduced by the part of the budget the store does not hold yet. Above 0; by default no budget; needs the crun runtime |
//! | `--log-file-cap <bytes>` | the bytes of each stream of a crun job with a live-log sink that the capture keeps in its file (default 67108864, 64 MiB; from 1 MiB to 1 GiB): the file is the local backlog the agent uploads from while the store is down, and bytes past it are a gap in the job's live logs (`docs/design/live-logs.md`); a job without a sink keeps the capture's 1 MiB. For a native job with a sink (`--native-accounts`) it is the most bytes of each of the job's log files the agent uploads, the rest a gap, and the files are as before. With the crun runtime or the native runtime the agent reports the capability `log-upload`, holds the host's upload credential from the heartbeat replies in memory only, and reports what it holds in each heartbeat (`log_grant_state`) |
//! | `--cpu-series` | record each job's CPU second by second and send it with the job's result: at most 128 points, each the job's mean CPU over a span of its run in sixteenths of a core, starting at one second a point and combined in pairs by their mean, doubling the span, when a run outlasts them; with the agent's own run length, and marked partial for a run adopted after a restart of the agent. Read from the same samples as the job's usage: of each job's cgroup by the crun runtime (as its CPU peak is; a job of the container engine sends none), of the CPU time of the run's processes by a Mac's native runtime ([`mac_usage`]). Off by default; not with `--no-jobs` |
//! | `--memory-series` | record each job's memory about once a second and send it with the job's result: at most 128 points in MiB, each the most of its span, combined in pairs by their maximum when a run outlasts them; partial for a run adopted after a restart. The crun runtime records the job's non-file memory (`memory.current` less `file - shmem` of its cgroup: no page cache; shared memory and tmpfs kept), a Mac's native runtime the physical footprint of the run's processes together ([`mac_usage`]). The job service books a class's memory from its peak. Off by default; not with `--no-jobs` |
//! | `--job-user <name>` | (Linux build only) the non-root user every `crun` command runs as, whose subuid and subgid ranges every job's user namespace is mapped onto; required with the crun runtime, and needs it |
//! | `--gpu-device <path>` | (Linux build only) a character device below `/dev` a `crun` job that booked a GPU is given; repeatable. With `{index}` (`/dev/nvidia{index}`) it is one GPU's node, given for each index the job booked and no other; without it (`/dev/nvidiactl`, `/dev/nvidia-uvm`) every GPU job gets it. Each node must be a character device at start, for every GPU the host offers, and the host must offer one; the cgroup of a GPU job allows exactly its nodes. With it the crun runtime runs GPU jobs and the hello reports `gpu-passthrough`; without it a start that names a GPU is refused `gpu-unsupported`. Needs the crun runtime |
//! | `--gpu-mount <src>:<dst>` | (Linux build only) a host directory a `crun` GPU job is given read-only (`nosuid,nodev`, not recursive), such as the driver's libraries; repeatable; both absolute, the source a directory not below `/proc`, `/sys` or `/dev`, the destination not at or below those, `/scratch` or a place the runtime mounts itself (`/work`, `/root`, `/tmp`, `/var/tmp`, `/run`, the `/etc` name files), nor hold one of them (`/etc`, `/var`), nor at or below another's. A file source is refused: binding a file where the image has none would leave a file in the image's root that every job of that image shares. A GPU job whose image has a link or a file on the way to the destination, or whose driver directory is gone, is refused when it starts, naming the flag. Needs `--gpu-device` |
//! | `--gpu-env <name>=<value>` | (Linux build only) a variable set in a `crun` GPU job's environment, over the image's of that name, such as the library path; repeatable; the name `[A-Z_][A-Z0-9_]*`, not starting with `JS_`, given once; needs `--gpu-device` |
//! | `--image-allow <host[:port]/prefix>` | a registry and namespace an image may be pulled from, such as `ghcr.io/komira-ai/` (whole path components: `komira-ai/` admits `komira-ai/x`, not `komira-ai-x/y`); repeatable; a bare host is refused at start. An image on none of the entries is never pulled, so a job cannot make the host reach a registry of its choosing. Read at start and kept for the runtime that pulls with js-oci, for a job's image and its image layers; the container engine pulls as `--container-pull` says and do not read it. `--image-allow-loopback <127.0.0.1:port/prefix>` (or `[::1]:port/prefix`) is an entry for a registry on the host's own loopback, pulled over plain http; repeatable, the crun runtime only, and a host that is not a loopback address or an entry without a port is refused at start. A Mac agent with `--bundle-store` also reads it: a start named by its image must be on an entry or it is refused `image-not-allowed`, and `--bundle-pull missing` fetches by it |
//! | `--stop-grace <seconds>` | how long a container gets to stop on its own before it is killed, whenever the agent stops one; by default 10, at most 3600; not with `--no-jobs` |
//! | `--drain-wait <seconds>` | how long SIGTERM lets the running jobs finish before the agent stops them; by default the stop grace, at most 3600; also the longest a stop asked to hand the runs over waits for the starts, stops and post-job checks under way (see [`lifecycle::handover`]); not with `--no-jobs` |
//! | `--settle-ms <milliseconds>` | (Linux build only) how long the agent lets the host settle before it counts what it sees against a job: a process of a job's user is a job's leftover (`process-as-job-user`) only when it is still there this long after the check first saw it (the processes and mounts the host adds of its own are logged, not judged), and a running run whose control group the runtime cannot name taints the host as unchecked (`cgroup-not-found`, which clears itself) only when it is still so this long after the first step that saw it and its container's first process then runs in another group; a host's own short-lived processes and mounts at the moment a job ends, a container not made yet, and a container read as running after it exited and its group was removed, are neither. By default 1000, from 1 to 60000; not with `--no-jobs` |
//! | `--taint-recheck-ms <milliseconds>` | (Linux build only) how long the agent waits between the re-checks of a taint it may clear itself: a taint that a check judged (`residue` of a process of a job's user, `process-as-job-user`, or `unchecked` for the process table the check could not read, or for a run whose container group was not found, `cgroup-not-found`) is re-checked while no run is held, and so is a marker an older agent wrote for a check that no longer taints (`daemonised-child`, `new-mount`, `user-changed`, `group-changed`, `kernel-module-changed`, `watch-dir-changed`: a re-check of it passes unless a job's process is left), and ends after two clean re-checks this far apart and the server's acknowledgement of the clear; the marker is then kept as `taint.json.cleared-<unix seconds>` and the clear is logged (`taint-cleared`). A process still there keeps the host tainted. A residue of a post-job check read from the marker when the agent starts is re-checked too, with one rule that needs no baseline (a process of the job users' range is a finding even if the restart baseline holds it; one whose parent is `init` in a control group a job can be in is only noted); any other taint read from the marker stays. `cleanup-failed`, `check-owed`, `host-job` and an unreadable marker are never cleared by the agent. What a re-check notes (a change of mounts, users, groups, modules or a watched directory) is logged and does not fail it. By default 60000, from 1 to 3600000; not with `--no-jobs` |
//! | `--silence-stop <seconds>` | how long the agent runs its jobs while it hears nothing from the server (contact is a welcome or a heartbeat reply it accepted, one its worker had no room for included, counted on the agent's monotonic clock from the last of them, or from the agent's start before the first): then it stops every job it holds as a stop from the server would (SIGTERM, the stop grace, the kill; a native run's job group signalled and its processes ended after the grace), retires each job's fence (job id and allocation id, in the state root before the stop is reported, so a start of it never runs again here, after a restart too) and reports each `killed` with the detail `silence-stop`; once for each silence. By default 300 (twenty times the server's default grace of 15 s, and more than the 140 s a server restart costs a well agent beside the restart's own time: one heartbeat interval, the 120 s cap on `Retry-After` and the old session's grace), from 1 to 7200; no value turns the stop off. It must be above the stop grace plus the server's default grace (status 2 at start otherwise: a `--stop-grace` of 285 or more needs a longer `--silence-stop`), and a welcome whose grace plus the stop grace is not below it ends the agent (status 1, naming the flag). It runs only while the agent runs; a native run does not outlive the agent's process. Not with `--no-jobs` |
//! | `--health-listen <addr>` | a loopback socket address (`127.0.0.1:7311`; port 0 takes a free port, which is printed at start) on which the agent answers `GET /healthz` for the host's supervisor (see [`health`]): `200` and `placeable` when the session holds a welcome and neither the agent nor the lease server (the `host_taint` of its last reply) holds a taint on the host, `503` and `not placeable: <reason>` otherwise (the reason of the agent's own taint names what changed in one line when the check that found it said so: `the agent holds a taint on this host: <detail>`), `404` for any other request; every change of that verdict is one line in the log. hostkeeper reads it as a package's `healthUrl`, so a tainted host is reported unhealthy and holds a rollout's bake. An address that is not a loopback one, or that cannot be bound, ends the agent with exit status 2 naming the flag; without the flag nothing is bound |
//! | `--bundle-store <path>` | the store of unpacked toolchain bundles (a Mac only): absolute, on the volume of `--scratch-root`, which must be APFS, mounted with ownership honoured and not noexec, with the kernel's securelevel at most 0, or the agent does not start; needs `--scratch-root` and `--bundle-budget-gib`. The hello then carries `bundle-root` (this path) and `bundle` (the held manifest digests, comma-separated, at most 16 of them), and the disk offered is reduced by the part of the budget the store has not used (see [`bundle`]). The hello carries `bundle` and `bundle-root` only when the agent can run a bundle: it runs native jobs (`--native-accounts`) and the store opened; without them it carries neither. A job that pins a bundle runs `@bundle/<entrypoint>` from the store's trees, its writable trees cloned for the run, under the profile of any native job. A start named by its image with a `run` is a bundle start too: the image reference is the bundle's name, pinned by a manifest digest or by the digest of an index that lists a darwin/arm64 manifest, held to the same deny list, allow list and needs as a `bundle` pin, but to the `--image-allow` entries for its registry (`image-not-allowed` on none) and to darwin/arm64 for its `os` and `arch` (`platform-mismatch`); an image start without a `run`, or on an agent with no store, is refused `image-unsupported-here`. And, until a server says it takes a longer value, `bundle` holds one digest (two are 143 bytes, over the 128 an older server accepts and then refuses the hello). Roll out in this order: servers first (they accept the long value and the new codes), then agents, then `--bundle-store`, then placement by `bundle`; no job that names a bundle may be placed on a host that did not report `bundle` |
//! | `--bundle-budget-gib <n>` | the most GiB the store's trees and trash may take; required with a store, above 0 |
//! | `--bundle-low-water-gib <n>` | fetches are refused while the volume has less than this free; default 0 |
//! | `--bundle-pull <never\|missing>` | whether a bundle the store lacks is fetched; default `never` (a job that names one is refused `bundle-not-held`); `missing` needs trust `list` and an `--image-allow`, `--bundle-registry-allow` or `--bundle-registry-allow-private` entry: the first start that names a bundle (by `bundle`, or by its image with a `run`) begins a fetch in the background (`bundle-pending` until it is held; every byte is checked against its digest; a fetch that fails is reported once as `bundle-fetch-failed`) |
//! | `--bundle-registry-allow`, `--bundle-registry-allow-private <host[:port]/prefix>` | the registries and namespaces a bundle's name may point to; repeatable; a bare host is refused; when any is given, a `bundle` name outside them is refused `bundle-registry-not-allowed` (an image reference is held to `--image-allow` instead: on none, `image-not-allowed`) |
//! | `--registry-credentials <host[:port]/prefix>=<path>` | the user and password an entry of `--image-allow`, `--bundle-registry-allow` or `--bundle-registry-allow-private` (spelled as it is there) sends to its registry's token realm, for a private package (a task image or a bundle); repeatable, once per entry. The file holds one line `USER:PASSWORD` (the user up to the first colon; one line ending, `\n` or `\r\n`, is dropped, and anything after it is refused) and must be a regular file, not a link (opened with `O_NOFOLLOW`), owned by root with no permission for group or others (`0600` or `0400`), at most 4096 bytes, in a root-owned directory that group and others cannot write. Read once at start; a flag that is not `ENTRY=PATH`, an entry given twice or on no allow flag, and a file that breaks a rule or holds no `USER:PASSWORD` stop the agent with status 2, naming the flag or the file and never its content. The credentials go only to the token realm the registry's challenge names, when it passes js-oci's realm checks and is on the registry's own host and port, over https: never to a manifest or blob request, a redirect's target, another host, or a plain-http registry. Whoever answers as the registry's host can read them, so give a token that can only pull what the host's jobs need. Nothing the agent prints or records holds them. The `image-pull` verb does not take it |
//! | `--bundle-min-age-min <n>` | minutes a bundle's trees stay after their last use; default 30 |
//! | `--bundle-keep-file`, `--bundle-prefetch-file <path>` | files of manifest digests (`sha256:<hex>` a line, `#` comments), checked at start; the eviction and the fetcher that read them arrive later |
//! | `--bundle-deny-file <path>` | digests refused `bundle-denied`, read at every start of a job |
//! | `--bundle-trust <list\|digest>` | which digests may run: `list` (default) those of `--bundle-allow-file`, which must be given and hold a digest; `digest` any, for developer hosts, and never with `--bundle-pull missing`; a digest off the list is refused `bundle-not-allowed` |
//! | `--bundle-allow-file <path>` | the allow list, read at every start of a job |
//! | `--read-model <open\|allow>` | the read model of the profile of a run that pins a bundle (`open` by default; a run that pins none keeps `open`); needs `--bundle-store`. Under `allow` such a run reads only the base set of the host's macOS version, its run directory, its run files and mounts, its bundle's trees, the kinds its bundle declares and the Xcode its start names. A host whose macOS version has no base set refuses it at start, and at every start, before the hello is sent, the runtime runs the model's self-test as a lent account through the supervisor (each denied place unreadable under the profile and readable without it, the toolchain working under it); a failure ends the agent with status 2, naming every step that was wrong (see [`mac_allow`]) |
//!
//! The job flags are checked at start and a wrong one ends the agent with exit status 2.
//! A marker that is already in the state root is reported on stderr at start.
//! The runtime is the build's, so a flag exists in one build only: the flags marked "Linux
//! build only" (`crun`, the container engine, the job user, the egress flags, the post-job
//! checks; the list is `platform::LINUX_ONLY`) are refused by name in a macOS build, and the
//! flags marked "macOS build only" (the `--native-*` flags, `--vm-driver` and `--vm-store`;
//! `platform::MACOS_ONLY`) in a Linux build; the agent then ends with exit status 2. The
//! removed `--runtime` is refused by name too, with the text of what the build runs instead.
//!
//! # Jobs
//!
//! With `--no-jobs` the agent registers and heartbeats and starts nothing; a `Start` the
//! server sends it is ignored (and counted in the log). A Linux agent otherwise runs `crun`,
//! and a Mac agent the native runtime of `--native-accounts`, which it requires. With
//! `--allow-container-engine` and `--container-bin` a Linux agent runs the container engine
//! (Docker or Podman by their command line) instead. An agent that runs jobs builds its
//! runtime (after the start check of `crun`, which names every piece of the host it misses),
//! takes the baseline of the host for the post-job checks, opens the taint marker, adopts the
//! runs an earlier process of this agent left, and starts the [`Worker`], the thread that
//! owns the [`lifecycle`]: it runs every call into the runtime, so a slow runtime never
//! delays a heartbeat. The first hello lists what was adopted.
//!
//! From then on each heartbeat carries the runs the agent holds, the results the server has
//! not acknowledged and the taint to report; each reply's commands go to the worker in
//! order (a command it refuses is logged by its place in the reply), its result
//! acknowledgements let the worker drop the results it sent, and its `taint_ack` counts
//! only when the heartbeat it answers carried the taint. See [`Session`] and [`Worker`]
//! for the details and [`lifecycle`] for what the agent guarantees per job.
//!
//! The agent that cannot start its jobs' half (the host fails the crun check, the engine is
//! not Docker or Podman or does not answer, the native runtime's account pool cannot open or its self-test fails,
//! the agent cannot see the host, the runs of an earlier process cannot be listed) ends with
//! exit status 2 before it registers.
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
//! and a `--gpu-toolkit-hook` that exits 0 it also carries `gpu-toolkit=true`; a `crun` agent
//! given `--gpu-device` carries `gpu-passthrough=true`, which tells the operator it gives jobs
//! the GPUs they book (it is not a placement rule). An agent that runs no jobs reports none
//! of them, as an older agent does not. A pool that declares an
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
//! server's code on stderr (an agent that runs jobs first stops the runs it holds, bounded,
//! and the results it could not send are lost; the same when the job worker stops, which
//! ends it with status 1, except that the stop does not go through the worker; a run that
//! could not be stopped is named on stderr, and the agent relies on its supervisor and on the
//! next process adopting it, so the supervisor must restart
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
//! A stop that finds the handover file in the state root (the update hook writes it just
//! before it stops the agent to restart it on another release) does not drain, in an agent
//! with jobs: it takes no new job and announces `draining`, waits until no start, stop or
//! post-job check is under way (at most `--drain-wait`), and exits 0 leaving every run as it
//! is for the next process to adopt (see [`lifecycle::handover`] and [`drive`]). Under an
//! egress ceiling each run keeps its network wiring and its firewall rules throughout, and
//! the next process takes the wiring over from the run's record in the egress store (the
//! state root's `egress` directory). An agent removes a handover file it finds at its start.
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
//! [`PREPARE_TIMEOUT`] until a flag sets it, counted on the worker's clock. An agent that runs no
//! job (`--no-jobs`) has no host to prepare, so it ignores `exclusive`. The
//! VM check of the design is a later change. [`Hooked`] is the same hook rule as a wrapper
//! around any [`Quiesce`], for a host driven in-thread.
//!
//! # Flags in a file
//!
//! `--flags-file <PATH>` (or `--flags-file=<PATH>`) is replaced, before the other flags are
//! read, by the words of the file, one flag per line; see [`flags_file`] for the rules of the
//! file (owner, mode, size, form). It is for a package unit that is the same on every host
//! and takes the host's `--server` and `--pool` from `/etc/js-agent/flags`. The flag may be
//! given once, and a flags file may not name another.
//!
//! # Secrets
//!
//! The join token is read from its file by `js-client`, held there, and sent in the
//! `Authorization` header only. The session secret lives in the [`Session`], in the dispatch
//! [`dispatch::Cell`] and in the body of the heartbeats and of the dispatch polls. Neither is
//! in argv, in a log line, in an error or in a `Debug` output.
//!
//! # Exit status
//!
//! 0 after a stop request, 1 when the program cannot install its signal handlers or its
//! job worker stops, 2 for a flag or file that is wrong, a job engine that cannot be used
//! and a server that refuses the host for good.

pub mod agent_process;
mod backoff;
pub mod bundle;
#[cfg(target_os = "linux")]
mod clear_taint;
mod cli;
pub mod cli_runtime;
mod container;
mod contract;
pub mod cordon;
mod credential;
#[cfg(target_os = "linux")]
pub mod crun;
#[cfg(target_os = "linux")]
pub mod crun_host;
pub mod disk;
pub mod dispatch;
pub mod egress;
mod exclusive;
mod facts;
pub mod flags_file;
#[cfg(target_os = "linux")]
pub mod fuse_env;
#[cfg(target_os = "linux")]
mod gpu_dropin;
pub mod health;
mod hidden_verbs;
pub mod hold_cordon;
mod host;
pub mod host_ports;
mod identity;
#[cfg(target_os = "linux")]
pub mod image;
pub mod in_place;
pub mod integrity;
mod jobs;
pub mod leak_scan;
pub mod lifecycle;
mod login;
pub mod logs;
pub mod mac_accounts;
pub mod mac_allow;
pub mod mac_put;
pub mod mac_supervisor;
pub mod mac_sweep;
pub mod mac_usage;
#[cfg(target_os = "macos")]
pub mod native;
pub mod native_fs;
mod os_version;
pub mod own_host;
pub mod platform;
mod pool;
pub mod provider;
pub mod quota;
mod readiness;
mod refused_body;
mod registry_credentials;
mod run;
mod series;
mod session;
mod signals;
#[cfg(target_os = "linux")]
pub mod start_refused;
mod system;
pub mod systemd;
#[cfg(test)]
mod test_tracing;
mod transport;
mod trusted_file;
pub mod update_hook;
pub mod usage;
pub mod vm;
mod warnings;
mod worker;
pub mod xcode;

use std::ffi::OsString;
use std::fmt::Display;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;

pub use backoff::{BASE, Backoff, CAP, Jitter, RETRY_AFTER_CAP, SystemJitter};
use bundle::{Admission, BundleHost, Settings as BundleSettings, SystemHost};
pub use cli::{Cli, FactsError, HostIdFlags, resolve_host_id};
pub use credential::{CREDENTIAL_FILE, CREDENTIAL_TEMP_FILE, CredentialFile, CredentialStore};
pub use exclusive::{PREPARE_TIMEOUT, Quiesce};
pub use facts::{
    APPLE_GPU_CLASS, CommandRunner, Detected, FileReader, Gpus, PROBE_LIMIT, PROBE_OUTPUT_CAP,
    Platform, Ran, Streams, SystemCommands, SystemFiles, detect, host_id, machine_id,
};
use health::{Health, Route};
use hold_cordon::{Keeper, Seams as KeeperSeams};
pub use host::{PreparedHello, StoreView, offer};
pub use identity::{IdentityFlags, IdentitySourceKind};
pub use jobs::DRAIN_SETTLE;
use jobs::Seams;
use lifecycle::{Local, StateRoot};
pub use login::Login;
pub use readiness::{
    EXIT_NOISE_OVER_BOUND, EXIT_NOT_QUIET, EXIT_SETUP_FAILED, EXIT_UNSUPPORTED, HookRunner,
    HookWindow, Hooked, ReadinessHook, Verdict, WorkerHook, hook_runner,
};
pub use registry_credentials::{
    CREDENTIALS_MAX, ImageAllowError, RegistryCredentials, RegistryCredentialsError,
};
pub use run::{Exit, Sleeper, Wake, drive, drive_holding};
pub use session::{
    CredentialState, Drain, Fatal, HEARTBEAT_PATH, HELLO_PATH, Note, Reason, Session, Step,
};
pub use signals::SystemSleeper;
pub use system::{Backend, Machine, RuntimeBuild, SharedClock, System};
pub use transport::{Answer, Connector, Dialer, REQUEST_LIMIT, Transport};
pub use worker::{
    Delivery, Handle, LINES_CAP, LeaseAnswers, Parked, Plan, QUEUE, RESCUE_BOUND, RESCUE_MAX,
    Rescue, STALL_CALLS_MAX, STALL_MARGIN, STEP_EVERY, Snapshot, WIND_DOWN_MARGIN, Worker,
    WorkerHost, stall_bound,
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
    /// Makes the transports of the dispatch poll, which runs on a thread of its own and so
    /// needs a connector it can share (see [`dispatch`]).
    pub dispatch: &'a (dyn Connector + Sync),
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
    /// The directories above a crun scratch root are checked from here down (`/` outside a
    /// test): a test that cannot make `/tmp` root-owned names the tree it made.
    pub ancestors_floor: &'a Path,
    /// The user a `--registry-credentials` file must be owned by: root (0) outside a test,
    /// which cannot make a root-owned file.
    pub secret_owner: u32,
    /// Where a Mac's Xcodes are and whose they must be: [`xcode::Site::system`] outside a test.
    pub xcode_site: xcode::Site<'a>,
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

/// The program, with the machine's own services. A helper container of the OCI runner runs
/// this binary as `<binary> <mode> <arguments>` (ownership changes, the unpack of an image's
/// layers), answered first: no flag is parsed, no signal handler installed (the mode words are
/// the runner's default `HelperModes`, `js_contain::oci::helper`; only Linux runs containers).
/// Every other start writes the agent's warnings to standard error (the module `warnings`).
pub fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    if let Some(code) =
        js_contain::modes::dispatch(&js_contain::oci::helper::HelperModes::default())
    {
        return ExitCode::from(code);
    }
    warnings::install();
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
    if let Some(code) = hidden_verbs::dispatch(&args, out, err) {
        return code;
    }
    // An operator's clear of the taint stops and starts the agent's unit itself: it runs no
    // agent and installs no signal handler.
    #[cfg(target_os = "linux")]
    if args
        .get(1)
        .is_some_and(|verb| verb.to_str() == Some(clear_taint::VERB))
    {
        return clear_taint::run_system(&args, out, err);
    }
    // The GPU drop-in is a short-lived step of systemd's, before the crun unit starts.
    #[cfg(target_os = "linux")]
    if args
        .get(1)
        .is_some_and(|verb| verb.to_str() == Some(gpu_dropin::VERB))
    {
        let smi = |program: &str| SystemCommands::default().run(program, &["-L"]);
        let host = gpu_dropin::Host {
            smi: &smi,
            read: crun::read_node,
            expand: &flags_file::expand,
        };
        return gpu_dropin::run(&args, out, err, &host);
    }
    #[cfg(target_os = "linux")]
    if let Some(code) = start_refused::dispatch(&args, out, err) {
        return code;
    }
    // The updater's hook is answered here too, before `install()`: it is a short-lived
    // step of another program, and installs no signal handler. hostkeeper runs an update step as
    // the bare word (`js-agent prepare ...`) and its other steps behind the verb.
    if args
        .get(1)
        .is_some_and(|word| update_hook::is_hook_word(word))
    {
        return update_hook::run(&args, out, err, &update_hook::Host::system());
    }
    // The cordon verb is an operator's, on any host: it installs its own signal handling, to
    // lift its cordon on a stop, and runs no agent.
    if args
        .get(1)
        .is_some_and(|verb| verb.to_str() == Some(cordon::VERB))
    {
        return cordon::run_system(install, &args, out, err);
    }
    match install().and_then(|sleeper| sleeper.replace_on_hangup().map(|()| sleeper)) {
        Err(why) => fail(err, &why, exit::FAILED),
        Ok(mut sleeper) => {
            let services = &mut Services {
                platform: Platform::current(),
                commands: &SystemCommands::default(),
                files: &SystemFiles,
                connector: &Dialer,
                dispatch: &Dialer,
                jitter: &mut SystemJitter,
                sleeper: &mut sleeper,
                machine: Arc::new(System::new()),
                bundle: Arc::new(SystemHost::system()),
                long_bundle_values: false,
                worker_hook: None,
                rescue_bound: None,
                euid: rustix::process::geteuid().as_raw(),
                ancestors_floor: Path::new("/"),
                secret_owner: 0,
                xcode_site: xcode::Site::system(),
            };
            run_with(args, out, err, services)
        }
    }
}

/// Runs the agent on `args` (the program name first) until it is stopped or refused. After a
/// handover in place (see `in_place`) it runs `args[0]` again over this process.
pub fn run_with(
    args: Vec<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: &mut Services<'_>,
) -> ExitCode {
    run_with_exec(args, out, err, services, &in_place::exec)
}

/// [`run_with`], with the program that runs the command line again after a handover in place
/// given (it returns only when it failed), so a test can end the run in that way.
pub fn run_with_exec(
    args: Vec<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: &mut Services<'_>,
    exec: &dyn Fn(&[OsString]) -> std::io::Error,
) -> ExitCode {
    let original = args.clone();
    match execute(args, err, services) {
        Ok(Ended::Replaced) if services.sleeper.ready_to_run_again() => {
            in_place::run_again(&original, exec, err)
        }
        result => finish(result.map(|_| ()), out, err),
    }
}

/// How the agent ended without a failure.
enum Ended {
    /// A stop request: the host announced it, or drained.
    Stopped,
    /// A replace request handed the runs over: the command line runs again over this process.
    Replaced,
}

fn execute(
    args: Vec<OsString>,
    err: &mut dyn Write,
    services: &mut Services<'_>,
) -> Result<Ended, Failure> {
    // `--flags-file PATH` is replaced by the file's words before anything else reads the
    // arguments, so every later check sees the flags the file holds. Clap never sees the flag.
    let args = flags_file::expand(args).map_err(|why| Failure::Usage(why.to_string()))?;
    // A removed flag is refused by name with what replaces it, before clap would call it
    // unknown, and before anything is read or the agent registers.
    if let Some(refusal) = cli::removed_flag(&args) {
        return Err(Failure::Usage(refusal.to_owned()));
    }
    let cli = Cli::try_parse_from(args).map_err(Failure::Clap)?;
    // The health route is bound before anything else, so a wrong address stops the agent
    // before it registers.
    let route = cli.health_listen.map(Route::bind);
    let route = route.transpose().map_err(Failure::Usage)?;
    // Read once, before anything else: a credentials file that breaks a rule stops the
    // agent before it registers, and the files are never read again.
    let credentials = RegistryCredentials::from_cli(&cli, services.secret_owner)
        .map_err(|why| Failure::Usage(why.to_string()))?;
    // The image client's allow-list, with the credentials: what the crun runtime pulls by.
    let image_allow =
        platform::image_allow(&cli, &credentials).map_err(|why| Failure::Usage(why.to_string()))?;
    let ca_cert = cli.ca_cert.as_deref();
    let login = login::login(&cli)?;
    // The address beside the name, for every client of the server this process makes: the
    // session's, each dispatch poll's and the hold keeper's (js-client keeps one book per
    // server for the process).
    cli.server_address
        .map(|address| js_client::give_server_address(&cli.server, address))
        .transpose()
        .map_err(|why| Failure::Usage(why.to_string()))?;
    let mut transport = services
        .connector
        .connect(&cli.server, &login, ca_cert)
        .map_err(|why| Failure::Usage(why.to_string()))?;
    let version = env!("CARGO_PKG_VERSION");
    let (platform, commands, files) = (&services.platform, services.commands, services.files);
    // Checked before registering: a bad store stops the agent now. A Mac mounts its volume first.
    let host = &services.bundle;
    platform::mount_scratch(&cli, services)?;
    let bundle = BundleSettings::from_cli(&cli, platform, &**host).map_err(Failure::Usage)?;
    let bundle = bundle.map(|settings| settings.with_credentials(credentials.clone()));
    // The runtime's checks that come before the host is described (a macOS build's bundle
    // store is opened now: the hello says it holds bundles only when the store is open).
    let opened = platform::open(&cli, services, bundle.as_ref())?;
    let store = bundle.as_ref().map(|settings| StoreView {
        settings,
        host: &**host,
        long_values: services.long_bundle_values,
    });
    let site = services.xcode_site;
    let mut offer =
        offer(&cli, platform, commands, files, site, version, store).map_err(Failure::Usage)?;
    // A host that authenticates by its own certificate is issued no host credential, and
    // says it keeps none.
    let by_identity = matches!(login, Login::HostCertificate(_));
    offer.hello.accepts_host_credential &= !by_identity;
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
    let gpus: Vec<u32> = offer
        .hello
        .gpu_devices
        .iter()
        .map(|gpu| gpu.index)
        .collect();
    // The runtime of this build and its flags (see `platform`): a wrong flag, or a host that
    // cannot run the runtime, ends the agent before it registers.
    let start = platform::check(
        &cli,
        services,
        image_allow,
        (&gpus, &offer.xcodes),
        opened,
        err,
    )?;
    // The job flags are checked now so a wrong one stops the agent before it registers;
    // the worker is built from them below, before the first hello.
    let mut start = start;
    // A crun agent gives each contained job a temporary directory, which a project quota
    // bounds whatever the flags say.
    let crun = start.take_crun();
    let bounds_tmp = crun.is_some();
    let local = match crun {
        Some(settings) => Local::from_cli_crun(&cli, services.euid, settings),
        None => Local::from_cli(&cli, services.euid),
    };
    let mut local = local.map_err(Failure::Usage)?;
    // The state root the hold file is read from (see `hold_cordon`), kept before the jobs
    // take `local`: an agent with no state root is never held.
    let held_root = local.state.clone();
    platform::complete(&mut local, &cli, &start, services.files).map_err(Failure::Usage)?;
    if let Some(why) = &local.files_warning {
        let _ = writeln!(err, "js-agent: warning: {why}");
    }
    // What the runtime holds while it runs (a crun agent's claims of its host id and scratch
    // root, and its egress), taken once the jobs' flags are known.
    let _claims = platform::claim(&mut local, services, &offer.hello.host_id)?;
    local
        .check_filesystems(services.files)
        .map_err(Failure::Usage)?;
    // What the scratch root's file system can bound a run's disk with. With
    // `--scratch-quota` the operator asked for enforcement, so a file system that cannot
    // give it ends the agent, with the probe's reason. Without the flag a crun agent still
    // bounds each job's temporary directory: a file system that cannot is said here, and the
    // lifecycle starts no run until it can (each start refused `quota-unavailable`, with a
    // finding). Otherwise the answer is only logged and nothing is enforced or advertised.
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
                (false, quota::Mode::None) if bounds_tmp => {
                    let _ = writeln!(
                        err,
                        "js-agent: warning: the scratch root cannot enforce a project quota, \
                         which bounds each job's /tmp and /var/tmp ({}): no job starts here \
                         until it can",
                        probe.reason
                    );
                    Some(jobs::FoundQuota {
                        quota: found,
                        probe,
                        bounds_tmp,
                    })
                }
                (true, quota::Mode::Project) => Some(jobs::FoundQuota {
                    quota: found,
                    probe,
                    bounds_tmp,
                }),
                (false, _) if bounds_tmp => Some(jobs::FoundQuota {
                    quota: found,
                    probe,
                    bounds_tmp,
                }),
                (false, _) => None,
            }
        }
        None => None,
    };
    let default_gib =
        (local.scratch_quota && scratch_quota.is_some()).then_some(local.scratch_default_gib);
    if default_gib.is_some_and(|gib| (gib << 30) > offer.hello.shape.disk_bytes) {
        let _ = writeln!(
            err,
            "js-agent: warning: --scratch-default-gib: {} GiB is more than the {} bytes of disk \
             this host offers, so a run without disk_bytes is refused here",
            local.scratch_default_gib, offer.hello.shape.disk_bytes
        );
    }
    // An agent that runs jobs says so in its hello. The hello is sent only after the job half
    // below has started, and that ends the agent first if the runtime does not
    // run, so no hello that names a runtime is sent for one that was not found.
    let log_grant = platform::log_grant(&local);
    let reported = platform::report(&mut offer.hello, &local, &start, services, default_gib)?;
    if let Some(found) = reported {
        found.iter().for_each(|warning| {
            let _ = writeln!(err, "js-agent: warning: {warning}");
        });
        let checked = offer.hello.validate();
        checked.map_err(|why| Failure::Usage(format!("the hello is not acceptable: {why}")))?;
    }
    // A host that was given a credential in an earlier life uses it from the first hello:
    // its token is dead. The transport was connected with the token file, or with this very
    // file when no token file was given (above). A host that authenticates by its own
    // certificate has no use for one: the file is not read and never adopted.
    let credentials = local
        .state
        .as_ref()
        .filter(|_| !by_identity)
        .map(|state| CredentialFile::in_dir(state.path()));
    let mut kept = None;
    if let Some(file) = &credentials {
        kept = file.load().map_err(Failure::Usage)?;
        if let Some(credential) = &kept {
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
    platform::warn(&local, err);
    // The scratch disk is read again for the heartbeats, over the same volume and store as
    // the hello's.
    let reserve = bundle.as_ref().map(|settings| disk::Reserve {
        settings: settings.clone(),
        host: host.clone(),
    });
    let budget = platform::image_budget(&local);
    let gauge = cli.scratch_root.clone().map(|root| {
        let clock = services.machine.clock();
        let images = disk::ImageReserve::of(budget, &root);
        disk::ScratchDisk::new(root, cli.scratch_low_water_gib, commands, reserve, clock)
            .with_images(images)
    });
    let seams = Seams {
        machine: &services.machine,
        hook: services.worker_hook,
        rescue_bound: services.rescue_bound,
        readiness,
        bundle: bundle.map(|settings| Admission::new(settings, host.clone())),
        quota: scratch_quota,
    };
    // A stop asked to hand the runs over to the next process (see `lifecycle::handover`)
    // leaves them as they are, their egress wiring included, for that process to adopt.
    let handover_root = held_root.clone();
    let started = jobs::start(local, &offer.hello, platform, seams, err);
    let jobs = started.map_err(Failure::Usage)?;
    // A handover file is a request to the process running when it was written: one left
    // there (by an update step that was killed) is removed once the runs are adopted, so this
    // process's stop drains unless the next request is made of it.
    let leftover = held_root.as_ref().filter(|_| jobs.is_some());
    match leftover.map(|root| lifecycle::handover::remove(root.path())) {
        Some(Ok(true)) => {
            let _ = writeln!(
                err,
                "js-agent: removed a handover file left by an earlier stop"
            );
        }
        Some(Err(why)) => {
            let _ = writeln!(
                err,
                "js-agent: warning: a handover file left by an earlier stop cannot be removed \
                 ({why}): a stop of this process may hand its runs over instead of draining"
            );
        }
        Some(Ok(false)) | None => {}
    }
    // The session asks the worker about a whole-node lease; the worker answers on its own
    // thread (see `WorkerHost`), so the heartbeat loop never waits for a check or the hook.
    let mut host = jobs
        .as_ref()
        .map(|(worker, ..)| WorkerHost::new(worker.handle()));
    let clock = jobs.as_ref().map(|(worker, ..)| worker.clock());
    let cell = dispatch::Cell::new(kept);
    let health = Health::default();
    let host_id = offer.hello.host_id.clone();
    let mut session = Session::new(offer.hello, &mut *transport, &mut *services.jitter)
        .with_agent_process(agent_process::at_start(leftover.map(StateRoot::path), err));
    if let Some((worker, drain, outbox)) = &jobs {
        // The worker rings the sleeper's bell when it has news, so a start taken and its
        // `running` are reported at once (`Session::beats_on_news`).
        let bell = services.sleeper.bell();
        bell.into_iter().for_each(|bell| worker.ring_on_news(bell));
        session = session
            .with_jobs(worker, *drain)
            .with_usage(outbox.clone())
            .with_dispatch(&cell);
        session = session.with_handover(handover_root.as_ref().map(StateRoot::path));
    }
    if let (Some(host), Some(clock)) = (host.as_mut(), clock.as_ref()) {
        session = session.with_default_quiesce(host, clock);
    }
    if let Some(file) = &credentials {
        session = session.with_credential_store(file);
    }
    if let Some(gauge) = &gauge {
        session = session.with_disk(gauge);
    }
    if let Some(grant) = log_grant {
        session = session.with_log_grant(grant);
    }
    // The route answers from what the session tells `health`; an agent with no route keeps none.
    if let Some(route) = &route {
        session = session.with_health(&health);
        let _ = writeln!(
            err,
            "js-agent: health route: http://{}{}",
            route.addr(),
            health::PATH
        );
    }
    // The cordon of a held host is asked on the lease server the agent registers with, with
    // the credential it registered with, after each exchange (see `hold_cordon`).
    let connect =
        |root: &Path| hold_cordon::connect_http(&cli.server, &login, ca_cert, &host_id, root);
    let hold_clock = services.machine.clock();
    let mut keeper = held_root.as_ref().map(|state| {
        Keeper::new(KeeperSeams {
            state,
            // The owner the state root and the lifecycle's look at the hold file check for.
            owner: rustix::process::geteuid().as_raw(),
            host: &host_id,
            connect: &connect,
            clock: &*hold_clock,
        })
    });
    // The dispatch poller runs beside the heartbeat loop for as long as the loop does.
    let dial = dispatch::Dial {
        connector: services.dispatch,
        server: &cli.server,
        login: &login,
        ca_cert,
    };
    let stop = AtomicBool::new(false);
    let exit = std::thread::scope(|scope| {
        if let Some((worker, ..)) = &jobs {
            let (cell, dial) = (&cell, &dial);
            scope.spawn(move || dispatch::serve(cell, dial, worker, Box::new(SystemJitter)));
        }
        if let Some(route) = &route {
            let (health, stop) = (&health, &stop);
            scope.spawn(move || route.serve(health, stop));
        }
        let exit = drive_holding(&mut session, &mut *services.sleeper, err, keeper.as_mut());
        cell.close();
        stop.store(true, Ordering::SeqCst);
        route.iter().for_each(Route::wake);
        exit
    });
    match exit {
        Exit::Stopped => Ok(Ended::Stopped),
        Exit::Replaced => Ok(Ended::Replaced),
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
