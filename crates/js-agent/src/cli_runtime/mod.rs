//! A [`ContainerRuntime`] over the `docker` or `podman` command line.
//!
//! [`CliRuntime`] starts the runtime's program through a [`CommandRunner`] (so a test
//! feeds recorded answers, and the real runner bounds every call in time and output), by
//! the absolute path of `--container-bin`: no `PATH` lookup, no shell, every value one
//! argv entry.
//!
//! # What a contained job gets
//!
//! `create` with, in this order: the labels, the limits ([`limits_to_args`]), the
//! hardening ([`hardening_args`]), then `--`, the image and the command line. Nothing
//! after `--` is read as an option. Then `start`, and when `start` fails the container
//! is removed again, so a failed start leaves nothing behind.
//!
//! ```text
//! <bin> create --label jobscheduler.dev/run-id=r1 ... --label jobscheduler.dev/agent=<host>
//!   --cpus 1.500 --memory 1073741824 --memory-swap 1073741824 --pids-limit 4096
//!   [--gpus "device=0,1" | --device nvidia.com/gpu=0 --device nvidia.com/gpu=1]
//!   --mount type=bind,src=<scratch>,dst=/scratch --mount type=bind,src=..,dst=..,readonly
//!   --cap-drop ALL --security-opt no-new-privileges --network none --read-only
//!   --tmpfs /tmp --user 65534:65534 -- <image> <args>...
//! ```
//!
//! Every container carries `jobscheduler.dev/agent=<host id>` and the time it was
//! created, beside the lifecycle's own labels; [`ContainerRuntime::list_labelled`] asks
//! for the containers that carry the run label *and* this agent's label, so two agents
//! sharing one runtime never adopt each other's jobs.
//!
//! # Docker and Podman
//!
//! The runtime is told apart once, by `--version`, whatever the file is called. The two
//! take the arguments above alike; they differ in one place the agent uses: GPUs. Docker
//! has `--gpus` (with the NVIDIA container toolkit); Podman is given the CDI device
//! names (`nvidia.com/gpu=<index>`), which need a CDI specification the operator has
//! generated. Podman's SELinux relabelling of bind mounts (`:Z`) is not applied: a host
//! with SELinux enforcing needs the mount roots labelled by the operator.
//!
//! # Calls that find the container gone
//!
//! The runner gives the agent no error text of the runtime (a failed call is its exit
//! status), so "no such container" is not read from a message. When `inspect`, `stop` or
//! `rm` fails the agent asks `ps -a` for the id: absent means gone (`poll` answers
//! [`Status::Gone`], `stop` and `remove` succeed), present means the failure is real.
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
//! Image pull policy and registry credentials (the agent holds none: an image is on the
//! host or pullable without one), and rootless setup.

mod args;
mod settings;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::Isolation;

pub use args::{
    ArgsError, DEFAULT_PIDS, SCRATCH_TARGET, hardening_args, label_args, limits_to_args,
};
pub use settings::{DEFAULT_NETWORK, DEFAULT_USER, Settings, SettingsError};

use crate::facts::{CommandRunner, Platform, SystemCommands};
use crate::lifecycle::{
    ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, RuntimeError, Status, label,
};

/// How long one call of the runtime may take, besides the stop grace it is given: long
/// enough for an image to be pulled by `create`.
pub const CALL_LIMIT: Duration = Duration::from_secs(600);

/// The most output of one call that is accepted, in bytes.
pub const CALL_OUTPUT_CAP: usize = 256 * 1024;

/// The ids of containers are lower-case hexadecimal of at least 12 and at most 64 digits.
const ID_DIGITS: std::ops::RangeInclusive<usize> = 12..=64;

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
    /// `start` failed after `create` had made the container, which was then removed (and
    /// `note` says so when it could not be).
    #[error("{why}{note}")]
    Start {
        /// The runner's reason.
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

/// The container runtime of the lifecycle, over the `docker` or `podman` command line.
pub struct CliRuntime {
    settings: Settings,
    engine: Engine,
    host_id: String,
    gpus: bool,
    now_ms: fn() -> u64,
    runner: Box<dyn CommandRunner>,
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
            .run(settings.bin(), &["--version"])
            .map_err(|why| CliError::Call {
                call: "--version",
                why,
            })?;
        let engine = Engine::of_version(&printed).ok_or(CliError::NotAnEngine)?;
        Ok(Self {
            settings,
            engine,
            host_id: host_id.to_owned(),
            gpus: platform.os != "macos",
            now_ms,
            runner,
        })
    }

    /// The engine found.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    fn call(&self, call: &'static str, args: &[&str]) -> Result<String, CliError> {
        self.runner
            .run(self.settings.bin(), args)
            .map_err(|why| CliError::Call { call, why })
    }

    /// The argv of `create` for `spec`, made `created_ms`.
    fn create_args(&self, spec: &ContainerSpec, created_ms: u64) -> Result<Vec<String>, CliError> {
        let mut labels: BTreeMap<String, String> = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), created_ms.to_string());
        let mut args = vec!["create".to_owned()];
        args.extend(label_args(&labels)?);
        args.extend(limits_to_args(self.engine, spec)?);
        args.extend(hardening_args(&self.settings));
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
        let args = self.create_args(spec, (self.now_ms)())?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let printed = self.call("create", &args)?;
        let id = printed
            .lines()
            .next_back()
            .map(str::trim)
            .filter(|line| is_id(line))
            .ok_or(CliError::Unreadable { call: "create" })?;
        let id = ContainerId(id.to_owned());
        self.call("start", &["start", "--", &id.0])
            .map(|_| id.clone())
            .map_err(|started| {
                let note = self.delete(&id).err().map_or_else(String::new, |why| {
                    format!("; the container could not be removed: {why}")
                });
                CliError::Start {
                    why: started.to_string(),
                    note,
                }
            })
    }

    /// Whether the runtime still has the container.
    fn exists(&self, id: &ContainerId) -> Result<bool, CliError> {
        let filter = format!("id={}", id.0);
        self.call("ps", &["ps", "-a", "-q", "--no-trunc", "--filter", &filter])
            .map(|ids| ids.lines().any(|line| line.trim() == id.0))
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

    fn found(&self, id: &str) -> Result<Option<Found>, CliError> {
        let id = ContainerId(id.to_owned());
        let unreadable = CliError::Unreadable { call: "inspect" };
        let Some(text) = self.inspect(&id, "{{json .Config.Labels}}")? else {
            return Ok(None);
        };
        let labels: Option<BTreeMap<String, String>> =
            serde_json::from_str(text.trim()).map_err(|_| unreadable)?;
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

/// Whether `text` is a container id as the runtime prints it.
fn is_id(text: &str) -> bool {
    ID_DIGITS.contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
    match state {
        "running" | "paused" | "restarting" | "removing" | "created" | "configured" => {
            Some(Status::Running)
        }
        "exited" | "dead" | "stopped" => Some(Status::Exited(Exit {
            code: code.parse().ok()?,
            oom_killed: oom == "true",
        })),
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
