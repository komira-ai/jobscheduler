//! The messages keyed by job id (design 3.2, 5.3 and 5.7): [`JobStartSpec`] and the stop that
//! go down in a reply, the [`HeldJob`]s and their [`JobResult`]s that go up in a heartbeat,
//! and the [`JobResultAck`] that answers one.
//!
//! They ride in protocol major 1 beside the lease and run messages and are announced by the
//! hello capability [`PER_JOB_CAPABILITY`] (see [`crate::protocol`]): a server sends a
//! `job_start` or `job_stop` only to an agent that reported it. A job start names no lease
//! and carries no time left in one: the wall limit is the job's own, counted by the agent
//! from the moment its container is made, so a start that waited in delivery is neither
//! shortened nor stretched. The pair of job id and allocation id names one placement of a
//! job; the allocation id is new at every placement.
//!
//! A job the agent started from a `job_start` is listed in `jobs` alone and its result in
//! `jobs[].result` alone, never in the `held` runs or the `results` of the same heartbeat, so
//! a lease server that knows no job ids never sees one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::files::{self, RunFile};
use crate::heartbeat::{Ack, HeldState, LogTail, Outcome};
use crate::image;
use crate::limits;
use crate::native;
use crate::refusal;

/// The hello capability of an agent that takes [`crate::Command::JobStart`] and
/// [`crate::Command::JobStop`] and reports `jobs` and `retired_jobs` in its heartbeat. Its
/// value is `true`. An agent without it reads both commands as [`crate::Command::Unknown`],
/// so a server sends them only to a host that reports it.
pub const PER_JOB_CAPABILITY: &str = "per-job";

/// What the job's allocation booked on the host, which the agent enforces as the job's
/// limits. The ranges are those of [`crate::Limits`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookedResources {
    /// Thousandths of a CPU core; above zero and at most [`limits::MAX_CPU_MILLIS`].
    pub cpu_millis: u64,
    /// Bytes of memory; above zero and at most [`limits::MAX_MEMORY_BYTES`].
    pub memory_bytes: u64,
    /// Bytes of scratch disk, when the allocation books them; above zero and at most
    /// [`limits::MAX_DISK_BYTES`] when present. Absent means the agent's own default, as
    /// for a run without `disk_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// The GPU device indices the job may use: at most [`limits::MAX_GPUS`], each below it,
    /// none twice. A device is in at most one job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_indices: Vec<u32>,
}

impl BookedResources {
    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::range(
            at.field("booked.cpu_millis"),
            self.cpu_millis,
            1,
            limits::MAX_CPU_MILLIS,
        )?;
        check::range(
            at.field("booked.memory_bytes"),
            self.memory_bytes,
            1,
            limits::MAX_MEMORY_BYTES,
        )?;
        if let Some(disk) = self.disk_bytes {
            check::range(
                at.field("booked.disk_bytes"),
                disk,
                1,
                limits::MAX_DISK_BYTES,
            )?;
        }
        let gpus = at.field("booked.gpu_indices");
        check::count(gpus, self.gpu_indices.len(), limits::MAX_GPUS as usize)?;
        check::distinct(gpus, self.gpu_indices.iter())?;
        for index in &self.gpu_indices {
            check::range(gpus, u64::from(*index), 0, limits::MAX_GPUS - 1)?;
        }
        Ok(())
    }
}

/// The body of a job start: one placement of one job.
///
/// Fields of the job a later change adds, as optional fields: the network policy and the
/// declared ports (the job network change), and `os` and `arch` (the one job definition,
/// which also makes the image the definition on macOS; a native body is not carried here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStartSpec {
    /// The job's id; the key of every idempotent step. The grammar of every id here
    /// (`[A-Za-z0-9._-]`, leading alphanumeric, at most [`limits::MAX_ID_LEN`] bytes).
    pub job_id: String,
    /// The id of the allocation this start is for, new at every placement of the job.
    pub allocation_id: String,
    /// The container image, pinned by a `sha256:` digest ([`JobStartSpec::require_pinned`]),
    /// in the grammar of [`crate::JobSpec::image`]. A start that names none (absent or
    /// empty) is refused `invalid-job`.
    #[serde(default)]
    pub image: String,
    /// The program and its arguments after the image, as [`crate::JobSpec::args`]: at most
    /// [`limits::MAX_ARGS`] entries, no NUL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The environment, literal and not secret, held to the rules of a native job's
    /// ([`crate::NativeSpec::env`]): at most [`limits::MAX_ENV`] entries, names
    /// `[A-Za-z_][A-Za-z0-9_]*` and not reserved, values without NUL. The check is the
    /// native body's, shared until the one job definition (`js-one-job-definition`) owns
    /// it. An agent of this version answers a start whose environment is not empty
    /// `failed_to_start` with [`refusal::ENV_UNSUPPORTED`] and runs nothing, until its
    /// runtimes can set it (`js-agent-job-env-runtimes`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Files the agent writes for the job before it starts, as [`crate::JobSpec::files`],
    /// with the same limits. Contents are secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RunFile>,
    /// What the allocation booked, enforced as the job's limits.
    pub booked: BookedResources,
    /// The job's wall limit in milliseconds: above zero and at most
    /// [`limits::MAX_WALL_MS`]. The agent counts it on its monotonic clock from the moment
    /// the container is made.
    pub wall_ms: u64,
}

impl JobStartSpec {
    /// Checks the start: the ids, that an image is named and well formed, the command, the
    /// environment, the files, the booked resources and the wall limit. The server calls it
    /// before it sends a start and the agent before it acts on one, together with
    /// [`JobStartSpec::require_pinned`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("job", refusal::INVALID_JOB);
        check::ident(at.field("job_id"), &self.job_id)?;
        check::ident(at.field("allocation_id"), &self.allocation_id)?;
        if self.image.is_empty() {
            return Err(at.field("image").fail(Rule::Empty));
        }
        image::image(At::new("image", refusal::INVALID_IMAGE), &self.image)?;
        check::args(at.field("command"), &self.command)?;
        native::check_env(At::new("env", refusal::INVALID_ENV), &self.env)?;
        files::validate_files(&self.files)?;
        self.booked.check(at)?;
        if self.wall_ms == 0 {
            return Err(ValidationError::new(
                "wall_ms",
                Rule::OutOfRange,
                refusal::WALL_TIME_NOT_POSITIVE,
            ));
        }
        check::range(at.field("wall_ms"), self.wall_ms, 1, limits::MAX_WALL_MS)
    }

    /// Checks that the image is pinned by a `sha256:` digest, after
    /// [`JobStartSpec::validate`] has passed: a tag-only image is refused with
    /// [`refusal::IMAGE_DIGEST_REQUIRED`] naming `image`, as for a run start.
    pub fn require_pinned(&self) -> Result<(), ValidationError> {
        image::pinned(
            At::new("image", refusal::IMAGE_DIGEST_REQUIRED),
            &self.image,
        )
    }
}

/// The result of one job, resent in every heartbeat (inside its [`HeldJob`]) until a
/// [`JobResultAck`] names the job. The fields and their bounds are those of
/// [`crate::RunResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    /// The job's id.
    pub job_id: String,
    /// The allocation the job was started under.
    pub allocation_id: String,
    /// How the job ended.
    pub outcome: Outcome,
    /// The exit status, when the job exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A refusal code or short text saying why, when the outcome is not a clean exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The end of what the job printed, when the agent could read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<LogTail>,
}

impl JobResult {
    /// Checks the ids, the detail, the log tail and that the exit status agrees with the
    /// outcome, as [`crate::RunResult::validate`] does for a run.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("jobs", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("jobs.result.job_id"), &self.job_id)?;
        check::ident(at.field("jobs.result.allocation_id"), &self.allocation_id)?;
        let detail = at.field("jobs.result.detail");
        check::opt_text(detail, self.detail.as_ref(), limits::MAX_DETAIL_LEN)?;
        if let Some(tail) = &self.log_tail {
            let streams = [
                (at.field("jobs.result.log_tail.stdout"), &tail.stdout),
                (at.field("jobs.result.log_tail.stderr"), &tail.stderr),
            ];
            for (field, text) in streams {
                if text.len() > limits::MAX_LOG_STREAM_LEN {
                    return Err(field.fail(Rule::TooLong));
                }
            }
        }
        let exit = at.field("jobs.result.exit_code");
        match (self.outcome, self.exit_code) {
            (Outcome::Succeeded, Some(code)) if code != 0 => Err(exit.fail(Rule::Inconsistent)),
            (Outcome::Failed | Outcome::FailedToStart, Some(0)) => {
                Err(exit.fail(Rule::Inconsistent))
            }
            _ => Ok(()),
        }
    }
}

/// A job the agent holds, keyed by job id. A job stays listed until its container and
/// scratch are gone and its result is acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldJob {
    /// The job's id.
    pub job_id: String,
    /// Where the job is.
    pub state: HeldState,
    /// The job's result, once it has ended, until a [`JobResultAck`] names the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
}

impl HeldJob {
    /// Checks the job id and the result; a result names the job it is listed under.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("jobs", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("jobs.job_id"), &self.job_id)?;
        let Some(result) = &self.result else {
            return Ok(());
        };
        result.check(at)?;
        if result.job_id != self.job_id {
            return Err(at.field("jobs.result.job_id").fail(Rule::Inconsistent));
        }
        Ok(())
    }
}

/// The job listing of a heartbeat: at most [`limits::MAX_HELD`] jobs, no job id twice.
pub(crate) fn check_jobs(at: At, jobs: &[HeldJob]) -> Result<(), ValidationError> {
    let listed = at.field("jobs");
    check::count(listed, jobs.len(), limits::MAX_HELD)?;
    for job in jobs {
        job.check(at)?;
    }
    check::distinct(listed, jobs.iter().map(|job| job.job_id.as_str()))
}

/// A list of job ids (`retired_jobs` of a heartbeat, `retired_acks` of a reply): at most
/// [`limits::MAX_HELD`] ids, each a good id, none twice.
pub(crate) fn check_ids(at: At, ids: &[String]) -> Result<(), ValidationError> {
    check::count(at, ids.len(), limits::MAX_HELD)?;
    for id in ids {
        check::ident(at, id)?;
    }
    check::distinct(at, ids.iter())
}

/// The server's verdict on the result of one job; the agent then drops the result. The
/// verdicts are the [`Ack`]s of a run's result, with the same meaning; an
/// [`Ack::Unknown`] keeps the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResultAck {
    /// The job the result was for.
    pub job_id: String,
    /// The verdict.
    pub ack: Ack,
}

/// The `job_acks` of a reply: at most [`limits::MAX_RESULTS`], each a good job id, none
/// twice.
pub(crate) fn check_job_acks(at: At, acks: &[JobResultAck]) -> Result<(), ValidationError> {
    let field = at.field("job_acks");
    check::count(field, acks.len(), limits::MAX_RESULTS)?;
    for ack in acks {
        check::ident(field, &ack.job_id)?;
    }
    check::distinct(field, acks.iter().map(|ack| ack.job_id.as_str()))
}
