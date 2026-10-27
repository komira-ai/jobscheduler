//! Work sent down to the agent: [`Command`] and the [`JobSpec`] a start carries.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::refusal;

/// Whether a job is confined to a container or may change the machine (design 5.12).
///
/// An absent value is [`Isolation::Contained`]. A value this version does not know fails
/// to decode, so an agent refuses a job it cannot classify and never runs it.
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
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// The most processes the job may hold, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    /// The wall-time limit in milliseconds; it must be above zero.
    pub wall_ms: u64,
}

/// A host directory made visible to a job: a path under a root the agent's own
/// configuration names, never a path from the message alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// The name of one of the agent's configured roots.
    pub root_name: String,
    /// The path below that root; relative, with no `..`.
    pub rel: String,
    /// Whether the job sees the directory read-only.
    pub ro: bool,
}

/// Why a [`Mount`] is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MountError {
    /// The root name is empty or holds a `/`.
    #[error("root_name {0:?} must be a non-empty root name without '/'")]
    BadRoot(String),
    /// The path is absolute, climbs with `..`, or holds a NUL byte.
    #[error("rel {0:?} must be relative, without '..' and without NUL")]
    Escapes(String),
}

impl Mount {
    /// Checks the mount without touching the file system; the agent also canonicalises
    /// the path against its root before it mounts anything.
    pub fn validate(&self) -> Result<(), MountError> {
        let bad_root = [self.root_name.is_empty(), self.root_name.contains('/')]
            .into_iter()
            .any(|bad| bad);
        if bad_root {
            return Err(MountError::BadRoot(self.root_name.clone()));
        }
        let climbs = Path::new(&self.rel).components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
        if [climbs, self.rel.contains('\0')].into_iter().any(|bad| bad) {
            return Err(MountError::Escapes(self.rel.clone()));
        }
        Ok(())
    }
}

/// The body of a start: one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The run's id; the key of every idempotent step.
    pub run_id: String,
    /// The lease the run belongs to.
    pub lease_id: String,
    /// The container image; the agent holds no registry credential, so it must be
    /// present on the host or pullable without one.
    pub image: String,
    /// The command line after the image.
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
    /// Labels for the container.
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

/// Why a [`JobSpec`] is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobSpecError {
    /// An id the start must name is empty.
    #[error("{0} must not be empty")]
    EmptyId(&'static str),
    /// The image name is empty.
    #[error("image must not be empty")]
    EmptyImage,
    /// The wall-time limit is zero (the start contract of design 5.2).
    #[error("limits.wall_ms must be above zero")]
    WallTimeNotPositive,
    /// A mount is refused; `index` is its place in `mounts`.
    #[error("mounts[{index}]: {source}")]
    Mount {
        /// The mount's place in the list.
        index: usize,
        /// Why it was refused.
        source: MountError,
    },
}

impl JobSpecError {
    /// The refusal code that goes on the wire for this error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyId(_) => refusal::INVALID_JOB,
            Self::EmptyImage => refusal::INVALID_IMAGE,
            Self::WallTimeNotPositive => refusal::WALL_TIME_NOT_POSITIVE,
            Self::Mount { .. } => refusal::INVALID_MOUNT,
        }
    }
}

impl JobSpec {
    /// Checks the start contract: ids and image present, a positive wall-time limit,
    /// every mount relative to a named root.
    pub fn validate(&self) -> Result<(), JobSpecError> {
        if self.run_id.is_empty() {
            return Err(JobSpecError::EmptyId("run_id"));
        }
        if self.lease_id.is_empty() {
            return Err(JobSpecError::EmptyId("lease_id"));
        }
        if self.image.is_empty() {
            return Err(JobSpecError::EmptyImage);
        }
        if self.limits.wall_ms == 0 {
            return Err(JobSpecError::WallTimeNotPositive);
        }
        for (index, mount) in self.mounts.iter().enumerate() {
            mount
                .validate()
                .map_err(|source| JobSpecError::Mount { index, source })?;
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
}
