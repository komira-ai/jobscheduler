//! Work sent down to the agent: [`Command`] and the [`JobSpec`] a start carries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::files::{self, RunFile};
use crate::hello::WireShape;
use crate::image;
use crate::limits;
use crate::native::{self, NativeSpec};
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
    /// The path below that root: relative, split on `/` alone (on every OS), every part
    /// a name: no empty part, no `.` and no `..` part (so `.` and `./` alone, which name
    /// the root, are refused), no `\`, no `:` and no `,` (so it cannot add a field to a
    /// mount option string), and none of the control, format and separator characters
    /// free text refuses (U+202E among them). At most [`limits::MAX_REL_LEN`] bytes.
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
            .any(|c| check::hidden(c) || matches!(c, ':' | ',' | '\\'));
        if unsafe_char {
            return Err(rel.fail(Rule::BadChars));
        }
        // Split on `/` alone, never through `std::path`, so the answer is the same on
        // every OS the validator runs on.
        if self.rel.starts_with('/') || self.rel.split('/').any(|part| part == "..") {
            return Err(rel.fail(Rule::Escapes));
        }
        // A `.` part is a no-op like an empty one, so both report `Empty`; only a part that
        // can leave the root (`..`, a leading `/`) is `Escapes`.
        if self
            .rel
            .split('/')
            .any(|part| part.is_empty() || part == ".")
        {
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
    /// The container image, empty for a native job (see [`JobSpec::native`]); a decoded start
    /// may leave it out. The agent holds no registry credential, so it must be
    /// present on the host or pullable without one. A registry reference
    /// `name[:tag][@digest]` as the OCI distribution grammar defines it (a domain with an
    /// optional port only in the first component, lower-case path components, a tag of
    /// at most 128 bytes, a digest `algorithm:hex`). A transport prefix such as `oci:`,
    /// `dir:`, `docker-archive:`, `containers-storage:` or `docker://` is refused, and a
    /// reference starts with a letter or digit, so it is never read as an option. A
    /// start must also pin the image: [`JobSpec::require_pinned`].
    #[serde(default)]
    pub image: String,
    /// What to run on the host instead of a container. A start has exactly one of a
    /// non-empty `image` and this body; `image` is empty (and still sent) for a native job,
    /// so an agent that predates the body refuses the start as `invalid-image` and never
    /// runs it as a container. A native job is contained, and its `args`, `gpu_indices`
    /// and `mounts` are empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeSpec>,
    /// The command line after the image: at most [`limits::MAX_ARGS`] entries, no NUL.
    #[serde(default)]
    pub args: Vec<String>,
    /// What the agent enforces.
    pub limits: Limits,
    /// The GPU device indices the job may use. A device is in at most one job.
    #[serde(default)]
    pub gpu_indices: Vec<u32>,
    /// The directories the job sees: at most [`limits::MAX_MOUNTS`], none twice (the same
    /// root and rel is a duplicate whether the two are read-only or not).
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Files the agent writes for the job before it starts, read at `/run/js-files/<name>`
    /// (see [`RunFile`]): at most [`limits::MAX_RUN_FILES`], contents secret. Contained
    /// jobs only: an agent refuses a host job that has any. An agent that predates the
    /// field ignores it, so a server sends a start with files only to an agent that
    /// reported the `run-files` capability. Absent means none, and none is not written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RunFile>,
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
    /// A darwin bundle the job pins: `name[:tag]@sha256:<manifest digest>`, at most one.
    /// It is checked like `image` (same alphabet and length) and must be pinned
    /// ([`JobSpec::require_pinned`]). Every bundle is refused today: without `native` an
    /// agent from before this field ignores it and would run the job without the tools,
    /// and with `native` no agent can unpack one yet (a later change allows the pair).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

impl JobSpec {
    /// Checks that the image is pinned by a `sha256:` digest, after [`JobSpec::validate`]
    /// has passed. A tag-only image is refused with [`refusal::IMAGE_DIGEST_REQUIRED`]
    /// naming `image`, and a tag-only bundle with [`refusal::BUNDLE_DIGEST_REQUIRED`]
    /// naming `bundle`; `name:tag@sha256:...` passes. The server calls it with `validate`
    /// before it queues a start and the agent before it acts on one, so no host pulls an
    /// image whose content can move.
    pub fn require_pinned(&self) -> Result<(), ValidationError> {
        // The bundle first: a native job returns early below and must not skip it.
        if let Some(bundle) = &self.bundle {
            image::bundle_pinned(At::new("bundle", refusal::BUNDLE_DIGEST_REQUIRED), bundle)?;
        }
        if self.native.is_some() {
            return Ok(());
        }
        image::pinned(
            At::new("image", refusal::IMAGE_DIGEST_REQUIRED),
            &self.image,
        )
    }

    /// Exactly one of the image and the native body names what runs; a native job also
    /// leaves the container-only fields unused.
    fn check_body(&self) -> Result<(), ValidationError> {
        let at = At::new("image", refusal::INVALID_IMAGE);
        let Some(native) = &self.native else {
            return image::image(at, &self.image);
        };
        if !self.image.is_empty() {
            return Err(at.fail(Rule::Inconsistent));
        }
        native.validate()?;
        native::check_job(self)
    }

    /// Checks the start contract and every bound: ids, image, arguments, limits, GPU
    /// indices, mounts, files and labels. The server calls it before it queues a start and the
    /// agent before it acts on one.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("job", refusal::INVALID_JOB);
        check::ident(at.field("run_id"), &self.run_id)?;
        check::ident(at.field("lease_id"), &self.lease_id)?;
        self.check_bundle()?;
        self.check_body()?;
        check::args(at.field("args"), &self.args)?;
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
        // `rel` has no empty, `.` or `//` part, so two spellings of one path are equal.
        let places = self
            .mounts
            .iter()
            .map(|mount| (mount.root_name.as_str(), mount.rel.as_str()));
        check::distinct(mounts, places)?;
        files::validate_files(&self.files)?;
        check::labels(at.field("labels"), &self.labels)?;
        check::range(
            at.field("valid_for_ms"),
            self.valid_for_ms,
            0,
            limits::MAX_VALID_FOR_MS,
        )
    }

    /// A bundle is the toolchain of a native job, and no runtime can fetch one yet, so
    /// every bundle is refused: without a native body the job is a container (or empty)
    /// job that an agent from before the field would run without the tools, and beside a
    /// native body the agent would run the program with the host's tools. The grammar is
    /// checked first so a malformed name reads as such.
    fn check_bundle(&self) -> Result<(), ValidationError> {
        let Some(bundle) = &self.bundle else {
            return Ok(());
        };
        let at = At::new("bundle", refusal::INVALID_BUNDLE);
        image::bundle(at, bundle)?;
        if self.native.is_some() {
            return Err(at.fail(Rule::Inconsistent));
        }
        Err(at.fail(Rule::NeedsNative))
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
    /// Stop one run the server holds no live record of, and do not report it.
    ///
    /// The server sends it for every run the agent lists that the server has no live
    /// record of. That is a run of a lease that has ended, and also a run whose record the
    /// server lost (a server restart forgets its runs, and the lease can still hold the
    /// host). In both cases the work is lost: the agent kills the run and sends no result,
    /// so nothing tells the server or the lease holder what became of it.
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
            Self::Start(spec) => spec.validate().and_then(|()| spec.require_pinned()),
            Self::Stop { run_id } | Self::Cancel { run_id } => {
                check::ident(At::new("run_id", refusal::INVALID_REPLY), run_id)
            }
            Self::Unknown => Ok(()),
        }
    }
}
