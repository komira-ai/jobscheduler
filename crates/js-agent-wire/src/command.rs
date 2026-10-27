//! Work sent down to the agent: [`Command`] and the [`JobSpec`] a start carries.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::hello::WireShape;
use crate::limits;
use crate::refusal;

/// Whether a job is confined to a container or may change the machine (design 5.12).
///
/// An absent value is [`Isolation::Contained`]. A value this version does not know fails
/// to decode, so an agent never runs a job it cannot classify; inside a reply that start
/// reads as [`Command::Unknown`] and the other commands still arrive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A container with the stated limits and mounts.
    #[default]
    Contained,
    /// The job may change the machine; the host is tainted afterwards.
    Host,
}

/// What the agent must enforce on a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Thousandths of a CPU core; above zero.
    pub cpu_millis: u64,
    /// Bytes of memory; above zero.
    pub memory_bytes: u64,
    /// The most processes the job may hold, when bounded; above zero when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    /// The wall-time limit in milliseconds; above zero and at most
    /// [`limits::MAX_WALL_MS`].
    pub wall_ms: u64,
}

impl Limits {
    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::range(
            at.field("limits.cpu_millis"),
            self.cpu_millis,
            1,
            limits::MAX_CPU_MILLIS,
        )?;
        check::range(
            at.field("limits.memory_bytes"),
            self.memory_bytes,
            1,
            limits::MAX_MEMORY_BYTES,
        )?;
        if let Some(pids) = self.pids {
            check::range(at.field("limits.pids"), pids, 1, limits::MAX_PIDS)?;
        }
        if self.wall_ms == 0 {
            return Err(ValidationError::new(
                "limits.wall_ms",
                Rule::OutOfRange,
                refusal::WALL_TIME_NOT_POSITIVE,
            ));
        }
        check::range(
            at.field("limits.wall_ms"),
            self.wall_ms,
            1,
            limits::MAX_WALL_MS,
        )
    }
}

/// A host directory made visible to a job: a path under a root the agent's own
/// configuration names, never a path from the message alone. The agent looks the root
/// name up in its own table (it is never a path) and canonicalises the result against the
/// root before it mounts anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// The name of one of the agent's configured roots: `[A-Za-z0-9._-]`, leading
    /// alphanumeric, at most [`limits::MAX_ROOT_NAME_LEN`] bytes.
    pub root_name: String,
    /// The path below that root: relative, with at least one named component (`.` and `./`
    /// alone name the root, which is not mountable), no `..` component, no control
    /// character, no `:` and no `,` (so it cannot add a field to a mount option string),
    /// at most [`limits::MAX_REL_LEN`] bytes.
    pub rel: String,
    /// Whether the job sees the directory read-only.
    pub ro: bool,
}

impl Mount {
    /// Checks the mount without touching the file system.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("mounts", refusal::INVALID_MOUNT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        let root = at.field("mounts.root_name");
        check::token(
            root,
            &self.root_name,
            limits::MAX_ROOT_NAME_LEN,
            check::ident_byte,
        )?;
        let rel = at.field("mounts.rel");
        if self.rel.is_empty() {
            return Err(rel.fail(Rule::Empty));
        }
        if self.rel.len() > limits::MAX_REL_LEN {
            return Err(rel.fail(Rule::TooLong));
        }
        let unsafe_char = self
            .rel
            .chars()
            .any(|c| c.is_control() || c == ':' || c == ',');
        if unsafe_char {
            return Err(rel.fail(Rule::BadChars));
        }
        let climbs = Path::new(&self.rel).components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
        if climbs {
            return Err(rel.fail(Rule::Escapes));
        }
        let names_something = Path::new(&self.rel)
            .components()
            .any(|part| matches!(part, Component::Normal(_)));
        if !names_something {
            return Err(rel.fail(Rule::Empty));
        }
        Ok(())
    }
}

/// The body of a start: one job.
///
/// Fields the design gives a start that are not here yet: the fence, the booking, the
/// deadline, named assignments beyond `gpu_indices`, and environment by reference. The
/// run-delivery change adds them, as optional fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The run's id; the key of every idempotent step.
    pub run_id: String,
    /// The lease the run belongs to.
    pub lease_id: String,
    /// The container image; the agent holds no registry credential, so it must be
    /// present on the host or pullable without one. An OCI reference: it starts with a
    /// letter or digit, so it can never be read as a command-line option.
    pub image: String,
    /// The command line after the image: at most [`limits::MAX_ARGS`] entries, no NUL.
    #[serde(default)]
    pub args: Vec<String>,
    /// What the agent enforces.
    pub limits: Limits,
    /// The GPU device indices the job may use. A device is in at most one job.
    #[serde(default)]
    pub gpu_indices: Vec<u32>,
    /// The directories the job sees.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Container or host. Absent means contained.
    #[serde(default)]
    pub isolation: Isolation,
    /// Labels for the container; none may use the reserved prefix.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The heartbeat the server had taken when it queued the start.
    #[serde(default)]
    pub heartbeat_seq: u64,
    /// How long after the agent received the start it may still be acted on, measured
    /// on the agent's clock alone; 0 means no bound.
    #[serde(default)]
    pub valid_for_ms: u64,
}

impl JobSpec {
    /// Checks the start contract and every bound: ids, image, arguments, limits, GPU
    /// indices, mounts and labels. The server calls it before it queues a start and the
    /// agent before it acts on one.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("job", refusal::INVALID_JOB);
        check::ident(at.field("run_id"), &self.run_id)?;
        check::ident(at.field("lease_id"), &self.lease_id)?;
        let image = At::new("image", refusal::INVALID_IMAGE);
        check::token(image, &self.image, limits::MAX_IMAGE_LEN, check::image_byte)?;
        self.check_args(at.field("args"))?;
        self.limits.check(at)?;
        let gpus = at.field("gpu_indices");
        check::count(gpus, self.gpu_indices.len(), limits::MAX_GPUS as usize)?;
        check::distinct(gpus, self.gpu_indices.iter())?;
        for index in &self.gpu_indices {
            check::range(gpus, u64::from(*index), 0, limits::MAX_GPUS - 1)?;
        }
        let mounts = At::new("mounts", refusal::INVALID_MOUNT);
        check::count(mounts, self.mounts.len(), limits::MAX_MOUNTS)?;
        for mount in &self.mounts {
            mount.check(mounts)?;
        }
        check::labels(at.field("labels"), &self.labels)?;
        check::range(
            at.field("valid_for_ms"),
            self.valid_for_ms,
            0,
            limits::MAX_VALID_FOR_MS,
        )
    }

    fn check_args(&self, at: At) -> Result<(), ValidationError> {
        check::count(at, self.args.len(), limits::MAX_ARGS)?;
        let mut total = 0usize;
        for arg in &self.args {
            if arg.len() > limits::MAX_ARG_LEN {
                return Err(at.fail(Rule::TooLong));
            }
            if arg.contains('\0') {
                return Err(at.fail(Rule::BadChars));
            }
            total += arg.len();
        }
        if total > limits::MAX_ARGS_TOTAL_BYTES {
            return Err(at.fail(Rule::TooLong));
        }
        Ok(())
    }

    /// Whether the job's limits and the number of its GPU indices fit one host's offered
    /// shape. [`JobSpec::validate`] bounds each index; matching the indices to the devices
    /// the host reported is the caller's, who holds that list. This is the check for a single job; the sum over several jobs on one host is the server's, and
    /// it adds with `checked_add` (every value here is below the caps that keep a sum of
    /// 2^18 jobs from overflowing).
    pub fn fits_shape(&self, shape: &WireShape) -> Result<(), ValidationError> {
        let at = At::new("limits", refusal::LIMITS_EXCEED_SHAPE);
        let gpus = self.gpu_indices.len() as u64;
        let over = [
            self.limits.cpu_millis > shape.cpu_millis,
            self.limits.memory_bytes > shape.memory_bytes,
            gpus > shape.gpus,
        ];
        if over.into_iter().any(|bad| bad) {
            return Err(at.fail(Rule::Inconsistent));
        }
        Ok(())
    }
}

/// An order from the server to the agent, carried in a heartbeat reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start one job. Idempotent on the run id.
    Start(Box<JobSpec>),
    /// Stop one run: a retained intent, resent until the run leaves the held list.
    Stop {
        /// The run to stop.
        run_id: String,
    },
    /// Stop one run the server no longer holds a lease for, and do not report it.
    Cancel {
        /// The run to cancel.
        run_id: String,
    },
    /// A command this version cannot decode. The agent ignores it, so the commands
    /// beside it still take effect.
    #[serde(other)]
    Unknown,
}

impl Command {
    /// Checks the command: a start by [`JobSpec::validate`], a stop or cancel by its run
    /// id. An unknown command passes; the agent skips it.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Start(spec) => spec.validate(),
            Self::Stop { run_id } | Self::Cancel { run_id } => {
                check::ident(At::new("run_id", refusal::INVALID_REPLY), run_id)
            }
            Self::Unknown => Ok(()),
        }
    }
}
