//! The command line: flags only, never environment variables.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// What `--version` prints after the version: the runtimes this build can run jobs with, on a
/// line a script can read (`js-agent --version | grep -x 'runtimes: docker crun'`). A build
/// where `--runtime crun` is wired says `docker crun`; others say `docker`.
#[cfg(target_os = "linux")]
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\nruntimes: docker crun");

/// See the Linux form.
#[cfg(not(target_os = "linux"))]
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\nruntimes: docker");

/// The flags of `js-agent`.
#[derive(Debug, Parser)]
#[command(
    name = "js-agent",
    version,
    long_version = LONG_VERSION,
    about = "Registers this machine with a lease server and keeps it registered"
)]
pub struct Cli {
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is
    /// accepted for a loopback host only: the join token never crosses a network in
    /// clear.
    #[arg(long, value_name = "URL")]
    pub server: String,
    /// A file holding the pool's join token, or the one-time token minted for this
    /// instance: a regular file owned by the user running the agent, mode 0600 or 0400.
    /// The token is sent only in the `Authorization` header. Without `--state-root` a
    /// one-time token is this host's credential for as long as the host stays registered,
    /// so the agent leaves the file in place: it reads it again at every start. With a
    /// state root, the first hello of a host that joins with a one-time token is answered
    /// with a host credential that replaces the token (the server retires it): the agent
    /// keeps the credential in the state root and sends it from then on. Required unless
    /// `--state-root` holds a usable `host-credential` file, which the agent then sends in
    /// place of a token; a token file given beside it is read as before.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// A file holding the instance this machine runs on, as one line `PROVIDER:INSTANCE_ID`
    /// (`lambdalabs:0920582c...`), read when the agent starts. The hello names the
    /// instance, which a one-time join token minted for that instance requires: the server
    /// refuses a hello that names another instance or none. The file is not secret.
    #[arg(long, value_name = "PATH")]
    pub instance_file: Option<PathBuf>,
    /// A PEM file with the certificate to trust for `https://` (the server's, or the
    /// authority that issued it); only those certificates are trusted then. Optional:
    /// without it an `https://` server is verified against the public roots bundled in the
    /// agent (a Mozilla set), never the platform's store.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
    /// The pool this host asks to join.
    #[arg(long, value_name = "NAME")]
    pub pool: String,
    /// The id this host registers under. By default a salted hash of the machine id
    /// (`/etc/machine-id`, or the platform UUID on a Mac), never the machine id itself.
    #[arg(long, value_name = "ID")]
    pub host_id: Option<String>,
    /// A label to put on the host, as `key=value`. May be given more than once; a key
    /// given twice takes its last value.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = key_value)]
    pub labels: Vec<(String, String)>,
    /// A capability to report, as `key=value`. May be given more than once. `os` and
    /// `arch` are detected and cannot be set.
    #[arg(long = "capability", value_name = "KEY=VALUE", value_parser = key_value)]
    pub capabilities: Vec<(String, String)>,
    /// Offer at most this many thousandths of a CPU core. A flag only lowers what is
    /// detected.
    #[arg(long, value_name = "N")]
    pub cpu_millis: Option<u64>,
    /// Offer at most this many bytes of memory. A flag only lowers what is detected.
    #[arg(long, value_name = "N")]
    pub memory_bytes: Option<u64>,
    /// Offer at most this many GPUs; `0` offers none. A flag only lowers what is
    /// detected.
    #[arg(long, value_name = "N")]
    pub gpus: Option<u64>,
    /// Name the offered GPUs' class, instead of the one derived from the detected model.
    /// Allowed only when at least one GPU is offered.
    #[arg(long, value_name = "CLASS")]
    pub gpu_class: Option<String>,
    /// Accept a start whose job is not confined to a container (`isolation = host`). Not
    /// available yet: the scheduler does not record a host job's taint, and the agent
    /// refuses this flag at start. Without it such a start creates nothing and is
    /// refused.
    #[arg(long)]
    pub allow_host_jobs: bool,
    /// A directory jobs may mount from, as `name=path`; the name is what a mount in a
    /// start refers to. May be given more than once. The path must be an existing
    /// directory; it is canonicalised when the agent starts. No root, no mount. With
    /// `--scratch-quota` it may not be on the scratch root's file system; that check sees
    /// the root's own device only, not a scratch device mounted below it (a submount a
    /// recursive bind brings along), which the operator must see to be absent.
    #[arg(long = "mount-root", value_name = "NAME=PATH", value_parser = named_path)]
    pub mount_roots: Vec<(String, PathBuf)>,
    /// A directory jobs may mount from read-only, as `name=path`; like `--mount-root`, and
    /// it shares its namespace of names, except that a start with a read-write mount from
    /// it is refused. Unlike `--mount-root`, it may be on the scratch root's file system
    /// when `--scratch-quota` is given. May be given more than once. The comparison of file
    /// systems sees the root's own device only: a scratch device mounted below a root (a
    /// submount that a recursive bind brings along) is not seen, and the operator must
    /// see to it that there is none.
    #[arg(long = "mount-root-ro", value_name = "NAME=PATH", value_parser = named_path)]
    pub mount_roots_ro: Vec<(String, PathBuf)>,
    /// Bound each run's disk with a project quota on the scratch root's file system. At
    /// start the agent probes the root and refuses to start unless the file system
    /// enforces project quota; then it advertises `scratch-quota=project` and
    /// `scratch-default-gib`, books `--scratch-default-gib` for a run that names no
    /// `disk_bytes`, and gives each run's scratch directory a block limit of the bytes it
    /// books and an inode limit of `--scratch-max-inodes`. It also refuses to start when a
    /// `--mount-root` is on the same file system as the scratch root (a read-write mount
    /// there would be an unbounded path to its disk); `--mount-root-ro` roots may be there.
    /// Needs `--scratch-root`.
    #[arg(long)]
    pub scratch_quota: bool,
    /// The GiB a run that names no `disk_bytes` books and is bounded to, and the
    /// `scratch-default-gib` capability. From 1; default 2. Needs `--scratch-quota`.
    #[arg(long, value_name = "GIB")]
    pub scratch_default_gib: Option<u64>,
    /// The inode limit of each run's scratch directory, so a flood of empty files cannot
    /// exhaust the file system's inodes. From 1; default 1000000. Needs `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_max_inodes: Option<u64>,
    /// The quota calls (`apply`, a usage read, `clear`) that may fail in a row before the
    /// agent probes the scratch root again; if it no longer enforces project quota the agent
    /// starts no run until it does, and says so every minute. From 1; default 3. Needs
    /// `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_quota_failures: Option<u64>,
    /// The seconds a finished run's slot is held for its project's usage to read zero (the
    /// file system frees a removed tree's inodes in the background). After that the slot is
    /// parked, the run's place and booked bytes are given back, and the quota counts one
    /// failure. From 1 to 86400; default 600. Needs `--scratch-quota`.
    #[arg(long, value_name = "SECS")]
    pub scratch_usage_wait_secs: Option<u64>,
    /// The slots the agent may give up on (their usage never read zero within
    /// `--scratch-usage-wait-secs`) over its life before the quota is unavailable: no run
    /// starts, each start is refused `quota-unavailable`, until the agent is restarted. A
    /// run that ends well does not take one back. From 1; default 8. Needs
    /// `--scratch-quota`.
    #[arg(long, value_name = "N")]
    pub scratch_abandon_limit: Option<u64>,
    /// The most GiB of the scratch root's free disk the agent keeps back from jobs: the
    /// disk it offers is the free space of that volume when the agent starts minus this
    /// (0 when this is more). Needs `--scratch-root`. Without it, the free space is offered.
    #[arg(long, value_name = "GIB")]
    pub scratch_low_water_gib: Option<u64>,
    /// The directory under which every run gets a private scratch directory (`0700`). It
    /// must be owned by the user running the agent, with no permission for group or
    /// others.
    #[arg(long, value_name = "PATH")]
    pub scratch_root: Option<PathBuf>,
    /// The directory under which each run's files (`files` of a start: URLs and tokens) are
    /// written, `0555` with `0444` files, and mounted read-only at `/run/js-files`. Made
    /// `0700` when missing. It must be on a `tmpfs` so the secrets never reach a disk; the
    /// agent refuses to start when it is not. A reboot clears it, which is fine: a run does
    /// not survive a reboot. A tmpfs can be swapped to disk when the host has swap, so run hosts
    /// that handle these secrets with swap off or encrypted. It may not be, hold or lie in
    /// the state, scratch or a mount root. Needs `--container-bin`. Without the flag `/run/js-agent/files` is used when it can be
    /// made; when it cannot, the agent runs without run files and does not report the
    /// `run-files` capability.
    #[arg(long, value_name = "PATH")]
    pub run_files_root: Option<PathBuf>,
    /// The directory the agent keeps its taint marker and its host credential in: it must be
    /// owned by the user running the agent, with no permission for group or others.
    /// Required with `--container-bin`. Giving it also lets a server issue the host a
    /// credential at its first hello (a file `host-credential`, mode 0600, which the agent
    /// writes before it uses it and reads at start; a file that is there and unusable stops
    /// the agent). A host that a post-job check found unclean is tainted for good:
    /// the marker survives a restart, and no code of the agent removes it. It may not be,
    /// or lie in, a mount root or the scratch root (a job with a read-write mount could
    /// delete the marker). It must be on a disk of its own: a job that fills a disk the
    /// state root shares makes the marker unwritable. The agent compares the device number
    /// (`st_dev`) of the state root with that of every mount root and the scratch root and
    /// refuses to start when one is equal. That is a best-effort guard, not a guarantee: the
    /// engine's data root and the container layers are not compared, and file systems that
    /// share free space under distinct device numbers (btrfs subvolumes, for one) pass it.
    /// The operator provides the separate disk. The taint belongs to this directory, not to
    /// the machine: a second agent, another state root, or a state root on a file system
    /// that is lost at a reboot (a `tmpfs`) does not know of it, so give each machine one
    /// agent and one state root.
    #[arg(long, value_name = "PATH")]
    pub state_root: Option<PathBuf>,
    /// A directory whose listing (names, sizes, modification and status change times,
    /// inodes, owners and modes of its entries) the post-job checks compare with the
    /// listing taken when the host was last known clean; a change taints the host. May be
    /// given more than once. An absolute path to an existing directory that holds no mount
    /// root, scratch root or state root and lies in none. A directory with more than 4096
    /// entries cannot be checked and taints the host. Only the directory's own entries are
    /// listed, one level deep: a write to an existing file inside a subdirectory is not
    /// seen, and only a name added to, removed from or renamed in the subdirectory moves
    /// its modification time (watch the subdirectory itself to see more). Without any
    /// `--watch-dir` only `/etc/passwd`, `/etc/group`, the processes, the mounts and the
    /// kernel modules are checked, and the agent warns at start; suggested: `/etc`,
    /// `/etc/cron.d`, `/etc/sudoers.d`, `/etc/systemd/system`, `/etc/ld.so.conf.d`,
    /// `/root/.ssh`, `/usr/local/bin` and `/usr/local/sbin`. What a job leaves in a
    /// read-write `--mount-root` is never examined (those directories change with every
    /// job). Needs `--container-bin`.
    #[arg(long = "watch-dir", value_name = "PATH")]
    pub watch_dirs: Vec<PathBuf>,
    /// The absolute path of the `docker` or `podman` program that runs container jobs.
    /// It is started by exactly this path, never looked up on the `PATH`, with an
    /// environment of its own (a fixed `PATH` and `--container-host`, nothing the agent
    /// inherited); without it the agent runs no container. A job that names a GPU needs
    /// the NVIDIA container toolkit with Docker, and with Podman a CDI specification
    /// (`nvidia.com/gpu=<index>`) the operator has generated. Rootless engines are not
    /// supported: the engine gets none of `XDG_RUNTIME_DIR`, `HOME` or the D-Bus session
    /// that a rootless Podman needs (it may then run containers without their limits), so
    /// use a rootful engine. Every job runs in a user namespace mapped onto the
    /// `--container-subuid` range (Podman: per container; Docker: the daemon must run with
    /// `userns-remap` set to that user, or the agent does not start). The agent must see
    /// the host: it must be in the pid and
    /// mount namespaces of `init` (not in a container, and not under a service manager's
    /// `PrivatePIDs`, `PrivateMounts` or `ProtectSystem`), and able to read
    /// `/proc/1/ns` (root, or `CAP_SYS_PTRACE`), or it does not start.
    #[arg(long, value_name = "PATH")]
    pub container_bin: Option<PathBuf>,
    /// The runtime that runs jobs: `docker` (the engine program at `--container-bin`, which
    /// may be Docker or Podman, exactly as before) or `crun` (`js-contain` drives the
    /// distribution's `crun`, run as the `--job-user`; Linux only, and the agent must be
    /// root). Without the flag the runtime is the engine of `--container-bin`, as before. The
    /// packaged unit of the Docker cloud hosts names `--runtime docker`; the unit for the
    /// Linux farm names `--runtime crun`. With `crun` the agent checks the host before it
    /// registers and exits 2 naming every piece that is missing (see `--crun-bin`).
    #[arg(long, value_enum, value_name = "docker|crun")]
    pub runtime: Option<RuntimeKind>,
    /// The `crun` program of `--runtime crun`, by absolute path; by default
    /// `/usr/bin/crun`. It must be the distribution's own file at that path: the
    /// distribution's AppArmor profile for `crun` (the one that allows it a user
    /// namespace where `kernel.apparmor_restrict_unprivileged_userns` is 1) attaches to
    /// that path, so a copy elsewhere is refused by the kernel. Any other path starts the
    /// agent with a loud warning. Needs `--runtime crun`.
    #[arg(long, value_name = "PATH")]
    pub crun_bin: Option<PathBuf>,
    /// The dedicated non-root user every `crun` command of `--runtime crun` runs as, such as
    /// `jsjob`: the account whose ranges in `/etc/subuid` and `/etc/subgid` (at least 65536
    /// ids each) every job's user namespace is mapped onto. It must exist, with a uid and a
    /// gid above 0. The agent stays root. Required with `--runtime crun`, and needs it.
    #[arg(long, value_name = "USER")]
    pub job_user: Option<String>,
    /// The network containers join: the name of a user-defined network. By default
    /// `none`, no network at all. `host` is refused. Any named network gives the job
    /// egress to whatever that network reaches, the host's metadata endpoint included;
    /// the agent warns at start. Needs `--container-bin`.
    #[arg(long, value_name = "NAME")]
    pub container_network: Option<String>,
    /// The `<uid>:<gid>` of the first slot, the base of the users containers run as:
    /// container ids, both above zero (see `--container-subuid`). By default `1000:1000`.
    /// A run in slot `k` (from 0; one slot for each run the agent can hold, 4096) runs as
    /// the base plus `k` for the uid and the gid, so both numbers must be at most 61440.
    /// The run's scratch directory is private to the agent, which gives it to its slot's
    /// host ids: the start of the `--container-subuid` range plus the slot's ids. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "UID:GID")]
    pub container_user: Option<String>,
    /// The user whose subordinate id range every job's user namespace is mapped onto, such
    /// as `jobmap`. When it starts the agent reads that user's range from `/etc/subuid`
    /// and `/etc/subgid` and refuses to start unless both have an entry of at least 65536
    /// ids that does not begin at 0 and holds no id of `/etc/passwd` or `/etc/group`.
    /// Container ids 0 to 65535 are host ids of that range, so no id in a container is a
    /// host id. With Podman every container is created with `--uidmap` and `--gidmap` of
    /// the range. Docker has no such option per container: its daemon must run with
    /// `"userns-remap"` set to this user in `daemon.json`, and the agent refuses to start
    /// when it does not. A second agent on the host whose range overlaps one in use is
    /// refused. The agent must run as root: it gives each run's scratch directory to the
    /// job's mapped user. Required with `--container-bin`, and needs it.
    #[arg(long, value_name = "USER")]
    pub container_subuid: Option<String>,
    /// The engine to talk to, as `unix:///<path>`: a socket on this machine; given to the
    /// program as `DOCKER_HOST` and `CONTAINER_HOST` for its calls only. By default none
    /// is given and the engine uses its own default. Any other address (`tcp://`,
    /// `ssh://`) is refused: the post-job checks read this machine's `/proc`, mounts and
    /// users, and a job that runs on another machine would pass them all. (A socket that
    /// is forwarded to another machine cannot be told from a local one and is not
    /// supported.) Needs `--container-bin`.
    #[arg(long, value_name = "URI")]
    pub container_host: Option<String>,
    /// When a job's image is pulled: `never` (the default: the image must be on the host,
    /// so a job cannot make the host reach a registry of its choosing) or `missing`.
    /// With `never`, the operator puts an image on the host (`docker pull`, or `docker
    /// load` from an archive) before a job names it; a job whose image is not there ends
    /// as `failed_to_start`. Needs `--container-bin`.
    #[arg(long, value_name = "never|missing")]
    pub container_pull: Option<String>,
    /// The size of each container's `/tmp`, such as `64m` (the default) or `1g`. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "SIZE")]
    pub container_tmp_size: Option<String>,
    /// A static host entry for every container, `<dns-name>:<ip>` (IPv4 or IPv6); repeatable,
    /// at most 16; passed at create as `--add-host`. A network made without DNS resolves
    /// nothing, and a presigned URL names its host (the signature covers it), so each
    /// endpoint a job may reach is given here. `host-gateway` and anything that is not an
    /// address are refused. Needs `--container-bin`.
    #[arg(long, value_name = "NAME:IP")]
    pub container_add_host: Vec<String>,
    /// An endpoint a named network may reach, `IPV4:PORT` or `CIDR:PORT` (a prefix of 24
    /// to 32, outside 169.254.0.0/16 and the network's own subnet); repeatable, at most 16.
    /// Required with a named `--container-network`. Before every job is created the agent
    /// reads back the network (a bridge, DNS off, no IPv6) and the nftables table
    /// `inet js_egress` that `deploy/js-agent/egress-network.sh` installs, and refuses the
    /// job (`egress-unchecked`) unless the table holds exactly the rules for these
    /// endpoints. Podman only; `br_netfilter` must be active.
    #[arg(long, value_name = "IP:PORT")]
    pub container_egress_allow: Vec<String>,
    /// The absolute path of the `nft` program the egress check runs, by default
    /// `/usr/sbin/nft`. Needs `--container-egress-allow`.
    #[arg(long, value_name = "PATH")]
    pub container_nft: Option<String>,
    /// The network a job's `crun` container gets: `none` (the default: loopback only, no
    /// route) or `allow-list` (one interface to the host, which forwards only what
    /// `--egress-allow` and `--egress-dns` name and refuses everything else, the metadata
    /// range and all IPv6 first). The agent, as root in the host's network namespace, wires
    /// each container from outside and keeps the firewall; see `docs/design/agent-egress.md`.
    /// Only the `crun` runtime reads it, and the `crun` runtime of this build gives jobs
    /// loopback only, so anything but `none` is refused at start, with either runtime; the Podman network flags above are the
    /// Podman runtime's own and are refused together with this one.
    #[arg(long, value_name = "none|allow-list")]
    pub egress: Option<String>,
    /// An endpoint an `allow-list` may reach, `IPV4:PORT` or `CIDR:PORT` (TCP; a prefix of
    /// 24 to 32, outside 169.254.0.0/16 and the pool); repeatable, at most 16; at least one
    /// is required by `--egress allow-list`.
    #[arg(long, value_name = "IP:PORT")]
    pub egress_allow: Vec<String>,
    /// A resolver an `allow-list` may ask (UDP and TCP, port 53), and the only nameserver of
    /// a job's `/etc/resolv.conf`; an IPv4 address, repeatable, at most 4. Without one a job
    /// resolves no name.
    #[arg(long, value_name = "IP")]
    pub egress_dns: Vec<String>,
    /// The IPv4 network the jobs' addresses are taken from, one /30 each, `<addr>/16` to
    /// `<addr>/24`; by default `198.18.0.0/16`. Needs `--egress allow-list`.
    #[arg(long, value_name = "CIDR")]
    pub egress_pool: Option<String>,
    /// The absolute path of the `ip` program the egress setup runs, by default
    /// `/usr/sbin/ip`. Needs `--egress allow-list`.
    #[arg(long, value_name = "PATH")]
    pub egress_ip: Option<String>,
    /// The absolute path of the `nft` program the egress setup runs, by default
    /// `/usr/sbin/nft`. Needs `--egress allow-list`.
    #[arg(long, value_name = "PATH")]
    pub egress_nft: Option<String>,
    /// The absolute path of the `nsenter` program the egress setup runs to configure a
    /// job's network namespace from outside, by default `/usr/bin/nsenter`. Needs
    /// `--egress allow-list`.
    #[arg(long, value_name = "PATH")]
    pub egress_nsenter: Option<String>,
    /// The control group that holds every container the agent starts, as a directory of
    /// the cgroup version 2 file system below `/sys/fs/cgroup` (such as
    /// `/sys/fs/cgroup/jobscheduler.slice`). The operator makes and delegates it. The
    /// agent starts each container in it (`--cgroup-parent`: the slice name for a
    /// one-component path that ends in `.slice`, which the systemd cgroup driver takes,
    /// and the path below `/sys/fs/cgroup` with a leading `/` otherwise, for the cgroupfs
    /// driver) and reads the CPU and memory of the directory and of each container's
    /// control group to send what each lease costs on the heartbeat. Without it containers
    /// land where the engine puts them and the heartbeat carries no load. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "PATH")]
    pub actions_cgroup: Option<PathBuf>,
    /// The absolute path of a program that checks the host's GPU container toolkit, run
    /// once at start when the host offers a GPU. Exit 0 makes the hello report
    /// `gpu-toolkit`; any other outcome (another exit status, no output within the probe
    /// limit, a program that cannot be started) leaves it out and warns. A pool with GPUs
    /// refuses a host without it (`host-contract`). The program gets a `PATH` of its own
    /// and no arguments, and its output is not read. It should run a container that uses
    /// a GPU, since that is what the capability promises. Needs `--container-bin`.
    #[arg(long, value_name = "PATH")]
    pub gpu_toolkit_hook: Option<String>,
    /// The absolute path of a program the agent runs while it prepares the host for a
    /// whole-node lease, after its own checks: exit 0 reports the host ready, other exit
    /// statuses refuse the lease with a typed reason (75 `noise-over-bound`, 76
    /// `not-quiet`, 77 `setup-failed`, 78 `unsupported`; any other `setup-failed`), and
    /// the first line it prints is the refusal's detail, sent to the scheduler verbatim (cut
    /// to a byte bound), so it must not print secrets. It runs in a process group of its own,
    /// which is killed when the hook overruns. It gets an environment of its
    /// own and nothing of the scheduler's, and `prepare_timeout` for its output to end and
    /// as long again for its exit (twice that in the worst case).
    #[arg(long, value_name = "PATH")]
    pub readiness_hook: Option<String>,
    /// An argument of `--readiness-hook`, in order. May be given more than once. Needs
    /// `--readiness-hook`.
    #[arg(
        long = "readiness-hook-arg",
        value_name = "ARG",
        allow_hyphen_values = true
    )]
    pub readiness_hook_args: Vec<String>,
    /// A registry and namespace an image may be pulled from, `HOST[:PORT]/PREFIX` such as
    /// `ghcr.io/komira-ai/`; repeatable. The prefix is whole path components, and a bare
    /// host (every account on it) is refused at start. An image on no entry is never
    /// pulled. The runtime that pulls with js-oci reads the list; the Docker and Podman
    /// runtimes do not.
    #[arg(long, value_name = "HOST/PREFIX")]
    pub image_allow: Vec<String>,
    /// How many seconds a container gets to stop on its own before it is killed, whenever
    /// the agent stops one: a `Stop` or `Cancel` from the server, a job past its wall-time
    /// limit, the end of a drain. By default 10; at most 3600. Needs `--container-bin`.
    #[arg(long, value_name = "SECONDS")]
    pub stop_grace: Option<u64>,
    /// How many seconds SIGTERM and SIGINT let the running jobs finish before the agent
    /// stops them. The agent announces that it drains at once, takes no new job, and sends
    /// the results of the jobs as they end. By default the stop grace; at most 3600. Needs
    /// `--container-bin`.
    #[arg(long, value_name = "SECONDS")]
    pub drain_wait: Option<u64>,

    /// The bundle store: the directory a Mac keeps unpacked toolchain bundles in (`--bundle-*`
    /// flags need it). Absolute; on the same APFS volume as `--scratch-root` (cloning needs
    /// one volume) mounted with ownership honoured and not `noexec`, and the kernel's
    /// securelevel must be 0 or less; the agent refuses to start otherwise, and on a host
    /// that is not a Mac. Needs `--scratch-root` and `--bundle-budget-gib`. The disk the agent
    /// offers (`disk-gib`) is reduced by the part of the budget the store has not used yet.
    /// The hello reports `bundle-root` (this path) and `bundle` (the manifest digests the
    /// store holds, that the deny and allow lists let through) only when the agent can run a
    /// bundle, which this build cannot, so for now it reports neither. Until a server says it
    /// takes a longer value, `bundle` holds one digest. Roll out servers first, then agents,
    /// then this flag, then placement by `bundle`.
    #[arg(long, value_name = "PATH")]
    pub bundle_store: Option<PathBuf>,
    /// The most GiB the store's trees (and the trees waiting to be deleted) may take
    /// together. Required with `--bundle-store`; above 0.
    #[arg(long, value_name = "GIB")]
    pub bundle_budget_gib: Option<u64>,
    /// New fetches are refused (`bundle-no-space`) while the store volume has less than this
    /// many GiB free. By default 0. Needs `--bundle-store`.
    #[arg(long, value_name = "GIB")]
    pub bundle_low_water_gib: Option<u64>,
    /// Whether a bundle that is not in the store is fetched: `never` (the default: the
    /// operator, or the updater, puts it there first, and a job that names another is
    /// refused `bundle-not-held`) or `missing` (only with `--bundle-trust list`, and at
    /// least one registry flag). This build has no fetcher, so a fetch ends
    /// `bundle-fetch-failed`. Needs `--bundle-store`.
    #[arg(long, value_name = "never|missing")]
    pub bundle_pull: Option<String>,
    /// A registry and namespace a bundle may be fetched from, `HOST[:PORT]/PREFIX`
    /// (`ghcr.io/example/`); not a bare host. A job's bundle must name one when any is
    /// given. May be given more than once. Needs `--bundle-store`.
    #[arg(long = "bundle-registry-allow", value_name = "HOST[:PORT]/PREFIX")]
    pub bundle_registry_allow: Vec<String>,
    /// Like `--bundle-registry-allow`, for a registry on a private address, and for that
    /// registry only. May be given more than once. Needs `--bundle-store`.
    #[arg(
        long = "bundle-registry-allow-private",
        value_name = "HOST[:PORT]/PREFIX"
    )]
    pub bundle_registry_allow_private: Vec<String>,
    /// Minutes a bundle's trees are kept after their last use before they may be evicted.
    /// By default 30. Needs `--bundle-store`.
    #[arg(long, value_name = "MINUTES")]
    pub bundle_min_age_min: Option<u64>,
    /// A root-owned file of manifest digests (`sha256:<hex>`, one a line, `#` comments) the
    /// store never evicts. Needs `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_keep_file: Option<PathBuf>,
    /// A file of manifest digests (same form) that are refused (`bundle-denied`) even when
    /// listed. Read at every start of a job, so a change takes effect at once. Needs
    /// `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_deny_file: Option<PathBuf>,
    /// A file of manifest digests (same form) a fetcher puts in the store ahead of the jobs
    /// that name them. Checked at start; this build has no fetcher. Needs `--bundle-store`.
    #[arg(long, value_name = "PATH")]
    pub bundle_prefetch_file: Option<PathBuf>,
    /// Which digests may run: `list` (the default: only those in `--bundle-allow-file`) or
    /// `digest` (any a job names; for developer hosts only, and never with
    /// `--bundle-pull missing`). Needs `--bundle-store`.
    #[arg(long, value_name = "list|digest")]
    pub bundle_trust: Option<String>,
    /// The allow list for `--bundle-trust list`: manifest digests (same form), one a line.
    /// Required with `list` and must hold a digest; refused with `digest`. Read at every
    /// start of a job, so a change takes effect at once. A job whose digest is not in it is
    /// refused `bundle-not-allowed`.
    #[arg(long, value_name = "PATH")]
    pub bundle_allow_file: Option<PathBuf>,
    /// The read model of a run's profile: `open` (the default) or `allow`. `allow` is not
    /// available yet: the agent refuses it at start.
    #[arg(long, value_name = "open|allow")]
    pub read_model: Option<String>,
}

/// `name=path` split at the first `=`.
fn named_path(text: &str) -> Result<(String, PathBuf), String> {
    key_value(text).map(|(name, path)| (name, PathBuf::from(path)))
}

/// `key=value` split at the first `=`.
fn key_value(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("{text:?} is not key=value"))
}

/// What runs a job: the value of `--runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RuntimeKind {
    /// The engine program of `--container-bin` (Docker or Podman).
    Docker,
    /// `js-contain`'s `crun` backend, over the distribution's `crun`.
    Crun,
}
