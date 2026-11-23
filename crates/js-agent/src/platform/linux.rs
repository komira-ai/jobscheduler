//! The Linux build: jobs run in containers that `crun` makes (`js-contain`, see
//! [`crate::crun`]), run as the `--job-user`; or, for a host that cannot run `crun` and opted in
//! with `--allow-container-engine`, through the container engine at `--container-bin` (Docker or
//! Podman, [`crate::cli_runtime`]), whose jobs get network none. A start with `--no-jobs` runs
//! no job. The native runtime and its VMs are not built here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cli::Cli;
use crate::cli_runtime::{CliRuntime, Flags as EngineFlags, Settings, system_runner, unix_ms};
use crate::contract::ToolkitHook;
use crate::integrity::Integrity;
use crate::lifecycle::CheckHook;
use crate::lifecycle::{Local, MountProviders, Runtime, UserNames};
use crate::registry_credentials::RegistryCredentials;
use crate::system::{Machine, RuntimeBuild, System};
use crate::{Failure, Services, crun, crun_host, disk};

mod claims;
pub(crate) use claims::{claim, report};

/// What `--version` prints after the version: the runtimes this build runs jobs with, on a
/// line a script can read (`js-agent --version | grep -x 'runtimes: docker crun'`): `crun`, and
/// the container engine (`docker`, Docker or Podman) behind `--allow-container-engine`; then the
/// ways its stop can go: `drain`, and `handover`, which the update hook looks for in the agent
/// it is to stop before it asks that agent to leave its runs to the next process (see
/// [`crate::lifecycle::handover`]); and `egress-handover`, which the hook looks for in both
/// agents of an update on a host with an egress ceiling: the one it stops leaves the runs'
/// network wiring in place, and the one it starts adopts it; and `in-place`: SIGHUP to the main
/// process, with a handover request in the state root, hands the runs over and runs the program
/// again in the same process, so the unit is not stopped and systemd starts nothing.
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nruntimes: docker crun\nstops: drain handover egress-handover in-place"
);

/// The one warning of an agent that runs its jobs through the container engine.
pub const ENGINE_WARNING: &str = "--allow-container-engine: the container engine runs this \
     host's jobs, and gives none of them a network (network none): it is a fallback for a host \
     that cannot run crun";

/// What a start whose host cannot run `crun` is told after why, on a line of its own: the
/// runtime of this build is `crun`, and the two ways forward.
pub const NOT_READY: &str = "the runtime of this Linux build is crun, and the agent does not \
     fall back to another. Either make this host ready for crun (fix what is named above; see \
     docs/runbooks/crun-host-setup.md), or opt in to the container engine, a fallback whose jobs \
     get network none: --allow-container-engine --container-bin <path>";

/// The refusal of a flag that only a macOS build has (`name` is a flag of
/// [`super::MACOS_ONLY`], without its `--`): it is refused by name, before clap would call it
/// unknown.
pub fn foreign_flag(name: &str) -> String {
    format!(
        "--{name}: macOS only (a flag of the native runtime): this Linux build runs its jobs with \
         crun, or with the container engine of --allow-container-engine"
    )
}

/// The flags of the Linux build's runtimes, flattened into [`Cli`]: none of them exists in a
/// macOS build.
#[derive(Debug, clap::Args)]
pub struct Flags {
    /// Accept a start whose job is not confined to a container (`isolation = host`): `crun`
    /// runs it as root in the host's namespaces, with the host's `/` as its root, in a cgroup
    /// of its own below its lease's. Taken with the crun runtime only; the agent then reports
    /// `host-jobs`. The server taints a host for good when it places a host job: the machine
    /// must be reimaged. Without it such a start creates nothing and is refused.
    #[arg(long)]
    pub allow_host_jobs: bool,
    /// Give a contained job that asks for it the FUSE device: `/dev/fuse` and `CAP_SYS_ADMIN`
    /// of its own user namespace, io_uring allowed, network unchanged. Taken with the crun
    /// runtime only. The agent reports `fuse-jobs` when the kernel is at least 6.14 and
    /// `/dev/fuse` is the FUSE device, and `fuse-uring` when the fuse module's `enable_uring`
    /// is on and `kernel.io_uring_disabled` is 0; otherwise it warns, naming what is missing,
    /// and reports neither. Without it such a start creates nothing and is refused.
    #[arg(long)]
    pub allow_fuse_jobs: bool,
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
    /// job). Not with `--no-jobs`.
    #[arg(long = "watch-dir", value_name = "PATH")]
    pub watch_dirs: Vec<PathBuf>,
    /// The absolute path of the `docker` or `podman` program that runs container jobs, with
    /// `--allow-container-engine`; given without it the flag is refused, because the runtime
    /// is then `crun`, which takes no engine flag.
    /// It is started by exactly this path, never looked up on the `PATH`, with an
    /// environment of its own (a fixed `PATH` and `--container-host`, nothing the agent
    /// inherited); without it the agent runs no engine (a Linux agent runs `crun` unless
    /// `--no-jobs` is given). A job that names a GPU needs
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
    /// Run the jobs through the container engine at `--container-bin`, which it needs.
    /// The engine is a fallback for a host that cannot run `crun` and for GPU jobs: every
    /// job it runs gets network none, and the agent says so in a warning at start. Without
    /// the flag the agent runs `crun` and refuses `--container-bin`; the flag without
    /// `--container-bin` exits 2.
    #[arg(long)]
    pub allow_container_engine: bool,
    /// The `crun` program of the crun runtime, by absolute path; by default
    /// `/usr/bin/crun`. It must be the distribution's own file at that path: the
    /// distribution's AppArmor profile for `crun` (the one that allows it a user
    /// namespace where `kernel.apparmor_restrict_unprivileged_userns` is 1) attaches to
    /// that path, so a copy elsewhere is refused by the kernel. Any other path starts the
    /// agent with a loud warning. Needs the crun runtime.
    #[arg(long, value_name = "PATH")]
    pub crun_bin: Option<PathBuf>,
    /// The most GiB the unpacked images of the crun runtime (the store beside the scratch
    /// root) may take together. A pull that would take the store over it first evicts the
    /// images used least recently, never one a run holds; one that cannot fit even so fails
    /// as `no-capacity`, evicting nothing, and the image it brought is removed again. Only
    /// when the store refuses a removal partway (another agent on the store took the image
    /// meanwhile) does such a failed pull leave the images it evicted before evicted. The
    /// disk the agent offers
    /// (`disk-gib`) is reduced by the part of the budget the store does not hold yet. Above
    /// 0; by default there is no budget and no image is evicted. Needs the crun runtime.
    #[arg(long, value_name = "GIB")]
    pub image_budget_gib: Option<u64>,
    /// A ceiling on each job's CPU, as a multiple of the CPU it booked (`1`, `1.5`, `2`; from
    /// 1 to 1000, at most three decimals). A job is always guaranteed the CPU it booked: its
    /// lease's `cpu.weight` keeps that when the host is busier than planned. By default there
    /// is no ceiling and a job also uses every core no other job wants. With one, the lease
    /// and the job's own cgroup get a `cpu.max` of the booking times the multiple, so a job
    /// is held to it and reads it as its CPU count (`--cpus` on the container engine); at 1 a
    /// job is held to its booking.
    #[arg(long, value_name = "MULTIPLE")]
    pub cpu_ceiling: Option<js_contain::CpuCeiling>,
    /// Record each job's CPU second by second and send it with the job's result
    /// (`cpu_series`): at most 128 points of a sixteenth of a core, a granularity that
    /// doubles when a run outlasts them, and the agent's own run length. Read from the same
    /// samples of each job's cgroup as the job's CPU peak. Off by default. Taken by the crun
    /// runtime only: a job of the container engine records none.
    #[arg(long)]
    pub cpu_series: bool,
    /// The dedicated non-root user every `crun` command of the crun runtime runs as, such as
    /// `jsjob`: the account whose ranges in `/etc/subuid` and `/etc/subgid` (at least 65536
    /// ids each) every job's user namespace is mapped onto. It must exist, with a uid and a
    /// gid above 0. The agent stays root. Required with the crun runtime, and needs it.
    #[arg(long, value_name = "USER")]
    pub job_user: Option<String>,
    /// A character device a job of the crun runtime that booked a GPU is given, by its
    /// absolute path below `/dev`; repeatable. A path with `{index}` (`/dev/nvidia{index}`)
    /// is the node of one GPU, given for each GPU index the job booked and for no other; a
    /// path without it (`/dev/nvidiactl`, `/dev/nvidia-uvm`) is given to every job that
    /// booked a GPU. At start each must be a character device, for every GPU the host
    /// offers, and the host must offer one; a missing node stops the agent. A job that
    /// books no GPU is given none. With it the crun runtime runs GPU jobs and the hello
    /// reports `gpu-passthrough`. Needs the crun runtime.
    #[arg(long = "gpu-device", value_name = "PATH")]
    pub gpu_devices: Vec<String>,
    /// A host directory a job of the crun runtime that booked a GPU is given read-only
    /// (`nosuid`, `nodev`, and not recursive: no mount below it reaches the job), as
    /// `SRC:DST`: the driver's libraries and programs; repeatable. Both are absolute. The
    /// source must be a directory, not below `/proc`, `/sys` or `/dev`. The destination may
    /// not be at or below those, `/scratch` or a place the runtime mounts itself (`/work`,
    /// `/root`, `/tmp`, `/var/tmp`, `/run`, `/etc/hosts`, `/etc/hostname`,
    /// `/etc/resolv.conf`), nor hold one of them (`/etc`, `/var`), nor be at or below
    /// another's. A file source is refused: binding a
    /// file where the image has none would leave a file in the image's root that every job of
    /// that image shares. A GPU job is refused when it starts if its image has a link or a file
    /// on the way to the destination, or the directory is gone. Needs `--gpu-device`.
    #[arg(long = "gpu-mount", value_name = "SRC:DST")]
    pub gpu_mounts: Vec<String>,
    /// A variable set in the environment of a job of the crun runtime that booked a GPU,
    /// over the image's variable of the same name: `NAME=VALUE`, such as the driver's
    /// library path; repeatable. The name is `[A-Z_][A-Z0-9_]*`, does not start with `JS_`
    /// and is given once. Needs `--gpu-device`.
    #[arg(long = "gpu-env", value_name = "NAME=VALUE")]
    pub gpu_env: Vec<String>,
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
    /// at most 16; passed at create as `--add-host`. `host-gateway` and anything that is not
    /// an address are refused. Needs `--container-bin`.
    #[arg(long, value_name = "NAME:IP")]
    pub container_add_host: Vec<String>,
    /// The most network a job's `crun` container may ask for, the agent's ceiling (a job
    /// asks with its `network`; one above the ceiling is refused `network-above-ceiling`,
    /// never run with less): `none` (the default: every job has loopback only, no route),
    /// `allow-list` (a job may ask for one interface to the host, which forwards only what
    /// `--egress-allow` and `--egress-dns` name and refuses everything else, the metadata
    /// range and all IPv6 first) or `open` (a job may also ask for any IPv4 address but the
    /// metadata range, the host and another job; IPv6 is dropped). The agent, as root in the
    /// host's network namespace, wires each container that asked for a network from outside
    /// and keeps the firewall; see `docs/design/agent-egress.md`. The hello reports the
    /// ceiling as the capability `egress`. Only the crun runtime reads it, so another runtime
    /// is refused `allow-list` and `open` at start: a job run through the container engine
    /// (`--container-bin`) always has network none.
    #[arg(long, value_name = "none|allow-list|open")]
    pub egress: Option<String>,
    /// An endpoint an `allow-list` job may reach, `IPV4:PORT` or `CIDR:PORT` (TCP; a prefix
    /// of 24 to 32, outside 169.254.0.0/16 and the pool); repeatable, at most 16; at least
    /// one is required by `--egress allow-list`, none by `--egress open`.
    #[arg(long, value_name = "IP:PORT")]
    pub egress_allow: Vec<String>,
    /// A resolver a job with a network may ask (UDP and TCP, port 53), and the only
    /// nameserver of its `/etc/resolv.conf`; an IPv4 address, repeatable, at most 4. Without
    /// one a job resolves no name. Needs `--egress allow-list` or `open`.
    #[arg(long, value_name = "IP")]
    pub egress_dns: Vec<String>,
    /// A host port a job may declare (`ports`), `PORT` or `LO-HI`, each from 1 to 65535;
    /// repeatable, at most 16. A job that declares a port not listed is refused
    /// `port-above-ceiling`. Each declared port of an admitted `allow-list` or `open` job is
    /// forwarded from the host's port to the job's (a `none` job that declares one is refused
    /// `ports-not-supported`), and the hello reports them as `egress-ports`. Needs
    /// `--egress allow-list` or `open`.
    #[arg(long, value_name = "PORT|LO-HI")]
    pub egress_forward_port: Vec<String>,
    /// A port of the host's loopback a job may reach (`host_ports`), `PORT` or `LO-HI`, each
    /// from 1 to 65535; repeatable, at most 16. A job reaches each host port it names at a port
    /// of its own loopback, whatever its network level, through a listener the agent holds
    /// in the job's network namespace; a job that names a host port not listed is refused
    /// `input-not-allowed`, as is every job that names one when none is given. Needs
    /// the crun runtime.
    #[arg(long, value_name = "PORT|LO-HI")]
    pub host_port: Vec<String>,
    /// The IPv4 network the jobs' addresses are taken from, one /30 each, `<addr>/16` to
    /// `<addr>/24`; by default `198.18.0.0/16`. Needs `--egress allow-list` or `open`.
    #[arg(long, value_name = "CIDR")]
    pub egress_pool: Option<String>,
    /// The absolute path of the `ip` program the egress setup runs, by default
    /// `/usr/sbin/ip`. Needs `--egress allow-list` or `open`.
    #[arg(long, value_name = "PATH")]
    pub egress_ip: Option<String>,
    /// The absolute path of the `nft` program the egress setup runs, by default
    /// `/usr/sbin/nft`. Needs `--egress allow-list` or `open`.
    #[arg(long, value_name = "PATH")]
    pub egress_nft: Option<String>,
    /// The absolute path of the `nsenter` program the egress setup runs to configure a
    /// job's network namespace from outside, by default `/usr/bin/nsenter`. Needs
    /// `--egress allow-list` or `open`.
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
    /// Sends, in each heartbeat, the busy CPU and the CPU pressure of the whole host for each
    /// second of the beat's window (at most 16 values of each, newest last): the agent reads
    /// `/proc/stat` and `/proc/pressure/cpu` every second and the heartbeat carries the rates
    /// between those readings. Telemetry only; the lease server lists it on `GET /v1/hosts`
    /// and nothing places or sizes by it. The readings ride with the load, so without an
    /// actions control group (no `--actions-cgroup` for the container engine, none delegated
    /// for crun) there is no load and the flag does nothing; a host with no
    /// `/proc/pressure/cpu` sends the busy values alone. Off by default.
    #[arg(long)]
    pub host_second_samples: bool,
    /// The absolute path of a program that checks the host's GPU container toolkit, run
    /// once at start when the host offers a GPU. Exit 0 makes the hello report
    /// `gpu-toolkit`; any other outcome (another exit status, no output within the probe
    /// limit, a program that cannot be started) leaves it out and warns. A pool with GPUs
    /// refuses a host without it (`host-contract`). The program gets a `PATH` of its own
    /// and no arguments, and its output is not read. It should run a container that uses
    /// a GPU, since that is what the capability promises. Needs `--container-bin`.
    #[arg(long, value_name = "PATH")]
    pub gpu_toolkit_hook: Option<String>,
    /// A registry on this host's own loopback that a crun job's image, or an
    /// image of its layers, may be pulled from over plain http: `127.0.0.1:PORT/PREFIX` or
    /// `[::1]:PORT/PREFIX`; repeatable. Another host, or an entry with no port, is refused at
    /// start. `--registry-credentials` may name it as any other entry.
    #[arg(long, value_name = "127.0.0.1:PORT/PREFIX")]
    pub image_allow_loopback: Vec<String>,
    /// How many milliseconds the agent lets the host settle before it counts what it sees
    /// against a job: a process of a job's user that was not on the host before the job counts as the job's
    /// leftover (`process-as-job-user`) only when it is still there this long after the check
    /// first saw it (the processes and mounts the host adds of its own are logged, not judged), and a running run whose control
    /// group the runtime cannot name taints the host as unchecked only when it is still so
    /// this long after the first step that saw it. A host's own short-lived processes and
    /// mounts at the moment a job ends and a container read as running a step after it
    /// exited are neither. By default 1000; from 1 to 60000. Not with `--no-jobs`.
    #[arg(long, value_name = "MILLISECONDS")]
    pub settle_ms: Option<u64>,
    /// How many milliseconds the agent waits between the re-checks of a taint it may clear
    /// itself: a taint that a check judged (a process that outlived a job, or a check that
    /// could not read its input; a process of a job's user that outlived a job, a check that could not read its input, or a marker an older agent wrote for a check that no longer taints; never a failed removal, owed checks or a host job. What a re-check notes does not fail it) is re-checked while no run is held, and
    /// ends after two clean re-checks this far apart and the server's acknowledgement of the
    /// clear; the marker is kept as `taint.json.cleared-<time>` and the clear is logged. A
    /// process still there keeps the host tainted. By default 60000; from 1 to 3600000. Not
    /// with `--no-jobs`.
    #[arg(long, value_name = "MILLISECONDS")]
    pub taint_recheck_ms: Option<u64>,
}

impl Flags {
    /// The flags of the GPUs a crun job is given, with whether each was given: an agent
    /// that does not run the crun runtime refuses them all.
    #[must_use]
    pub fn gpu_flags(&self) -> [(&'static str, bool); 3] {
        [
            ("--gpu-device", !self.gpu_devices.is_empty()),
            ("--gpu-mount", !self.gpu_mounts.is_empty()),
            ("--gpu-env", !self.gpu_env.is_empty()),
        ]
    }

    /// The flags only the crun runtime takes, with whether each was given: an agent that runs
    /// the container engine refuses them all (`--allow-host-jobs` and `--egress` above none
    /// are refused where the jobs are made, [`HOST_JOBS_NEED_CRUN`]).
    #[must_use]
    pub fn crun_flags(&self) -> [(&'static str, bool); 8] {
        let [device, mount, env] = self.gpu_flags();
        [
            ("--crun-bin", self.crun_bin.is_some()),
            ("--image-budget-gib", self.image_budget_gib.is_some()),
            ("--job-user", self.job_user.is_some()),
            device,
            mount,
            env,
            ("--host-port", !self.host_port.is_empty()),
            (
                "--image-allow-loopback",
                !self.image_allow_loopback.is_empty(),
            ),
        ]
    }

    /// The flags only the container engine takes, with whether each was given: an agent
    /// that runs crun refuses them all, and `--allow-container-engine` selects the engine.
    #[must_use]
    pub fn engine_flags(&self) -> [(&'static str, bool); 9] {
        [
            ("--container-bin", self.container_bin.is_some()),
            ("--container-user", self.container_user.is_some()),
            ("--container-subuid", self.container_subuid.is_some()),
            ("--container-host", self.container_host.is_some()),
            ("--container-pull", self.container_pull.is_some()),
            ("--container-tmp-size", self.container_tmp_size.is_some()),
            ("--container-add-host", !self.container_add_host.is_empty()),
            ("--actions-cgroup", self.actions_cgroup.is_some()),
            ("--gpu-toolkit-hook", self.gpu_toolkit_hook.is_some()),
        ]
    }

    /// Every flag of the set with whether it was given: an agent that runs no job
    /// (`--no-jobs`) refuses them all. A test walks the flags clap knows and fails when one is
    /// missing here.
    #[must_use]
    pub fn given(&self) -> Vec<(&'static str, bool)> {
        let checks = [
            ("--allow-container-engine", self.allow_container_engine),
            ("--allow-host-jobs", self.allow_host_jobs),
            ("--allow-fuse-jobs", self.allow_fuse_jobs),
            ("--cpu-ceiling", self.cpu_ceiling.is_some()),
            ("--cpu-series", self.cpu_series),
            ("--watch-dir", !self.watch_dirs.is_empty()),
            ("--settle-ms", self.settle_ms.is_some()),
            ("--taint-recheck-ms", self.taint_recheck_ms.is_some()),
            ("--host-second-samples", self.host_second_samples),
            ("--egress", self.egress.is_some()),
            ("--egress-allow", !self.egress_allow.is_empty()),
            ("--egress-dns", !self.egress_dns.is_empty()),
            (
                "--egress-forward-port",
                !self.egress_forward_port.is_empty(),
            ),
            ("--egress-pool", self.egress_pool.is_some()),
            ("--egress-ip", self.egress_ip.is_some()),
            ("--egress-nft", self.egress_nft.is_some()),
            ("--egress-nsenter", self.egress_nsenter.is_some()),
        ];
        let engine = self.engine_flags().into_iter();
        let crun = self.crun_flags().into_iter();
        checks.into_iter().chain(engine).chain(crun).collect()
    }

    /// The container engine's flags, as its settings are made from them.
    fn engine(&self) -> EngineFlags<'_> {
        EngineFlags {
            bin: self.container_bin.as_deref(),
            user: self.container_user.as_deref(),
            host: self.container_host.as_deref(),
            pull: self.container_pull.as_deref(),
            tmp_size: self.container_tmp_size.as_deref(),
            add_hosts: &self.container_add_host,
            actions_cgroup: self.actions_cgroup.as_deref(),
            subuid: self.container_subuid.as_deref(),
        }
    }

    /// The registries `--image-allow-loopback` names, whose credentials the agent reads.
    pub fn image_registries(&self) -> impl Iterator<Item = &String> {
        self.image_allow_loopback.iter()
    }

    /// `--image-budget-gib`, which the disk the agent offers is reduced by.
    #[must_use]
    pub fn image_budget_gib(&self) -> Option<u64> {
        self.image_budget_gib
    }
}

/// The first flag of `flags` that was given.
fn first(flags: impl IntoIterator<Item = (&'static str, bool)>) -> Option<&'static str> {
    flags
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
}

/// What the agent runs jobs with: containers, which `crun` makes (the settings hold a
/// [`crate::cli_runtime::CrunPlan`]) or the container engine runs.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Containers, through `crun` or the container engine.
    Container(Settings),
}

impl Backend {
    /// The settings of the containers.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        let Self::Container(settings) = self;
        settings
    }

    /// The flag a failure to start the jobs is told under.
    pub(crate) fn flag(&self) -> &'static str {
        match self.settings().crun() {
            Some(_) => "the crun runtime",
            None => "--container-bin",
        }
    }
}

/// The image client's allow-list of `cli` (`--image-allow`, then `--image-allow-loopback`),
/// each entry with its `credentials`: what the crun runtime pulls by. It is checked before the
/// agent connects, whatever runtime it runs.
///
/// # Errors
///
/// [`crate::ImageAllowError`]: the first entry that is wrong.
pub(crate) fn image_allow(
    cli: &Cli,
    credentials: &RegistryCredentials,
) -> Result<Vec<js_oci::Allow>, crate::ImageAllowError> {
    credentials.image_allow(&cli.image_allow, &cli.os.image_allow_loopback)
}

/// Nothing to mount: a Linux build's scratch root is mounted by its unit before the agent starts.
///
/// # Errors
///
/// None; the signature is the macOS build's.
pub(crate) fn mount_scratch(_cli: &Cli, _services: &Services<'_>) -> Result<(), Failure> {
    Ok(())
}

/// What a Linux build checks before the host is described: nothing (its checks need the
/// GPUs the host offers, [`check`]).
pub(crate) struct Opened;

/// See [`Opened`].
///
/// # Errors
///
/// None; the signature is the macOS build's.
pub(crate) fn open(
    _cli: &Cli,
    _services: &Services<'_>,
    _bundle: Option<&crate::bundle::Settings>,
) -> Result<Opened, Failure> {
    Ok(Opened)
}

/// What the start check found, which the rest of the start is made with.
pub(crate) struct Start {
    /// The settings of the crun runtime; none for the engine and for no jobs.
    crun: Option<Settings>,
    /// The checked `--gpu-toolkit-hook`.
    toolkit: Option<ToolkitHook>,
}

impl Start {
    /// The settings of the crun runtime, taken out: the jobs are made from them.
    pub(crate) fn take_crun(&mut self) -> Option<Settings> {
        self.crun.take()
    }
}

/// Checks this build's runtime and the flags that belong to it, and for `crun` the host: a
/// wrong flag or a missing piece of the host ends the agent with status 2 before it
/// registers, and a host that fails the crun check is never given the engine instead. The
/// runtime is `crun` unless the agent runs no job (`--no-jobs`, which takes no flag of
/// [`Flags`]) or runs the container engine (`--allow-container-engine` with `--container-bin`,
/// which takes no flag only `crun` takes). `gpus` are the indices of the GPUs the host
/// offers, whose nodes the GPU flags must name. `xcodes` are the Xcodes the hello lists, which
/// only a Mac has: nothing here runs with one.
pub(crate) fn check(
    cli: &Cli,
    services: &Services<'_>,
    image_allow: Vec<js_oci::Allow>,
    (gpus, _xcodes): (&[u32], &[crate::xcode::Xcode]),
    _opened: Opened,
    err: &mut dyn Write,
) -> Result<Start, Failure> {
    let os = &cli.os;
    let usage = |text: String| Failure::Usage(text);
    if cli.no_jobs {
        // An agent that runs no job takes no flag of a runtime that would run them.
        return match first(os.given().into_iter().chain(cli.providers.given())) {
            Some(flag) => Err(usage(format!(
                "{flag}: not with --no-jobs (an agent that runs no job)"
            ))),
            None => Ok(Start {
                crun: None,
                toolkit: None,
            }),
        };
    }
    if os.allow_container_engine {
        if os.container_bin.is_none() {
            return Err(usage(
                "--allow-container-engine: needs --container-bin".to_owned(),
            ));
        }
        // Only crun gives a job GPU nodes (the engine takes its own from the toolkit), host
        // ports, a network, a host job or an image store of its own.
        if let Some(flag) = first(os.crun_flags().into_iter().chain(cli.providers.given())) {
            return Err(usage(format!(
                "{flag}: not with --allow-container-engine (a flag of the crun runtime)"
            )));
        }
        let toolkit = ToolkitHook::from_path(os.gpu_toolkit_hook.as_deref());
        let toolkit = toolkit.map_err(Failure::Usage)?;
        let _ = writeln!(err, "js-agent: warning: {ENGINE_WARNING}");
        return Ok(Start {
            crun: None,
            toolkit,
        });
    }
    // The GPU flags need no host check, so a wrong one is named first.
    let passthrough = crun::GpuPassthrough::from_cli(os, gpus, crun::read_node);
    let passthrough = passthrough.map_err(|why| usage(why.to_string()))?;
    let budget = disk::image_budget_bytes(os.image_budget_gib).map_err(Failure::Usage)?;
    let host = services.machine.crun_host();
    let ready = crun_host::start(cli, &host, services.commands);
    let ready = ready.map_err(|why| usage(format!("{why}\n{NOT_READY}")))?;
    for warning in &ready.warnings {
        let _ = writeln!(err, "js-agent: warning: {warning}");
    }
    let settings = ready
        .settings(image_allow)
        .with_log_file_cap(cli.log_file_cap);
    let settings = settings.with_image_budget(budget);
    Ok(Start {
        crun: Some(settings.with_gpus(passthrough)),
        toolkit: None,
    })
}

/// The jobs of a Linux agent, made from the flags of [`Flags`] by [`jobs`].
pub(crate) struct Jobs {
    /// The containers, when the agent runs jobs.
    pub(crate) container: Option<Settings>,
    /// The run files root's layout: searchable for `crun`, whose job ids walk down to it.
    pub(crate) layout: crate::lifecycle::DirLayout,
    /// `--egress*`, checked.
    pub(crate) egress: crate::egress::Config,
    /// `--settle-ms`, or its default.
    pub(crate) settle: Duration,
    /// `--taint-recheck-ms`, or its default.
    pub(crate) taint_recheck: Duration,
    /// `--host-second-samples`.
    pub(crate) host_second_samples: bool,
    /// `--allow-host-jobs`.
    pub(crate) allow_host_jobs: bool,
    /// `--allow-fuse-jobs`.
    pub(crate) allow_fuse_jobs: bool,
    /// `--mount-provider` and `--mount-provider-wait`, checked.
    providers: MountProviders,
}

impl Jobs {
    /// The mount providers the flags name.
    pub(crate) fn providers(&self) -> MountProviders {
        self.providers.clone()
    }

    /// Nothing is left to check once the roots are known: the native runtime's checks are
    /// macOS's.
    ///
    /// # Errors
    ///
    /// None; the signature is the macOS build's.
    pub(crate) fn finish(
        &mut self,
        _cli: &Cli,
        _denied: &[&Path],
        _state: Option<&crate::lifecycle::StateRoot>,
        _euid: u32,
    ) -> Result<(), String> {
        Ok(())
    }

    /// The paths a run files root may not be, hold or lie in, beyond the agent's roots: none.
    pub(crate) fn denied_paths(&self) -> Vec<(&'static str, &Path)> {
        Vec::new()
    }

    /// The flag that turned the jobs on, for a refusal that needs it; none for no jobs.
    pub(crate) fn needs(&self) -> Option<&'static str> {
        self.container
            .as_ref()
            .map(|settings| match settings.crun() {
                Some(_) => "the crun runtime",
                None => "--container-bin",
            })
    }

    /// What the run files root is for.
    pub(crate) fn runs(&self) -> crate::lifecycle::run_files::Runs {
        match self.container {
            Some(_) => crate::lifecycle::run_files::Runs::Containers(self.layout),
            None => crate::lifecycle::run_files::Runs::None,
        }
    }

    /// The `--watch-dir` flags of `cli`, canonicalised, against the roots that change with
    /// every job (`busy`).
    pub(crate) fn watch(cli: &Cli, busy: &[&Path]) -> Result<Vec<PathBuf>, String> {
        crate::lifecycle::watch_dirs(&cli.os.watch_dirs, busy)
    }

    /// The backend the worker runs, placed beside the scratch root for `crun`.
    pub(crate) fn backend(
        self,
        scratch: Option<&crate::lifecycle::ScratchRoot>,
        files: Option<&crate::lifecycle::FilesRoot>,
    ) -> Option<Backend> {
        let crun = self.container.as_ref().is_some_and(|s| s.crun().is_some());
        let container = match (crun, scratch) {
            (true, Some(root)) => self
                .container
                .map(|settings| settings.placed_beside(root.path(), files.cloned())),
            _ => self.container,
        };
        container.map(Backend::Container)
    }
}

/// The job flags of a Linux agent checked: the engine's settings from its flags (or the
/// settings of `crun`), `--allow-host-jobs` and `--allow-fuse-jobs` (which `crun` alone takes,
/// [`HOST_JOBS_NEED_CRUN`], [`FUSE_JOBS_NEED_CRUN`]), the egress ceiling (above none on `crun`
/// only), `--settle-ms` and `--taint-recheck-ms`. `mounts` are the mount roots, which `crun`
/// binds beneath.
///
/// # Errors
///
/// The text names the flag at fault.
pub(crate) fn jobs(
    cli: &Cli,
    start: Option<Settings>,
    mounts: &crate::lifecycle::MountRoots,
) -> Result<Jobs, String> {
    use crate::lifecycle::{DirLayout, taint};
    let os = &cli.os;
    let layout = match start {
        Some(_) => DirLayout::Searchable,
        None => DirLayout::Private,
    };
    if os.allow_host_jobs && start.is_none() {
        return Err(HOST_JOBS_NEED_CRUN.to_owned());
    }
    if os.allow_fuse_jobs && start.is_none() {
        return Err(FUSE_JOBS_NEED_CRUN.to_owned());
    }
    let crun = start.map(|settings| settings.with_host_jobs(os.allow_host_jobs));
    let egress = egress_of(os, crun.is_some())?;
    let crun = crun.map(|settings| settings.with_mount_roots(mounts));
    let agent = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let from_flags = Settings::from_flags(&os.engine(), agent).map_err(|why| why.to_string())?;
    Ok(Jobs {
        container: crun.or(from_flags).map(|settings| {
            settings
                .with_cpu_ceiling(os.cpu_ceiling)
                .with_cpu_series(os.cpu_series)
        }),
        layout,
        egress,
        settle: crate::lifecycle::settle(os.settle_ms)?,
        taint_recheck: taint::recheck_every(os.taint_recheck_ms)?,
        host_second_samples: os.host_second_samples,
        allow_host_jobs: os.allow_host_jobs,
        allow_fuse_jobs: os.allow_fuse_jobs,
        providers: MountProviders::from_flags(
            &cli.providers.mount_providers,
            &cli.providers.mount_provider_users,
            cli.providers.mount_provider_wait,
            UserNames::Passwd(Path::new(crate::cli_runtime::PASSWD_FILE)),
        )?,
    })
}

/// Why `--allow-host-jobs` is refused at start on an agent whose runtime is not `crun`.
pub const HOST_JOBS_NEED_CRUN: &str = "--allow-host-jobs: only the crun runtime launches a \
     host job; the container engine (--allow-container-engine) runs contained jobs only";

/// Why `--allow-fuse-jobs` is refused at start on an agent whose runtime is not `crun`.
pub const FUSE_JOBS_NEED_CRUN: &str = "--allow-fuse-jobs: only the crun runtime gives a job the \
     FUSE device; the container engine (--allow-container-engine) does not";

/// The `--egress*` flags, checked, for an agent whose runtime is `crun` when `crun`. Only
/// `crun` enforces a ceiling, so a ceiling above none on the engine is refused rather than
/// run with no network at all.
fn egress_of(os: &Flags, crun: bool) -> Result<crate::egress::Config, String> {
    use crate::egress::{Config, Flags, Mode};
    let config = Config::from_flags(&Flags {
        mode: os.egress.as_deref(),
        allow: &os.egress_allow,
        dns: &os.egress_dns,
        forward_ports: &os.egress_forward_port,
        pool: os.egress_pool.as_deref(),
        ip: os.egress_ip.as_deref(),
        nft: os.egress_nft.as_deref(),
        nsenter: os.egress_nsenter.as_deref(),
    })
    .map_err(|why| why.to_string())?;
    if config.mode() != Mode::None && !crun {
        return Err(format!(
            "--egress {}: needs the crun runtime, the only runtime that enforces it; a job run \
             through the container engine has network none",
            config.mode()
        ));
    }
    Ok(config)
}

/// Nothing is joined to the jobs once they are made: the bundle store and the VM backend are
/// macOS's.
///
/// # Errors
///
/// None; the signature is the macOS build's.
pub(crate) fn complete(
    _local: &mut Local,
    _cli: &Cli,
    _start: &Start,
    _files: &dyn crate::FileReader,
) -> Result<(), String> {
    Ok(())
}

/// The grant a crun agent's session fills from the replies, which its live logs are uploaded
/// with; none for the engine and for no jobs.
pub(crate) fn log_grant(local: &Local) -> Option<Arc<crate::logs::HostGrant>> {
    let crun = local.backend.as_ref().and_then(|b| b.settings().crun());
    crun.map(|plan| Arc::clone(&plan.logs.grant))
}

/// The image budget of a crun agent, which the disk it offers is reduced by.
pub(crate) fn image_budget(local: &Local) -> Option<u64> {
    let crun = local.backend.as_ref().and_then(|b| b.settings().crun());
    crun.and_then(|plan| plan.image_budget)
}

/// The warning of a container agent given no `--watch-dir`: after a job only the processes,
/// mounts, kernel modules, `/etc/passwd` and `/etc/group` are checked.
pub(crate) fn warn(local: &Local, err: &mut dyn Write) {
    if local.backend.is_some() && local.watch.is_empty() {
        let _ = writeln!(
            err,
            "js-agent: warning: no --watch-dir given: after a job only the processes, mounts, \
             kernel modules, /etc/passwd and /etc/group are checked, so a cron entry, a unit, a \
             key or a sudoers line a job leaves behind is not seen; suggested: {}",
            crate::SUGGESTED_WATCH_DIRS.join(", ")
        );
    }
}

/// A run files root must be on a `tmpfs`: a run's files hold secrets that must not reach a
/// disk.
pub(crate) const RUN_FILES_ON_TMPFS: bool = true;

/// The runtime of `build` on the machine itself: `crun` for a crun plan, the container
/// engine's command line otherwise.
pub(crate) fn runtime(
    system: &System,
    build: &RuntimeBuild<'_>,
) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
    let settings = build.backend.settings();
    if let Some(plan) = settings.crun() {
        let opened = crun::open_for_plan(plan, build.host_id, system.crun_seams());
        let (runtime, mapping, warnings) = opened.map_err(|why| why.to_string())?;
        settings.resolve(mapping);
        return Ok((Box::new(runtime), warnings));
    }
    let runner = Box::new(system_runner(build.stop_grace));
    let opened = CliRuntime::open(
        settings.clone(),
        build.host_id,
        build.platform,
        unix_ms,
        runner,
    );
    let runtime = opened.map_err(|why| format!("{}: {why}", settings.bin()))?;
    let warnings = runtime.warnings().to_vec();
    Ok((Box::new(runtime), warnings))
}

/// The post-job checks of a backend's jobs, and the cgroup directory the usage of the leases
/// is read from (or why there is none).
pub(crate) type Checks = (Box<dyn CheckHook>, Result<String, &'static str>);

/// The post-job checks of `backend`'s jobs, which watch `watch` and wait `settle` for the host
/// to settle; and the cgroup directory the usage of the leases is read from (or why there is
/// none).
///
/// # Errors
///
/// The checks cannot read the host.
pub(crate) fn checks(
    backend: &Backend,
    machine: &dyn Machine,
    watch: Vec<PathBuf>,
    settle: Duration,
) -> Result<Checks, String> {
    let settings = backend.settings();
    let integrity = Integrity::start(machine.view(), settings.uid(), watch)?.with_settle(settle);
    // The engines give each slot a user of its own; every crun job runs as the one id of the
    // job user's range, so there are no slots to tell apart.
    let integrity = match settings.crun() {
        Some(_) => integrity,
        None => integrity.with_slots(settings.slot_ids(0).0),
    };
    let actions = settings.actions_cgroup().map(str::to_owned);
    Ok((Box::new(integrity), actions.ok_or(NO_ACTIONS_CGROUP)))
}

/// Why the heartbeat carries no load.
const NO_ACTIONS_CGROUP: &str =
    "usage: --actions-cgroup is not given: the heartbeat carries no load";
