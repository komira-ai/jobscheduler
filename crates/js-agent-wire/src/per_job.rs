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

use js_types::{Arch, JobNetwork, Os, PortMap};
use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::command::Isolation;
use crate::error::{Rule, ValidationError};
use crate::files::{self, RunFile};
use crate::heartbeat::{Ack, HeldState, LogTail, Outcome};
use crate::image;
use crate::kind::StartKind;
use crate::limits;
use crate::log_sink::{self, LogReport, LogSink};
use crate::network;
use crate::refusal;
use crate::rootfs::{self, HostPort, JobMount, Layer};
use crate::run::RunSpec;

/// The hello capability of an agent that takes [`crate::Command::JobStart`] and
/// [`crate::Command::JobStop`] and reports `jobs` and `retired_jobs` in its heartbeat. Its
/// value is `true`. An agent without it reads both commands as [`crate::Command::Unknown`],
/// so a server sends them only to a host that reports it.
pub const PER_JOB_CAPABILITY: &str = "per-job";

/// The hello capability of an agent that fences its job starts. Its value is `true`. The
/// fence of a start is the pair already on it, [`JobStartSpec::job_id`] and
/// [`JobStartSpec::allocation_id`]; no field is added for it. An agent that reports it
/// remembers the fences of the jobs it stopped, was told to stop or saw end (the newest of
/// them, a bounded number), keeps them in its state root across a restart, and runs
/// nothing for a start whose fence is among them (a stop that names no allocation retires
/// every fence of the job). The same job id under a new allocation is a new fence and
/// runs. Such an agent also stops every job it holds after a bounded silence of the
/// server, and retires their fences. An agent without it promises neither: it may judge a
/// start by its job id alone, and it runs its jobs through any silence until their wall
/// limits. A server tells the two apart by this capability; nothing else on the wire
/// changes.
pub const START_FENCE_CAPABILITY: &str = "start-fence";

/// The hello capability of an agent that can launch a job with [`Isolation::Host`] from a
/// [`JobStartSpec`]. Its value is `true`. `js-agent` reports it when it was started with
/// `--allow-host-jobs`, which only its crun runtime takes. An agent without
/// it would run a start's `isolation: host` as a contained job, since it ignores the field,
/// so a server sends a host start only to a host that reports it. Nothing else on the wire
/// changes.
pub const HOST_JOBS_CAPABILITY: &str = "host-jobs";

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
/// The `run`, `os` and `arch` of the one job definition are here as on a run start
/// ([`crate::JobSpec::run`]); a native body and a bundle are not carried. Fields a later
/// change adds, as optional fields: the network policy and the declared ports (the job
/// network change), the kind of start, the isolation, and the layers, mounts and host
/// ports of a job's file system (`docs/design/job-rootfs.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStartSpec {
    /// The job's id; the key of every idempotent step. The grammar of every id here
    /// (`[A-Za-z0-9._-]`, leading alphanumeric, at most [`limits::MAX_ID_LEN`] bytes).
    pub job_id: String,
    /// The id of the allocation this start is for, new at every placement of the job.
    pub allocation_id: String,
    /// The container image, pinned by a `sha256:` digest ([`JobStartSpec::require_pinned`]),
    /// in the grammar of [`crate::JobSpec::image`]. It may be empty (or absent) only when
    /// `layers` is not: the root is then the layers alone, with no image configuration. A
    /// start with neither is refused `invalid-job`. Left out of the text when empty, which
    /// only a start with layers may be.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// The program and its arguments after the image, as [`crate::JobSpec::args`]: at most
    /// [`limits::MAX_ARGS`] entries, no NUL. With an empty `image` there is no image
    /// command to follow, so the first entry is the program, an absolute path in the root
    /// held to the rules of a run's `program` (refused `invalid-program` naming `command`
    /// otherwise, and when there is none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// What runs from the image ([`RunSpec`]), as [`crate::JobSpec::run`]: checked by
    /// [`RunSpec::validate`], and `command` is then empty (refused `invalid-job` naming
    /// `command` otherwise). An agent of this version runs no `run` yet and answers it
    /// `run-unsupported`, as for a run start. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSpec>,
    /// The operating system of the image manifest the digest pins, as
    /// [`crate::JobSpec::os`]. Absent when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    /// The CPU architecture of the image manifest the digest pins, as
    /// [`crate::JobSpec::arch`]. Absent when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<Arch>,
    /// The environment, literal and not secret, held to the rules of a run's
    /// ([`RunSpec::env`]) by the one check, [`crate::check_env`], naming `env`: at most
    /// [`limits::MAX_ENV`] entries, names `[A-Za-z_][A-Za-z0-9_]*` and not reserved, values
    /// without NUL. A container agent sets it in the job's container over the image's own
    /// variables (a name in both takes the job's value); an agent whose runtime cannot
    /// carry it (the macOS native runtime) answers a start whose environment is not empty
    /// `failed_to_start` with [`refusal::ENV_UNSUPPORTED`] and runs nothing.
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
    /// How much network the job's container has, as [`crate::JobSpec::network`]: `none`
    /// (the default, left out), `allow-list` or `open`; an unknown level fails to decode. An
    /// agent refuses a start above its ceiling (`network-above-ceiling`) and never runs it
    /// with less. An agent that predates the field ignores it, so a server sends a level other
    /// than `none` only to a host whose hello reports a ceiling that covers it
    /// ([`crate::NetworkCeiling`]).
    #[serde(default, skip_serializing_if = "JobNetwork::is_none")]
    pub network: JobNetwork,
    /// The ports the job declares, as [`crate::JobSpec::ports`], with the same bounds; left
    /// out when there are none, and refused or forwarded as there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
    /// Where to upload the job's output while it runs ([`LogSink`]), sent only to a host
    /// that reported [`crate::LOG_UPLOAD_CAPABILITY`]; absent means no live logs. It is
    /// never part of the start's acceptance: a value that does not decode reads as absent,
    /// and [`JobStartSpec::validate`] does not look at it; [`JobStartSpec::log_sink_checked`]
    /// does. An agent that predates the field ignores it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "log_sink::lenient"
    )]
    pub log_sink: Option<LogSink>,
    /// What the start runs in ([`StartKind`]): absent for the agent's own runtime, `vm` for a
    /// fresh VM; an unknown value fails to decode. An agent that predates the field ignores
    /// it, so a server sends `vm` only to a host whose hello reports `vm.slots`.
    #[serde(default, skip_serializing_if = "StartKind::is_action")]
    pub kind: StartKind,
    /// How the job is confined ([`Isolation`]): absent for `contained`, `host` for a job that
    /// runs on the machine itself; an unknown value fails to decode, so inside a reply that
    /// one start reads as [`crate::Command::Unknown`]. An agent that predates the field
    /// ignores it and would run the job contained, so a server sends `host` only to a host
    /// whose hello reports [`HOST_JOBS_CAPABILITY`].
    #[serde(default, skip_serializing_if = "Isolation::is_contained")]
    pub isolation: Isolation,
    /// The read-only layers of the job's root, base first, above `image` (or in place of it,
    /// when `image` is empty); a later layer's file hides an earlier one's. With layers the
    /// root is writable and the writes are the job's own. At most [`limits::MAX_LAYERS`];
    /// left out when there are none, and the root is then `image` alone, read-only.
    ///
    /// This and the two fields below are sent only to a host that reports the capability
    /// `layered-rootfs` (`js_caps::LAYERED_ROOTFS`): an agent that predates them ignores
    /// them and would run the job on its image alone. None is allowed with
    /// [`Isolation::Host`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<Layer>,
    /// What the job sees at paths of its file system ([`JobMount`]; today host
    /// directories), made after the
    /// root, parents before children. At most [`limits::MAX_MOUNTS`]; left out when there
    /// are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<JobMount>,
    /// Ports on the host's loopback ([`HostPort`]) the job reaches at ports of its own
    /// loopback, whatever its `network`. Not `ports`, which forward the other way. At most
    /// [`limits::MAX_PORTS`], each from 1 to 65535, none twice; left out when there are
    /// none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_ports: Vec<HostPort>,
}

impl JobStartSpec {
    /// Checks the start: the ids, that an image or a layer is named and the image well
    /// formed, the command, the environment, the files, the booked resources, the wall
    /// limit, the declared ports, and the layers, mounts and host ports
    /// ([`crate::rootfs`]). The server calls it before it sends a start and the agent before
    /// it acts on one, together with [`JobStartSpec::require_pinned`]. It does not look at
    /// `log_sink`: a sink never refuses a start ([`JobStartSpec::log_sink_checked`]).
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("job", refusal::INVALID_JOB);
        check::ident(at.field("job_id"), &self.job_id)?;
        check::ident(at.field("allocation_id"), &self.allocation_id)?;
        if self.image.is_empty() {
            self.check_imageless(at)?;
        } else {
            image::image(At::new("image", refusal::INVALID_IMAGE), &self.image)?;
        }
        check::args(at.field("command"), &self.command)?;
        if let Some(run) = &self.run {
            run.validate()?;
            if !self.command.is_empty() {
                return Err(at.field("command").fail(Rule::Inconsistent));
            }
        }
        crate::env::check_env("env", &self.env)?;
        files::validate_files(&self.files)?;
        self.booked.check(at)?;
        if self.wall_ms == 0 {
            return Err(ValidationError::new(
                "wall_ms",
                Rule::OutOfRange,
                refusal::WALL_TIME_NOT_POSITIVE,
            ));
        }
        check::range(at.field("wall_ms"), self.wall_ms, 1, limits::MAX_WALL_MS)?;
        network::check_ports(&network::JOB_PORTS, &self.ports)?;
        rootfs::check_layers(&self.layers)?;
        rootfs::check_mounts(&self.mounts)?;
        rootfs::check_host_ports(&self.host_ports, &self.ports)?;
        let rootfs = [
            self.layers.is_empty(),
            self.mounts.is_empty(),
            self.host_ports.is_empty(),
        ];
        if self.isolation == Isolation::Host && rootfs.contains(&false) {
            return Err(at.field("isolation").fail(Rule::Inconsistent));
        }
        Ok(())
    }

    /// A start with an empty image: it has layers (`image`, `invalid-job`), no `run`, which
    /// names its job by the image (`run`, `invalid-image`), and a command whose first entry
    /// is an absolute program (`command`, `invalid-program`).
    fn check_imageless(&self, at: At) -> Result<(), ValidationError> {
        if self.layers.is_empty() {
            return Err(at.field("image").fail(Rule::Empty));
        }
        if self.run.is_some() {
            return Err(At::new("run", refusal::INVALID_IMAGE).fail(Rule::Inconsistent));
        }
        let program = At::new("command", refusal::INVALID_PROGRAM);
        let Some(first) = self.command.first() else {
            return Err(program.fail(Rule::Empty));
        };
        check::path(program, js_types::path::absolute(first))
    }

    /// The start's [`LogSink`], checked: `Ok(None)` when there is none, and an error with
    /// [`refusal::INVALID_LOG_SINK`] naming the part (`log_sink.endpoint`, `log_sink.region`,
    /// `log_sink.bucket` or `log_sink.prefix`) when it is out of the form [`LogSink`] gives,
    /// or when its prefix does not end in this start's own job id and `/`. The server calls
    /// it before it sends a sink, and sends none when it fails; the agent calls it and, on an
    /// error, starts the job without a sink. It never refuses the start.
    ///
    /// # Errors
    ///
    /// The sink is malformed or names another job's prefix.
    pub fn log_sink_checked(&self) -> Result<Option<&LogSink>, ValidationError> {
        let Some(sink) = &self.log_sink else {
            return Ok(None);
        };
        let at = At::new("log_sink", refusal::INVALID_LOG_SINK);
        sink.check(at, &self.job_id).map(|()| Some(sink))
    }

    /// Checks that the image and every image layer are pinned by a `sha256:` digest, after
    /// [`JobStartSpec::validate`] has passed: a tag-only one is refused with
    /// [`refusal::IMAGE_DIGEST_REQUIRED`] naming `image` or `layers.image`, as for a run
    /// start. An empty image, which `validate` lets pass only beside layers, is not looked
    /// at.
    pub fn require_pinned(&self) -> Result<(), ValidationError> {
        if !self.image.is_empty() {
            image::pinned(
                At::new("image", refusal::IMAGE_DIGEST_REQUIRED),
                &self.image,
            )?;
        }
        rootfs::pin_layers(&self.layers)
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
    /// What the capture saw of each stream ([`LogReport`]), from a runtime that uploads live
    /// logs; absent otherwise. Never part of the result's acceptance: a value that does not
    /// decode reads as absent, [`JobResult::validate`] does not look at it, and
    /// [`JobResult::logs_report`] gives it only when it is consistent. A server from before
    /// the field ignores it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "log_sink::lenient"
    )]
    pub logs: Option<LogReport>,
    /// The code of an input the host provides that failed while the job ran:
    /// [`refusal::INPUT_UNREADABLE`] or [`refusal::HOST_PORT_LOST`], from an agent that
    /// reads a start's layers, mounts and host ports. The failure is then the host's,
    /// whatever the outcome and exit status, which are kept as they were. Absent otherwise,
    /// and left out of the text. An agent of this release on the `crun` runtime sets
    /// `host-port-lost` for a job that did not succeed after a restart at which a host port of
    /// it could not be offered again; none sets `input-unreadable` yet. `detail` keeps its
    /// meaning beside it. A server from before the field ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_failure: Option<String>,
}

impl JobResult {
    /// Checks the ids, the detail, the input failure, the log tail and that the exit
    /// status agrees with the outcome, as [`crate::RunResult::validate`] does for a run. It
    /// does not look at `logs`. The input failure is bounded as `detail` is; its code is not
    /// checked against a list, so a code a later agent adds still reaches the server.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("jobs", refusal::INVALID_HEARTBEAT))
    }

    /// The result's [`LogReport`] when it has one in which neither stream dropped more
    /// bytes than it had; `None` otherwise, and a server then stores nothing of it.
    #[must_use]
    pub fn logs_report(&self) -> Option<&LogReport> {
        self.logs.as_ref().filter(|report| report.consistent())
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("jobs.result.job_id"), &self.job_id)?;
        check::ident(at.field("jobs.result.allocation_id"), &self.allocation_id)?;
        let detail = at.field("jobs.result.detail");
        check::opt_text(detail, self.detail.as_ref(), limits::MAX_DETAIL_LEN)?;
        let input_failure = at.field("jobs.result.input_failure");
        let code = self.input_failure.as_ref();
        check::opt_text(input_failure, code, limits::MAX_DETAIL_LEN)?;
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
