//! A [`ContainerRuntime`] over the `docker` or `podman` command line.
//!
//! [`CliRuntime`] starts the runtime's program through a [`CommandRunner`] (so a test
//! feeds recorded answers, and the real runner bounds every call in time and output), by
//! the absolute path of `--container-bin`: no `PATH` lookup, no shell, every value one
//! argv entry.
//!
//! # What a contained job gets
//!
//! `create` with, in this order: the name, the labels, the limits ([`limits_to_args`]),
//! the hardening ([`hardening_args`]), then `--`, the image and the command line. Nothing
//! after `--` is read as an option. Then the mounts are read back (see below), then
//! `start`; when either fails the container is removed again, so a failed start leaves
//! nothing behind.
//!
//! ```text
//! <bin> create --name js-<run id> --label jobscheduler.dev/run-id=r1 ...
//!   --label jobscheduler.dev/agent=<host>
//!   --cpus 1.500 --memory 1073741824 --memory-swap 1073741824 --pids-limit 4096
//!   [--gpus "device=0,1" | --device nvidia.com/gpu=0 --device nvidia.com/gpu=1]
//!   --mount type=bind,src=<scratch>,dst=/scratch
//!   --mount type=bind,src=..,dst=..,readonly[,bind-recursive=readonly,bind-propagation=rprivate]
//!   --cap-drop ALL --security-opt no-new-privileges --network none --read-only
//!   --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m --ulimit core=0 --ipc private
//!   [--pid private] --pull never --user 65534:65534 -- <image> <args>...
//! <bin> inspect --type container --format "{{json .Mounts}}" -- <id>
//! <bin> start -- <id>
//! ```
//!
//! Every container carries `jobscheduler.dev/agent=<host id>` and the time it was
//! created, beside the lifecycle's own labels; [`ContainerRuntime::list_labelled`] asks
//! for the containers that carry the run label *and* this agent's label, so two agents
//! sharing one runtime never adopt each other's jobs.
//!
//! # The environment of the engine
//!
//! The program is started with an environment of its own, through
//! [`CommandRunner::run_in`]: the variables are exactly `PATH=/usr/sbin:/usr/bin:/sbin:/bin`
//! (where its helpers, such as `conmon`, `runc` and credential helpers, are found) and,
//! when `--container-host` is given, `DOCKER_HOST` and `CONTAINER_HOST` set to it. Nothing
//! the agent inherited (`LD_PRELOAD`, `DOCKER_CONFIG`, `DOCKER_CONTEXT`, `XDG_RUNTIME_DIR`,
//! the agent's own `PATH`) reaches the engine, and the host is a flag, not a variable.
//! Without `--container-host` the engine uses its own default (the system socket). A
//! rootless Podman that finds its runtime directory through `XDG_RUNTIME_DIR` falls back
//! to the one for the user's id; where that is not enough, `--container-host` names the
//! socket.
//!
//! # Names, and a create that may have worked
//!
//! A container is named `js-<run id>`. A run id is up to 128 bytes of `[A-Za-z0-9._-]`;
//! the name has to fit in 128 bytes, so a run id of more than 125 bytes is refused as a
//! start that cannot be named. The name exists so that a `create` whose outcome is not
//! known can be cleaned up: when the call fails (the program was killed at its time limit
//! while the daemon finished the pull and made the container) or exits 0 and prints
//! something that is not an id, the agent removes the container by that name and the
//! error is the create's, with a note when the removal failed too. A call that fails
//! cleanly costs one more `rm` and, when that fails, a `ps` that finds nothing. A name that
//! is taken (a container left by an earlier run of the same id) makes the create fail, and
//! the cleanup then removes that container.
//!
//! A container that was created and never started is never taken for a running job: when
//! the agent lists its containers after a restart, one in the state `created` (Podman:
//! `configured`) is removed and left out of the list.
//!
//! # The mounts, and the window that stays
//!
//! The daemon binds a mount by path. It cannot use a file descriptor the agent opened,
//! so the agent cannot hand the daemon the directory it resolved, only the canonical
//! path. A job that holds a read-write mount in a root can replace a directory of that
//! root by a link between the agent's resolution and the daemon's bind. The agent closes
//! what it can: after `create` it reads the mounts back from the engine (`inspect`,
//! `.Mounts`) and requires that every bind has the source the agent resolved (the
//! scratch path is canonical, see `ScratchRoot`), the destination and the access it asked
//! for, and that there is no other bind; the tmpfs and volumes are not looked at. A
//! container whose mounts differ is removed, `start` is not called, and the start fails
//! (`failed_to_start`, detail `runtime-failed`; the finding the lifecycle keeps says the
//! mounts differ). **The window that remains** is the time between the agent's resolution
//! and the daemon's bind, and the read-back sees only where the daemon says it bound: a
//! link swapped back after the bind, or a path the daemon reports as given, is not
//! detected. Mount roots shared by jobs of different trust are still shared at their own
//! risk (see [`MountRoots`](crate::lifecycle::MountRoots)).
//!
//! # The scratch directory and the user
//!
//! A run's scratch directory is `0700` and the agent's. The container runs as a user that
//! is not root (`--container-user`, `65534:65534` by default), so the directory has to
//! be made writable for it: an agent that runs as root gives the directory to that user
//! before `create` ([`hand_over`]); any other agent cannot give a file away and refuses to
//! start unless the container user is its own `uid:gid` ([`Settings::with_agent`]).
//!
//! # Docker and Podman
//!
//! The runtime is told apart once, by `--version`, whatever the file is called. The two
//! take the arguments above alike, except where noted: GPUs, the PID namespace, and the
//! recursive read-only bind. Docker has `--gpus` (with the NVIDIA container toolkit);
//! Podman is given the CDI device names (`nvidia.com/gpu=<index>`), which need a CDI
//! specification the operator has generated. Docker takes `--pid` only as `host` or
//! another container (its default is private, with no daemon setting that overrides it),
//! so only Podman is given `--pid private`; both are given `--ipc private`.
//! `bind-recursive=readonly` is passed for Docker 25 and later (it makes a mount below a
//! read-only bind read-only, and the engine refuses the container on a kernel before 5.12
//! instead of leaving it writable); older Docker and Podman get plain `readonly`, which
//! leaves a mount below the bind writable on the engines that do not make it recursive.
//! Podman's SELinux relabelling of bind mounts (`:Z`) is not applied: a host with SELinux
//! enforcing needs the mount roots labelled by the operator. Swap limits: Docker is asked
//! (`info --format {{.SwapLimit}}`) when the runtime opens, and a host without swap
//! accounting is reported in [`CliRuntime::warnings`] (`--memory-swap` is ignored there);
//! Podman reports no such flag, so nothing is said for it.
//!
//! # Calls that find the container gone
//!
//! The runner gives the agent no error text of the runtime (a failed call is its exit
//! status), so "no such container" is not read from a message. When `inspect`, `stop` or
//! `rm` fails the agent asks `ps -a` for the id: absent means gone (`poll` answers
//! [`Status::Gone`], `stop` and `remove` succeed), present means the failure is real. An
//! id is the 64 hexadecimal digits `ps --no-trunc` prints, and nothing shorter is
//! accepted: lines are compared whole.
//!
//! A container the engine calls `dead` (it failed to remove it) is a failure: it is
//! reported exited with its code when the code is not 0 or the memory controller killed
//! it, and as gone (which ends the job as `failed`) when the code is 0. It is never
//! reported as having succeeded.
//!
//! # Time
//!
//! A restarted agent counts the time a job already ran against its wall limit from the
//! `jobscheduler.dev/created-ms` label, which is the **wall clock** ([`unix_ms`]) when
//! the agent decided to create the container (before a pull, so the pull counts). The
//! engines print their own times in formats that differ, so the label is the agent's. A
//! step of the system clock moves the age: a clock that was set back never makes an age
//! below zero (it saturates at 0, the job gets its time), and one set forward shortens
//! the time adopted jobs have left; the age does not wrap. The lifecycle's own limit
//! while the agent runs is on its monotonic clock.
//!
//! A daemon that stops answering does not hang the agent: the runner ends every call after
//! [`CALL_LIMIT`] plus the stop grace ([`system_runner`]) and the call fails. The calls are
//! made on the lifecycle's worker thread (see the lifecycle documentation), which is
//! required for exactly this: a start can make up to five calls (a `create`, its `rm` and
//! `ps`; or a read-back and a removal), so the worker can be held for several times the
//! bound, which delays the wall limit of every job but never a heartbeat.
//!
//! # macOS
//!
//! A container on a Mac runs in a Linux virtual machine that cannot reach the Apple GPU.
//! A job that names a GPU there is refused as `failed_to_start` with the detail
//! `gpu-unsupported` and nothing is created ([`ContainerRuntime::supports_gpus`]); the Mac
//! path for GPU work is a lease of the whole host.
//!
//! # Not here
//!
//! Registry credentials (the agent holds none: an image is on the host, or pullable
//! without one when `--container-pull missing` is given), and rootless setup.

mod args;
mod settings;

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::Isolation;
use serde_json::Value;

pub use args::{
    ArgsError, Bind, DEFAULT_PIDS, SCRATCH_TARGET, binds_of, hardening_args, label_args,
    limits_to_args,
};
pub use settings::{
    DEFAULT_NETWORK, DEFAULT_TMP_SIZE, DEFAULT_USER, Flags, Pull, Settings, SettingsError,
};

use crate::facts::{CommandRunner, Platform, SystemCommands};
use crate::lifecycle::{
    ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, RuntimeError, Status, label,
};

/// How long one call of the runtime may take, besides the stop grace it is given: long
/// enough for an image to be pulled by `create`.
pub const CALL_LIMIT: Duration = Duration::from_secs(600);

/// The most output of one call that is accepted, in bytes.
pub const CALL_OUTPUT_CAP: usize = 256 * 1024;

/// The ids of containers are lower-case hexadecimal of exactly 64 digits, as
/// `ps --no-trunc` prints them.
const ID_LEN: usize = 64;

/// The `PATH` the engine is started with.
const ENGINE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// What a container's name starts with; the run id follows.
const NAME_PREFIX: &str = "js-";

/// The longest container name made.
const NAME_MAX: usize = 128;

/// The first Docker version that takes `bind-recursive`.
const RECURSIVE_RO_DOCKER: u32 = 25;

/// What the operator is told about an engine without swap accounting.
const NO_SWAP_LIMIT: &str = "the engine reports no swap limit support: --memory-swap is ignored, so a job can use swap beyond its memory limit";

/// Which command line the runtime speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// `docker`.
    Docker,
    /// `podman`.
    Podman,
}

impl Engine {
    /// The engine whose `--version` printed `text`, when it is one the agent knows.
    pub fn of_version(text: &str) -> Option<Self> {
        let text = text.to_ascii_lowercase();
        match (text.contains("podman"), text.contains("docker")) {
            (true, _) => Some(Self::Podman),
            (false, true) => Some(Self::Docker),
            (false, false) => None,
        }
    }
}

/// A call that failed. The text is the runtime's program and its exit, never what it
/// printed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    /// The program could not be run or exited with a status other than 0 (a missing
    /// binary is this too).
    #[error("{call}: {why}")]
    Call {
        /// The subcommand.
        call: &'static str,
        /// The runner's reason.
        why: String,
    },
    /// The program succeeded and printed something that is not what the call returns.
    #[error("{call}: unreadable output")]
    Unreadable {
        /// The subcommand.
        call: &'static str,
    },
    /// `--version` names neither Docker nor Podman.
    #[error("--container-bin is neither docker nor podman")]
    NotAnEngine,
    /// A host job reached the container runtime.
    #[error("only a contained job runs in a container")]
    NotContained,
    /// A job named a GPU on a host whose containers cannot reach one.
    #[error("containers on this host cannot use a GPU")]
    GpuUnsupported,
    /// An id that is not a container id the runtime would print.
    #[error("not a container id")]
    BadId,
    /// A value that cannot be passed safely.
    #[error("{0}")]
    Args(#[from] ArgsError),
    /// A run id that cannot be made a container name.
    #[error("the run id cannot be made a container name")]
    Name,
    /// The scratch directory could not be given to the container's user.
    #[error("scratch: {0}")]
    Scratch(String),
    /// The mounts the engine made are not the ones the agent resolved.
    #[error("the mounts of the container are not the ones the agent resolved")]
    MountsDiffer,
    /// A step of starting a container failed after the engine may have made it, and the
    /// container was removed again (`note` says so when it could not be).
    #[error("{why}{note}")]
    Abandoned {
        /// The failed step.
        why: String,
        /// Empty, or `; the container could not be removed: ..` .
        note: String,
    },
}

impl From<CliError> for RuntimeError {
    fn from(err: CliError) -> Self {
        Self(err.to_string())
    }
}

/// Milliseconds since the Unix epoch on the system clock: what [`CliRuntime`] stamps a
/// container with, and measures its age against after a restart.
pub fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// The runner the program gives a [`CliRuntime`]: every call may take [`CALL_LIMIT`]
/// and the `stop_grace` of the lifecycle, and print [`CALL_OUTPUT_CAP`] bytes.
pub fn system_runner(stop_grace: Duration) -> SystemCommands {
    SystemCommands::bounded(CALL_LIMIT.saturating_add(stop_grace), CALL_OUTPUT_CAP)
}

/// Gives a directory to a user and group.
pub type Chown = fn(&Path, u32, u32) -> io::Result<()>;

/// Gives the directory `path` itself (a link is not followed) to `uid` and `gid`.
///
/// # Errors
///
/// The system's reason: the caller may not give the file away, or it is not there.
pub fn hand_over(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

/// The container runtime of the lifecycle, over the `docker` or `podman` command line.
pub struct CliRuntime {
    settings: Settings,
    engine: Engine,
    recursive_ro: bool,
    host_id: String,
    gpus: bool,
    now_ms: fn() -> u64,
    runner: Box<dyn CommandRunner>,
    chown: Chown,
    warnings: Vec<String>,
}

impl CliRuntime {
    /// Finds out which engine `settings.bin()` is (by `--version`) and returns the
    /// runtime. `host_id` is the id the agent registered under; `now_ms` is the wall
    /// clock ([`unix_ms`]).
    ///
    /// # Errors
    ///
    /// The program cannot be run, or it is neither Docker nor Podman.
    pub fn open(
        settings: Settings,
        host_id: &str,
        platform: &Platform,
        now_ms: fn() -> u64,
        runner: Box<dyn CommandRunner>,
    ) -> Result<Self, CliError> {
        let printed = runner
            .run_in(settings.bin(), &["--version"], &engine_env(&settings))
            .map_err(|why| CliError::Call {
                call: "--version",
                why,
            })?;
        let engine = Engine::of_version(&printed).ok_or(CliError::NotAnEngine)?;
        let docker_major = major_of(&printed).filter(|_| engine == Engine::Docker);
        let mut runtime = Self {
            settings,
            engine,
            recursive_ro: docker_major.is_some_and(|major| major >= RECURSIVE_RO_DOCKER),
            host_id: host_id.to_owned(),
            gpus: platform.os != "macos",
            now_ms,
            runner,
            chown: hand_over,
            warnings: Vec::new(),
        };
        runtime.warnings = runtime.swap_warning().into_iter().collect();
        Ok(runtime)
    }

    /// The same runtime giving scratch directories away with `chown` instead of
    /// [`hand_over`]: a test's seam.
    #[must_use]
    pub fn with_chown(self, chown: Chown) -> Self {
        Self { chown, ..self }
    }

    /// What the operator should know about the engine, found when the runtime was opened.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The engine found.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    fn call(&self, call: &'static str, args: &[&str]) -> Result<String, CliError> {
        self.runner
            .run_in(self.settings.bin(), args, &engine_env(&self.settings))
            .map_err(|why| CliError::Call { call, why })
    }

    /// The warning for a Docker engine that cannot limit swap, when it says so.
    fn swap_warning(&self) -> Option<String> {
        let asked = (self.engine == Engine::Docker)
            .then(|| self.call("info", &["info", "--format", "{{.SwapLimit}}"]));
        asked
            .and_then(Result::ok)
            .filter(|printed| printed.trim() == "false")
            .map(|_| NO_SWAP_LIMIT.to_owned())
    }

    /// The argv of `create` for `spec`, named `name` and made `created_ms`.
    fn create_args(
        &self,
        spec: &ContainerSpec,
        name: &str,
        created_ms: u64,
    ) -> Result<Vec<String>, CliError> {
        let mut labels: BTreeMap<String, String> = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), created_ms.to_string());
        let mut args = vec!["create".to_owned(), "--name".to_owned(), name.to_owned()];
        args.extend(label_args(&labels)?);
        args.extend(limits_to_args(self.engine, spec, self.recursive_ro)?);
        args.extend(hardening_args(&self.settings, self.engine));
        args.extend(["--".to_owned(), spec.image.clone()]);
        args.extend(spec.args.iter().cloned());
        Ok(args)
    }

    fn launch(&self, spec: &ContainerSpec) -> Result<ContainerId, CliError> {
        if spec.isolation != Isolation::Contained {
            return Err(CliError::NotContained);
        }
        if !spec.gpu_indices.is_empty() && !self.gpus {
            return Err(CliError::GpuUnsupported);
        }
        let name = container_name(&spec.run_id)?;
        let args = self.create_args(spec, &name, (self.now_ms)())?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.hand_over_scratch(spec)?;
        let created = self
            .call("create", &args)
            .and_then(|printed| created_id(&printed));
        let id = match created {
            Ok(id) => id,
            Err(why) => return Err(abandoned(&why, self.delete_named(&name, &spec.run_id))),
        };
        self.verify_mounts(&id, spec)
            .and_then(|()| self.call("start", &["start", "--", &id.0]))
            .map(|_| id.clone())
            .map_err(|why| abandoned(&why, self.delete(&id)))
    }

    /// Gives the run's scratch directory to the container's user, when the agent is root.
    fn hand_over_scratch(&self, spec: &ContainerSpec) -> Result<(), CliError> {
        let give = |(uid, gid): (u32, u32)| {
            (self.chown)(&spec.scratch, uid, gid).map_err(|why| CliError::Scratch(why.to_string()))
        };
        self.settings.chown_scratch().map_or(Ok(()), give)
    }

    /// Reads the binds of the container back and compares them with what `spec` asked
    /// for (see the module documentation).
    fn verify_mounts(&self, id: &ContainerId, spec: &ContainerSpec) -> Result<(), CliError> {
        let unreadable = || CliError::Unreadable { call: "inspect" };
        let text = self
            .inspect(id, "{{json .Mounts}}")?
            .ok_or_else(unreadable)?;
        let mut seen = binds_read(&text).ok_or_else(unreadable)?;
        let mut asked = binds_of(spec);
        seen.sort();
        asked.sort();
        (seen == asked).then_some(()).ok_or(CliError::MountsDiffer)
    }

    /// Whether the runtime still has the container.
    fn exists(&self, id: &ContainerId) -> Result<bool, CliError> {
        let filter = format!("id={}", id.0);
        self.call("ps", &["ps", "-a", "-q", "--no-trunc", "--filter", &filter])
            .map(|ids| ids.lines().any(|line| line.trim() == id.0))
    }

    /// Whether the runtime has any container of this agent for the run.
    fn has_run(&self, run_id: &str) -> Result<bool, CliError> {
        let (run, agent) = (
            format!("label={}={run_id}", label::RUN),
            format!("label={}={}", label::AGENT, self.host_id),
        );
        let args = [
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            &run,
            "--filter",
            &agent,
        ];
        self.call("ps", &args)
            .map(|ids| ids.lines().any(|line| !line.trim().is_empty()))
    }

    /// `Ok(())` for a failed call whose container is gone anyway, else the failure.
    fn gone_or(&self, id: &ContainerId, failed: CliError) -> Result<(), CliError> {
        match self.exists(id)? {
            true => Err(failed),
            false => Ok(()),
        }
    }

    /// The `inspect` of one container with `format`, `None` when it is gone.
    fn inspect(&self, id: &ContainerId, format: &str) -> Result<Option<String>, CliError> {
        check(id)?;
        let args = [
            "inspect",
            "--type",
            "container",
            "--format",
            format,
            "--",
            &id.0,
        ];
        match self.call("inspect", &args) {
            Ok(text) => Ok(Some(text)),
            Err(failed) => self.gone_or(id, failed).map(|()| None),
        }
    }

    fn status(&self, id: &ContainerId) -> Result<Status, CliError> {
        let format = "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}";
        self.inspect(id, format)?.map_or(Ok(Status::Gone), |text| {
            parse_status(&text).ok_or(CliError::Unreadable { call: "inspect" })
        })
    }

    fn halt(&self, id: &ContainerId, grace: Duration) -> Result<(), CliError> {
        check(id)?;
        let secs = grace.as_secs() + u64::from(grace.subsec_nanos() > 0);
        let secs = secs.to_string();
        self.call("stop", &["stop", "--time", &secs, "--", &id.0])
            .map(|_| ())
            .or_else(|failed| self.gone_or(id, failed))
    }

    fn delete(&self, id: &ContainerId) -> Result<(), CliError> {
        check(id)?;
        self.call("rm", &["rm", "--force", "--volumes", "--", &id.0])
            .map(|_| ())
            .or_else(|failed| self.gone_or(id, failed))
    }

    /// Removes the container called `name`, made for `run_id`, when there is one: a failed
    /// `rm` is followed by a look at the run's containers, and none means there was nothing
    /// to remove.
    fn delete_named(&self, name: &str, run_id: &str) -> Result<(), CliError> {
        self.call("rm", &["rm", "--force", "--volumes", "--", name])
            .map(|_| ())
            .or_else(|failed| match self.has_run(run_id)? {
                true => Err(failed),
                false => Ok(()),
            })
    }

    fn found(&self, id: &str) -> Result<Option<Found>, CliError> {
        let id = ContainerId(id.to_owned());
        let unreadable = || CliError::Unreadable { call: "inspect" };
        let format = "{{.State.Status}} {{json .Config.Labels}}";
        let Some(text) = self.inspect(&id, format)? else {
            return Ok(None);
        };
        let (state, labels) = text.trim().split_once(' ').ok_or_else(unreadable)?;
        // Made and never started: no job ever ran in it, whatever its labels say.
        if matches!(state, "created" | "configured") {
            return self.delete(&id).map(|()| None);
        }
        let labels: Option<BTreeMap<String, String>> =
            serde_json::from_str(labels).map_err(|_| unreadable())?;
        let labels = labels.unwrap_or_default();
        // A container with no creation time is as old as it can be: its job is over time.
        let running_ms = labels
            .get(label::CREATED_MS)
            .and_then(|made| made.parse::<u64>().ok())
            .map_or(u64::MAX, |made| (self.now_ms)().saturating_sub(made));
        Ok(Some(Found {
            id,
            labels,
            running_ms,
        }))
    }

    fn listing(&self) -> Result<Vec<Found>, CliError> {
        let (run, agent) = (
            format!("label={}", label::RUN),
            format!("label={}={}", label::AGENT, self.host_id),
        );
        let args = [
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            &run,
            "--filter",
            &agent,
        ];
        let printed = self.call("ps", &args)?;
        let found = printed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|id| self.found(id))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(found.into_iter().flatten().collect())
    }
}

/// The environment the engine is started with: the pinned `PATH`, and the engine host
/// under both names the two engines read when one is given.
fn engine_env(settings: &Settings) -> Vec<(&str, &str)> {
    let hosts = settings
        .host()
        .into_iter()
        .flat_map(|host| [("DOCKER_HOST", host), ("CONTAINER_HOST", host)]);
    std::iter::once(("PATH", ENGINE_PATH))
        .chain(hosts)
        .collect()
}

/// The major version in a `--version` text such as `Docker version 27.3.1, build ..`.
fn major_of(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once("version ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `js-<run id>`, when that is a name the engines take.
fn container_name(run_id: &str) -> Result<String, CliError> {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
    let fits = !run_id.is_empty()
        && NAME_PREFIX.len() + run_id.len() <= NAME_MAX
        && run_id.bytes().all(word);
    fits.then(|| format!("{NAME_PREFIX}{run_id}"))
        .ok_or(CliError::Name)
}

/// The id `create` printed: its last line.
fn created_id(printed: &str) -> Result<ContainerId, CliError> {
    printed
        .lines()
        .next_back()
        .map(str::trim)
        .filter(|line| is_id(line))
        .map(|line| ContainerId(line.to_owned()))
        .ok_or(CliError::Unreadable { call: "create" })
}

/// The failure `why` of a step of starting, with a note when the container it may have
/// made could not be removed (`cleaned` is the removal's result).
fn abandoned(why: &CliError, cleaned: Result<(), CliError>) -> CliError {
    let note = cleaned.err().map_or_else(String::new, |gone| {
        format!("; the container could not be removed: {gone}")
    });
    CliError::Abandoned {
        why: why.to_string(),
        note,
    }
}

/// The bind mounts in the JSON `.Mounts` of an `inspect`; other kinds are left out. `None`
/// when the text is not that.
fn binds_read(text: &str) -> Option<Vec<Bind>> {
    let entries = match serde_json::from_str(text.trim()).ok()? {
        Value::Null => Vec::new(),
        Value::Array(entries) => entries,
        _ => return None,
    };
    let each = entries.iter().map(bind_of);
    each.collect::<Option<Vec<_>>>()
        .map(|binds| binds.into_iter().flatten().collect())
}

/// One entry of `.Mounts`: `Some(None)` for a kind other than a bind, `None` for an entry
/// that does not have the fields of its kind.
fn bind_of(entry: &Value) -> Option<Option<Bind>> {
    if entry.get("Type")?.as_str()? != "bind" {
        return Some(None);
    }
    let source = entry.get("Source")?.as_str()?;
    let destination = entry.get("Destination")?.as_str()?;
    let writable = entry.get("RW")?.as_bool()?;
    Some(Some((source.to_owned(), destination.to_owned(), writable)))
}

/// Whether `text` is a container id as `ps --no-trunc` prints it.
fn is_id(text: &str) -> bool {
    text.len() == ID_LEN && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `id` itself, when it is safe to put in an argument or a filter.
fn check(id: &ContainerId) -> Result<(), CliError> {
    Some(()).filter(|()| is_id(&id.0)).ok_or(CliError::BadId)
}

/// The state, exit code and OOM flag `inspect` printed.
fn parse_status(text: &str) -> Option<Status> {
    let [state, code, oom] = text.split_whitespace().collect::<Vec<_>>()[..] else {
        return None;
    };
    let exit = || {
        code.parse().ok().map(|code| Exit {
            code,
            oom_killed: oom == "true",
        })
    };
    match state {
        "running" | "paused" | "restarting" | "removing" | "created" | "configured" => {
            Some(Status::Running)
        }
        "exited" | "stopped" => exit().map(Status::Exited),
        // A container the engine could not remove after it stopped. Its code may be 0
        // however the job ended, so it is a success only if nothing says otherwise, and
        // nothing can: it is gone, which ends the job as failed.
        "dead" => exit().map(|exit| match (exit.code, exit.oom_killed) {
            (0, false) => Status::Gone,
            _ => Status::Exited(exit),
        }),
        _ => None,
    }
}

impl ContainerRuntime for CliRuntime {
    fn create_and_start(&mut self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        self.launch(spec).map_err(RuntimeError::from)
    }

    fn poll(&mut self, id: &ContainerId) -> Result<Status, RuntimeError> {
        self.status(id).map_err(RuntimeError::from)
    }

    fn stop(&mut self, id: &ContainerId, grace: Duration) -> Result<(), RuntimeError> {
        self.halt(id, grace).map_err(RuntimeError::from)
    }

    fn remove(&mut self, id: &ContainerId) -> Result<(), RuntimeError> {
        self.delete(id).map_err(RuntimeError::from)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.listing().map_err(RuntimeError::from)
    }

    fn supports_gpus(&self) -> bool {
        self.gpus
    }
}
