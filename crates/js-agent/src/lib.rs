//! `js-agent`: runs on a machine, tells a lease server what the machine offers, and
//! keeps telling it that the machine is alive, so the machine can be leased through the
//! server's ordinary lease API.
//!
//! The library is the program: [`run_with`] takes the arguments, the streams to print to
//! and the [`Services`] (the commands and files it reads the host from, the transport to
//! the server, the sleeper and the jitter), and `main` calls [`main`], which supplies the
//! machine's own. No clock is read anywhere: time passes only in the sleeper.
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
//! | `--allow-host-jobs` | not available yet: refused at start, because a host job taints the host and the record of that does not exist; without it a start with `isolation = host` creates nothing and is refused |
//! | `--mount-root <name>=<path>` | a directory jobs may mount from, by name; repeatable; canonicalised when the agent starts; no root, no mount |
//! | `--scratch-root <path>` | where each run's private (`0700`) scratch directory goes; owned by the agent's user, no permission for group or others |
//! | `--container-bin <path>` | the absolute path of the `docker` or `podman` program that runs container jobs; never looked up on the `PATH`; started with an environment of its own (a fixed `PATH` and `--container-host`, nothing inherited); without it no container runs; a job that names a GPU needs the NVIDIA container toolkit with Docker and, with Podman, a CDI specification (`nvidia.com/gpu=<index>`) the operator has generated; rootless engines are not supported (the engine gets no `XDG_RUNTIME_DIR`, `HOME` or D-Bus session, and a rootless Podman may then run containers without their limits) |
//! | `--container-network <name>` | the user-defined network containers join; by default none at all; `host` is refused; any named network gives a job egress to whatever it reaches, the host's metadata endpoint included, and the agent warns at start; needs `--container-bin` |
//! | `--container-user <uid:gid>` | the user containers run as, both above zero; by default `65534:65534`; an agent that runs as root gives each run's scratch directory to this user, any other agent refuses to start unless this is its own `uid:gid`; needs `--container-bin` |
//! | `--container-host <uri>` | the engine to talk to (`unix:///<path>`, `tcp://<address>`, `ssh://<address>`), passed as `DOCKER_HOST` and `CONTAINER_HOST` to the engine's calls only; by default the engine's own default; an `ssh://` host is accepted but not supported (the engine gets no `SSH_AUTH_SOCK` and no `HOME`); needs `--container-bin` |
//! | `--container-pull <never\|missing>` | when a job's image is pulled; by default `never`, so a job cannot make the host reach a registry of its choosing: the operator puts an image on the host first (`docker pull`, or `docker load` from an archive), and a job whose image is not there ends as `failed_to_start`; needs `--container-bin` |
//! | `--container-tmp-size <size>` | the size of each container's `/tmp` (`noexec,nosuid,nodev`), such as `64m` (the default) or `1g`; needs `--container-bin` |
//!
//! The job flags are checked at start and a wrong one ends the agent with exit status 2.
//! What they configure is the [`lifecycle`], which no change has connected to the session
//! yet: today the flags are validated and nothing runs a job.
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
//! SIGTERM and SIGINT make the agent send one last heartbeat with the phase `draining`
//! and exit 0, so the server leases the host no more.
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
//! 0 after a stop request, 1 when the program cannot install its signal handlers, 2 for
//! a flag or file that is wrong and for a server that refuses the host for good.

mod backoff;
mod cli;
pub mod cli_runtime;
mod container;
mod facts;
mod host;
pub mod lifecycle;
mod run;
mod session;
mod signals;
mod transport;

use std::ffi::OsString;
use std::fmt::Display;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

pub use backoff::{BASE, Backoff, CAP, Jitter, RETRY_AFTER_CAP, SystemJitter};
pub use cli::Cli;
use cli_runtime::Settings;
pub use facts::{
    APPLE_GPU_CLASS, CommandRunner, Detected, FileReader, Gpus, PROBE_LIMIT, PROBE_OUTPUT_CAP,
    Platform, SystemCommands, SystemFiles, detect, host_id, machine_id,
};
pub use host::{Offer, offer};
use lifecycle::Local;
pub use run::{Exit, Sleeper, Wake, drive};
pub use session::{Fatal, HEARTBEAT_PATH, HELLO_PATH, Note, Reason, Session, Step};
pub use signals::SystemSleeper;
pub use transport::{Answer, Connector, Dialer, REQUEST_LIMIT, Transport};

/// The exit statuses of `js-agent`.
pub mod exit {
    /// The agent was asked to stop and announced it.
    pub const OK: u8 = 0;
    /// The program could not install its signal handlers.
    pub const FAILED: u8 = 1;
    /// A flag or a file it names is wrong, or the server refused the host for good.
    pub const USAGE: u8 = 2;
}

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
}

/// Why the program ends without a stop request.
enum Failure {
    /// The command line, as clap reports it (this includes `--help` and `--version`).
    Clap(clap::Error),
    /// A flag or file is wrong.
    Usage(String),
    /// The server refused the host for good.
    Fatal(Fatal),
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
    // The job flags are checked now so a wrong one stops the agent before it registers.
    // The change that runs jobs builds the lifecycle from them.
    let local = Local::from_cli(&cli).map_err(Failure::Usage)?;
    let container = local.container.iter();
    container.flat_map(Settings::warnings).for_each(|warning| {
        let _ = writeln!(err, "js-agent: warning: {warning}");
    });
    let mut session = Session::new(offer.hello, &mut *transport, &mut *services.jitter);
    match drive(&mut session, &mut *services.sleeper, err) {
        Exit::Stopped => Ok(()),
        Exit::Fatal(fatal) => Err(Failure::Fatal(fatal)),
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
    }
}

/// Prints `message` as the program's last line and returns its exit status.
fn fail(err: &mut dyn Write, message: &dyn Display, status: u8) -> ExitCode {
    let _ = writeln!(err, "js-agent: {message}");
    ExitCode::from(status)
}
