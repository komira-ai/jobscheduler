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
//! # Flags
//!
//! Configuration is flags only: `js-agent` reads no environment variable.
//!
//! | flag | meaning |
//! |---|---|
//! | `--server <url>` | required: `https://host:port`; `http://` only for `localhost`, `127.0.0.0/8` and `::1` |
//! | `--token-file <path>` | required: the pool's join token, in a regular file owned by the user running the agent, mode 0600 or 0400 |
//! | `--ca-cert <path>` | required for `https://`: the PEM file with the certificate to trust; the platform's roots are never read |
//! | `--pool <name>` | required: the pool to join |
//! | `--host-id <id>` | the id to register under; by default `h-` and 32 hex digits of a salted SHA-256 of the machine id, never the machine id |
//! | `--label k=v` | a label on the host; repeatable |
//! | `--capability k=v` | a capability to report; repeatable; `os` and `arch` are detected and cannot be set |
//! | `--cpu-millis`, `--memory-bytes`, `--gpus` | offer less than the host has; never more |
//! | `--gpu-class <class>` | the class the offered GPUs are reported under, instead of the detected one |
//! | `--allow-host-jobs` | not available yet: refused at start, because the scheduler does not record a host job's taint yet; without it a start with `isolation = host` creates nothing and is refused |
//! | `--state-root <path>` | the directory the taint marker is kept in; owned by the agent's user, no permission for group or others; required with `--container-bin`. A host that a post-job check found unclean is tainted for good: the marker survives a restart and no code of the agent removes it. It may not be, or lie in, a mount root or the scratch root, and it must be on a disk of its own (a job that fills a shared disk makes the marker unwritable). The agent refuses a state root whose `st_dev` equals that of any mount root or the scratch root; that is a best-effort guard, not the guarantee: subvolumes that share free space have distinct device numbers, and the engine's data root and container layers are not compared. The taint belongs to this directory, not to the machine: a second agent, another state root or a state root that is lost at a reboot (`tmpfs`) does not know of it |
//! | `--watch-dir <path>` | a directory the post-job checks list (its own entries: names, sizes, modification and status change times, inodes, owners, modes; a write to an existing file inside a subdirectory is not seen) and compare with the listing taken when the host was last known clean; one level deep: a name added to a subdirectory is seen, a write to a file in it is not, so watch the subdirectory itself; repeatable; needs `--container-bin`; may not hold, or lie in, a mount root, the scratch root or the state root; more than 4096 entries cannot be checked and taint the host. With none, only `/etc/passwd`, `/etc/group`, the processes, the mounts and the kernel modules are checked, and the agent warns at start; suggested: `/etc`, `/etc/cron.d`, `/etc/sudoers.d`, `/etc/systemd/system`, `/etc/ld.so.conf.d`, `/root/.ssh`, `/usr/local/bin`, `/usr/local/sbin`. What a job leaves in a read-write `--mount-root` is never examined |
//! | `--mount-root <name>=<path>` | a directory jobs may mount from, by name; repeatable; canonicalised when the agent starts; no root, no mount |
//! | `--scratch-root <path>` | where each run's private (`0700`) scratch directory goes; owned by the agent's user, no permission for group or others; required with `--container-bin` |
//! | `--container-bin <path>` | the absolute path of the `docker` or `podman` program that runs container jobs; never looked up on the `PATH`; started with an environment of its own (a fixed `PATH` and `--container-host`, nothing inherited); without it no container runs; a job that names a GPU needs the NVIDIA container toolkit with Docker and, with Podman, a CDI specification (`nvidia.com/gpu=<index>`) the operator has generated; rootless engines are not supported (the engine gets no `XDG_RUNTIME_DIR`, `HOME` or D-Bus session, and a rootless Podman may then run containers without their limits); `userns-remap` is not supported either (the container user shows as another uid on the host); the agent must share the pid and mount namespaces of `init` and be able to read `/proc/1/ns` (root, or `CAP_SYS_PTRACE`), or it does not start |
//! | `--container-network <name>` | the user-defined network containers join; by default none at all; `host` is refused; any named network gives a job egress to whatever it reaches, the host's metadata endpoint included, and the agent warns at start; needs `--container-bin` |
//! | `--container-user <uid:gid>` | the user containers run as, both above zero; by default `65534:65534`; an agent that runs as root gives each run's scratch directory to this user, any other agent refuses to start unless this is its own `uid:gid`; needs `--container-bin` |
//! | `--container-host <uri>` | the engine to talk to: a socket on this machine, `unix:///<path>`, passed as `DOCKER_HOST` and `CONTAINER_HOST` to the engine's calls only; by default the engine's own default; `tcp://` and `ssh://` are refused (the post-job checks read this machine, and a job on another one would pass them all); needs `--container-bin` |
//! | `--container-pull <never\|missing>` | when a job's image is pulled; by default `never`, so a job cannot make the host reach a registry of its choosing: the operator puts an image on the host first (`docker pull`, or `docker load` from an archive), and a job whose image is not there ends as `failed_to_start`; needs `--container-bin` |
//! | `--container-tmp-size <size>` | the size of each container's `/tmp` (`noexec,nosuid,nodev`), such as `64m` (the default) or `1g`; needs `--container-bin` |
//! | `--actions-cgroup <path>` | the cgroup version 2 directory below `/sys/fs/cgroup` that holds every container (made and delegated by the operator): each container is created with `--cgroup-parent` naming it (the slice name for one `.slice` directly below `/sys/fs/cgroup`, the systemd driver; the path with a leading `/` otherwise, the cgroupfs driver), and the heartbeat carries the usage of the directory and of each lease's cgroup (see [`usage`]); without it the heartbeat carries no load, and the agent says so at start; needs `--container-bin` |
//! | `--stop-grace <seconds>` | how long a container gets to stop on its own before it is killed, whenever the agent stops one; by default 10, at most 3600; needs `--container-bin` |
//! | `--drain-wait <seconds>` | how long SIGTERM lets the running jobs finish before the agent stops them; by default the stop grace, at most 3600; needs `--container-bin` |
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
//! server's code on stderr: 401 (the token), 403 `unknown-pool`, `shape-not-accepted`,
//! `host-bound` and `host-listed` (things only the operator can change), and the 4xx that
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
//! after `prepare_timeout`), then `restoring` when the reply stops naming the lease; see
//! the `exclusive` module for the sequence and [`lifecycle::Lifecycle::prepare`] for what
//! quiet means. The agent started by [`run_with`] gives the
//! session no [`Quiesce`]: the job [`Worker`] does not implement it, so with `--container-bin`
//! too the session does not follow an `exclusive` lease. Its heartbeats name no lease phase,
//! `exclusive_ready` or `exclusive_refused`, and the worker's lifecycle refuses every start
//! (`no-capacity`, reported as `failed_to_start`) while a reply names a lease, since nothing
//! prepares the host for it; the runs it holds go on. A later change implements [`Quiesce`]
//! on the worker. `prepare_timeout` is [`PREPARE_TIMEOUT`]
//! until a flag sets it. The VM check of the design is a later change; the readiness hook
//! (`--readiness-hook`, see `readiness`) wraps the host in [`Hooked`].
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
mod cli;
pub mod cli_runtime;
mod container;
mod exclusive;
mod facts;
mod host;
pub mod integrity;
mod jobs;
pub mod lifecycle;
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
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

pub use backoff::{BASE, Backoff, CAP, Jitter, RETRY_AFTER_CAP, SystemJitter};
pub use cli::Cli;
use cli_runtime::Settings;
pub use exclusive::{PREPARE_TIMEOUT, Quiesce};
pub use facts::{
    APPLE_GPU_CLASS, CommandRunner, Detected, FileReader, Gpus, PROBE_LIMIT, PROBE_OUTPUT_CAP,
    Platform, Ran, SystemCommands, SystemFiles, detect, host_id, machine_id,
};
pub use host::{Offer, offer};
pub use jobs::DRAIN_SETTLE;
use lifecycle::{Local, StateRoot};
pub use readiness::{
    EXIT_NOISE_OVER_BOUND, EXIT_NOT_QUIET, EXIT_SETUP_FAILED, EXIT_UNSUPPORTED, HookRunner, Hooked,
    ReadinessHook, Verdict, hook_runner,
};
pub use run::{Exit, Sleeper, Wake, drive};
pub use session::{Drain, Fatal, HEARTBEAT_PATH, HELLO_PATH, Note, Reason, Session, Step};
pub use signals::SystemSleeper;
pub use system::{Machine, RuntimeBuild, SharedClock, System};
pub use transport::{Answer, Connector, Dialer, REQUEST_LIMIT, Transport};
pub use worker::{
    Delivery, Handle, LINES_CAP, Plan, QUEUE, STALL_MARGIN, STEP_EVERY, Snapshot, Worker,
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
    /// Told of the job worker once it has started (a test's way to wait for it).
    pub worker_hook: Option<&'a dyn Fn(Handle)>,
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

/// The program, with the machine's own services.
pub fn main() -> ExitCode {
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
                worker_hook: None,
            };
            run_with(args, out, err, services)
        }
    }
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
    let mut transport = services
        .connector
        .connect(&cli.server, &cli.token_file, ca_cert)
        .map_err(|why| Failure::Usage(why.to_string()))?;
    let version = env!("CARGO_PKG_VERSION");
    let (platform, commands, files) = (&services.platform, services.commands, services.files);
    let offer = offer(&cli, platform, commands, files, version).map_err(Failure::Usage)?;
    offer.warnings.iter().for_each(|warning| {
        let _ = writeln!(err, "js-agent: warning: {warning}");
    });
    let checked = offer.hello.validate();
    checked.map_err(|why| Failure::Usage(format!("the hello is not acceptable: {why}")))?;
    // The hook is checked now for the same reason; the change that wires the lifecycle
    // to the session wraps its host in `Hooked`.
    ReadinessHook::from_cli(&cli).map_err(Failure::Usage)?;
    // The job flags are checked now so a wrong one stops the agent before it registers;
    // the worker is built from them below, before the first hello.
    let local = Local::from_cli(&cli).map_err(Failure::Usage)?;
    local
        .check_filesystems(services.files)
        .map_err(Failure::Usage)?;
    // The post-job checks read /proc, which a Mac does not have: a container job there
    // would run unchecked, so none is accepted (see `integrity`).
    if platform.is_macos() && local.container.is_some() {
        return Err(Failure::Usage(
            "--container-bin: container jobs are Linux-only: the post-job checks read /proc, \
             which a Mac does not have"
                .to_owned(),
        ));
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
    let taint = local.state.iter().filter_map(StateRoot::read);
    taint.for_each(|taint| {
        let _ = writeln!(
            err,
            "js-agent: warning: this host is tainted ({}): the agent starts no job on it",
            taint.reason
        );
    });
    let container = local.container.iter();
    container.flat_map(Settings::warnings).for_each(|warning| {
        let _ = writeln!(err, "js-agent: warning: {warning}");
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
    let (machine, hook) = (&services.machine, services.worker_hook);
    let started = jobs::start(local, &offer.hello, platform, machine, hook, err);
    let jobs = started.map_err(Failure::Usage)?;
    let mut session = Session::new(offer.hello, &mut *transport, &mut *services.jitter);
    if let Some((worker, drain)) = &jobs {
        session = session.with_jobs(worker, *drain);
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
