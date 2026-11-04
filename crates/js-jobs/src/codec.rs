//! The bytes of the job and allocation rows.
//!
//! A row is a format byte ([`FORMAT`]) and then fields, each a tag (two bytes, big-endian),
//! a length (four bytes, big-endian) and that many bytes of value:
//!
//! ```text
//! row   = format:u8 field*
//! field = tag:u16 len:u32 value[len]
//! ```
//!
//! Fields are written in tag order, so one record always has one encoding. Within a value an
//! integer is big-endian of its width, a time is the store clock's reading in nanoseconds as
//! a `u64`, text is UTF-8, a list is its items each as `len:u32` and the bytes, and a map is
//! a list of its keys and values in turn, in key order. A field whose value is `None` is not
//! written; a list or map that is empty is written empty.
//!
//! **Additive only (B8).** A reader skips a tag it does not know and keeps its bytes, and the
//! record carries them ([`Carried`]) into the next write of the row, so a build one release
//! back can move a job a newer build wrote without losing the newer fields. A field this
//! build knows but the row lacks reads as its default (`None`, empty, zero, the default
//! network policy), so a row an older build wrote reads too. A row whose format byte is above
//! [`FORMAT`], or which holds an enum value this build does not know, is
//! [`DecodeError::Newer`]: the store answers it as an unknown row, never as a missing one.
//! Any other row that does not parse is an error naming what is wrong; nothing here panics or
//! reads past a length.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{ExecMode, HostId, ImageDigestRef, Labels, RESERVED_PREFIX, ReservedKey, Resources};

use crate::error::JobStoreError;
use crate::ids::{AllocationId, JobId};
use crate::record::{
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, JobRecord,
    JobSpec, JobState, Needs, NetworkPolicy, StartFailure,
};

/// The format byte of the rows this build writes.
pub(crate) const FORMAT: u8 = 1;

/// The tags of a job row.
pub(crate) mod job_tag {
    /// The job's id (text).
    pub const ID: u16 = 1;
    /// The image, `name@digest` (text).
    pub const IMAGE: u16 = 2;
    /// The command (list of text).
    pub const COMMAND: u16 = 3;
    /// The arguments (list of text).
    pub const ARGS: u16 = 4;
    /// The environment (map of text).
    pub const ENV: u16 = 5;
    /// Present (empty) when the job needs the whole host.
    pub const WHOLE_HOST: u16 = 6;
    /// Thousandths of a CPU (u64).
    pub const CPU_MILLIS: u16 = 7;
    /// Bytes of memory (u64).
    pub const MEMORY_BYTES: u16 = 8;
    /// Accelerators (u64).
    pub const GPUS: u16 = 9;
    /// The accelerators' class (text).
    pub const GPU_CLASS: u16 = 10;
    /// Bytes of disk (u64).
    pub const DISK_BYTES: u16 = 11;
    /// The operating system (text).
    pub const OS: u16 = 12;
    /// The CPU architecture (text).
    pub const ARCH: u16 = 13;
    /// The execution mode (u8).
    pub const EXEC: u16 = 14;
    /// Further capabilities (map of text).
    pub const CAPABILITIES: u16 = 15;
    /// The wall limit in seconds (u64).
    pub const WALL_LIMIT_SECS: u16 = 16;
    /// The network policy (u8).
    pub const NETWORK: u16 = 17;
    /// The declared ports (u16 host and u16 container per port).
    pub const PORTS: u16 = 18;
    /// The files (map of text to bytes).
    pub const FILES: u16 = 19;
    /// The labels (map of text).
    pub const LABELS: u16 = 20;
    /// The priority (i32).
    pub const PRIORITY: u16 = 21;
    /// The state (u8).
    pub const STATE: u16 = 22;
    /// When it was recorded (time).
    pub const SUBMITTED_AT: u16 = 23;
    /// When it was placed (time).
    pub const PLACED_AT: u16 = 24;
    /// When it began running (time).
    pub const STARTED_AT: u16 = 25;
    /// When it ended (time).
    pub const ENDED_AT: u16 = 26;
    /// The host (text).
    pub const HOST: u16 = 27;
    /// Whether a cancel was asked for (u8, 0 or 1).
    pub const CANCEL_REQUESTED: u16 = 28;
    /// When the cancel was asked for (time).
    pub const CANCEL_REQUESTED_AT: u16 = 29;
    /// When the record may be deleted (time).
    pub const RETENTION_ENDS: u16 = 30;
}

/// The tags of an allocation row.
pub(crate) mod alloc_tag {
    /// The job (text).
    pub const JOB: u16 = 1;
    /// The allocation id (16 bytes).
    pub const ALLOCATION_ID: u16 = 2;
    /// The host (text).
    pub const HOST: u16 = 3;
    /// Present (empty) when it books the whole host.
    pub const WHOLE_HOST: u16 = 4;
    /// Thousandths of a CPU (u64).
    pub const CPU_MILLIS: u16 = 5;
    /// Bytes of memory (u64).
    pub const MEMORY_BYTES: u16 = 6;
    /// Bytes of disk (u64).
    pub const DISK_BYTES: u16 = 7;
    /// Accelerators (u64).
    pub const ACCELERATORS: u16 = 8;
    /// The step (u8).
    pub const STEP: u16 = 9;
    /// When it was made (time).
    pub const CREATED_AT: u16 = 10;
    /// When it was freed (time).
    pub const FREED_AT: u16 = 11;
    /// Why it was freed (u8).
    pub const END_REASON: u16 = 12;
}

/// The fields of a row this build does not know, by tag, as the row held them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Carried(pub(crate) BTreeMap<u16, Vec<u8>>);

/// Why a row's bytes are not a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The row has no bytes, so no format byte.
    #[error("the row is empty")]
    Empty,
    /// The format byte is 0, which no build writes.
    #[error("the row's format byte is 0")]
    BadFormat,
    /// A later build wrote the row: its format byte, or an enum value in it, is one this
    /// build does not know.
    #[error("the row was written by a later build")]
    Newer,
    /// The row ends inside a field's tag, length or value.
    #[error("the row ends inside a field")]
    Truncated,
    /// A tag appears twice.
    #[error("field {tag} appears twice")]
    DuplicateTag {
        /// The tag.
        tag: u16,
    },
    /// A field every row holds is missing.
    #[error("field {tag} is missing")]
    Missing {
        /// The tag.
        tag: u16,
    },
    /// A fixed-width value has another width.
    #[error("field {tag} has the wrong length")]
    BadLength {
        /// The tag.
        tag: u16,
    },
    /// A text value is not UTF-8.
    #[error("field {tag} is not UTF-8")]
    NotUtf8 {
        /// The tag.
        tag: u16,
    },
    /// A value parses but is not valid for its field (a list that ends inside an item, an
    /// id or image reference that does not parse, a label that is refused).
    #[error("field {tag} holds a value that is not valid")]
    BadValue {
        /// The tag.
        tag: u16,
    },
}

// ---- writing ---------------------------------------------------------------------------

/// The fields of a row being written.
#[derive(Default)]
struct Writer(BTreeMap<u16, Vec<u8>>);

impl Writer {
    fn put(&mut self, tag: u16, value: impl Into<Vec<u8>>) {
        self.0.insert(tag, value.into());
    }

    fn put_opt(&mut self, tag: u16, value: Option<Vec<u8>>) {
        if let Some(value) = value {
            self.put(tag, value);
        }
    }

    /// The row: the format byte, then the fields and `carried`, in tag order.
    fn finish(self, carried: &Carried) -> Result<Vec<u8>, JobStoreError> {
        let mut fields = carried.0.clone();
        fields.extend(self.0);
        let mut out = vec![FORMAT];
        for (tag, value) in fields {
            let len = u32::try_from(value.len()).map_err(|_| JobStoreError::TooLarge { tag })?;
            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&value);
        }
        Ok(out)
    }
}

fn text(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn time(at: StoreTime) -> Vec<u8> {
    // u64 nanoseconds reach past the year 2500 of a clock whose origin is the Unix epoch.
    let nanos = u64::try_from(at.as_duration().as_nanos()).unwrap_or(u64::MAX);
    nanos.to_be_bytes().to_vec()
}

/// A list value. An item longer than a `u32` length is cut here, but its field is then over
/// 4 GiB too, and [`Writer::finish`] refuses the row.
fn list<'a>(items: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(&(item.len() as u32).to_be_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// A map value: its keys and values in turn.
fn map<'a>(pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Vec<u8> {
    list(pairs.into_iter().flat_map(|(key, value)| [key, value]))
}

fn texts(items: &[String]) -> Vec<u8> {
    list(items.iter().map(String::as_bytes))
}

fn text_map(pairs: &BTreeMap<String, String>) -> Vec<u8> {
    map(pairs.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())))
}

fn put_spec(row: &mut Writer, spec: &JobSpec) {
    use job_tag as t;
    row.put(
        t::IMAGE,
        format!("{}@{}", spec.image.name(), spec.image.digest()),
    );
    row.put(t::COMMAND, texts(&spec.command));
    row.put(t::ARGS, texts(&spec.args));
    row.put(t::ENV, text_map(&spec.env));
    match &spec.needs {
        Needs::WholeHost => row.put(t::WHOLE_HOST, Vec::new()),
        Needs::Shape(resources) => {
            row.put(t::CPU_MILLIS, resources.cpu_millis().to_be_bytes());
            row.put(t::MEMORY_BYTES, resources.memory_bytes().to_be_bytes());
            row.put(t::GPUS, resources.gpus().to_be_bytes());
            row.put_opt(t::GPU_CLASS, resources.gpu_class().map(text));
            row.put(t::DISK_BYTES, resources.disk_bytes().to_be_bytes());
        }
    }
    let constraints = &spec.constraints;
    row.put_opt(t::OS, constraints.os.as_deref().map(text));
    row.put_opt(t::ARCH, constraints.arch.as_deref().map(text));
    row.put_opt(t::EXEC, constraints.exec.map(|exec| vec![exec_code(exec)]));
    row.put(t::CAPABILITIES, text_map(&constraints.capabilities));
    row.put(t::WALL_LIMIT_SECS, spec.wall_limit_secs.to_be_bytes());
    row.put(t::NETWORK, [network_code(spec.network)]);
    let ports: Vec<u8> = spec
        .ports
        .iter()
        .flat_map(|port| {
            let [h0, h1] = port.host.to_be_bytes();
            let [c0, c1] = port.container.to_be_bytes();
            [h0, h1, c0, c1]
        })
        .collect();
    row.put(t::PORTS, ports);
    row.put(
        t::FILES,
        map(spec.files.iter().map(|(k, v)| (k.as_bytes(), v.as_slice()))),
    );
    row.put(
        t::LABELS,
        map(spec
            .labels
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))),
    );
}

/// The row of `job`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_job(job: &JobRecord) -> Result<Vec<u8>, JobStoreError> {
    use job_tag as t;
    let mut row = Writer::default();
    row.put(t::ID, job.id.as_str());
    put_spec(&mut row, &job.spec);
    row.put(t::PRIORITY, job.priority.to_be_bytes());
    row.put(t::STATE, [state_code(job.state)]);
    row.put(t::SUBMITTED_AT, time(job.submitted_at));
    row.put_opt(t::PLACED_AT, job.placed_at.map(time));
    row.put_opt(t::STARTED_AT, job.started_at.map(time));
    row.put_opt(t::ENDED_AT, job.ended_at.map(time));
    row.put_opt(t::HOST, job.host.as_ref().map(|host| text(host.as_str())));
    row.put(t::CANCEL_REQUESTED, [u8::from(job.cancel_requested)]);
    row.put_opt(t::CANCEL_REQUESTED_AT, job.cancel_requested_at.map(time));
    row.put_opt(t::RETENTION_ENDS, job.retention_ends.map(time));
    row.finish(&job.carried)
}

/// The row of `allocation`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_allocation(allocation: &AllocationRecord) -> Result<Vec<u8>, JobStoreError> {
    use alloc_tag as t;
    let mut row = Writer::default();
    row.put(t::JOB, allocation.job.as_str());
    row.put(t::ALLOCATION_ID, allocation.allocation_id.bits());
    row.put(t::HOST, allocation.host.as_str());
    match allocation.booked {
        Booked::WholeHost => row.put(t::WHOLE_HOST, Vec::new()),
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes,
            accelerators,
        } => {
            row.put(t::CPU_MILLIS, cpu_millis.to_be_bytes());
            row.put(t::MEMORY_BYTES, memory_bytes.to_be_bytes());
            row.put(t::DISK_BYTES, disk_bytes.to_be_bytes());
            row.put(t::ACCELERATORS, accelerators.to_be_bytes());
        }
    }
    row.put(t::STEP, [step_code(allocation.step)]);
    row.put(t::CREATED_AT, time(allocation.created_at));
    row.put_opt(t::FREED_AT, allocation.freed_at.map(time));
    row.put_opt(
        t::END_REASON,
        allocation
            .end_reason
            .map(|reason| vec![reason_code(reason)]),
    );
    row.finish(&allocation.carried)
}

// ---- reading ---------------------------------------------------------------------------

/// The fields of a row being read; what is left after the known ones are taken is carried.
struct Fields(BTreeMap<u16, Vec<u8>>);

impl Fields {
    /// The fields of `row`.
    fn of(row: &[u8]) -> Result<Fields, DecodeError> {
        let (&format, mut rest) = row.split_first().ok_or(DecodeError::Empty)?;
        match format.cmp(&FORMAT) {
            Ordering::Equal => {}
            Ordering::Greater => return Err(DecodeError::Newer),
            Ordering::Less => return Err(DecodeError::BadFormat),
        }
        let mut fields = BTreeMap::new();
        while let Some((tag, after)) = rest.split_first_chunk::<2>() {
            let (len, after) = after
                .split_first_chunk::<4>()
                .ok_or(DecodeError::Truncated)?;
            let (value, after) = after
                .split_at_checked(u32::from_be_bytes(*len) as usize)
                .ok_or(DecodeError::Truncated)?;
            let tag = u16::from_be_bytes(*tag);
            if fields.insert(tag, value.to_vec()).is_some() {
                return Err(DecodeError::DuplicateTag { tag });
            }
            rest = after;
        }
        rest.is_empty()
            .then_some(Fields(fields))
            .ok_or(DecodeError::Truncated)
    }

    /// The field `tag` read by `read`, or `None` when the row lacks it.
    fn opt<T>(
        &mut self,
        tag: u16,
        read: impl FnOnce(u16, Vec<u8>) -> Result<T, DecodeError>,
    ) -> Result<Option<T>, DecodeError> {
        self.0
            .remove(&tag)
            .map(|value| read(tag, value))
            .transpose()
    }

    /// The field `tag` read by `read`; every row holds it.
    fn need<T>(
        &mut self,
        tag: u16,
        read: impl FnOnce(u16, Vec<u8>) -> Result<T, DecodeError>,
    ) -> Result<T, DecodeError> {
        self.opt(tag, read)?.ok_or(DecodeError::Missing { tag })
    }

    /// Whether the row holds `tag`, whatever its value.
    fn has(&mut self, tag: u16) -> bool {
        self.0.remove(&tag).is_some()
    }

    fn carried(self) -> Carried {
        Carried(self.0)
    }
}

fn u64_of(tag: u16, value: Vec<u8>) -> Result<u64, DecodeError> {
    value
        .as_slice()
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| DecodeError::BadLength { tag })
}

fn i32_of(tag: u16, value: Vec<u8>) -> Result<i32, DecodeError> {
    value
        .as_slice()
        .try_into()
        .map(i32::from_be_bytes)
        .map_err(|_| DecodeError::BadLength { tag })
}

fn byte_of(tag: u16, value: &[u8]) -> Result<u8, DecodeError> {
    match value {
        [byte] => Ok(*byte),
        _ => Err(DecodeError::BadLength { tag }),
    }
}

fn bool_of(tag: u16, value: Vec<u8>) -> Result<bool, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::BadValue { tag }),
    }
}

fn time_of(tag: u16, value: Vec<u8>) -> Result<StoreTime, DecodeError> {
    u64_of(tag, value).map(|nanos| StoreTime::from_duration(Duration::from_nanos(nanos)))
}

fn text_of(tag: u16, value: Vec<u8>) -> Result<String, DecodeError> {
    String::from_utf8(value).map_err(|_| DecodeError::NotUtf8 { tag })
}

/// The items of a list value.
fn items_of(tag: u16, value: &[u8]) -> Result<Vec<&[u8]>, DecodeError> {
    let mut items = Vec::new();
    let mut rest = value;
    while let Some((len, after)) = rest.split_first_chunk::<4>() {
        let (item, after) = after
            .split_at_checked(u32::from_be_bytes(*len) as usize)
            .ok_or(DecodeError::BadValue { tag })?;
        items.push(item);
        rest = after;
    }
    rest.is_empty()
        .then_some(items)
        .ok_or(DecodeError::BadValue { tag })
}

/// The keys and values of a map value.
fn pairs_of(tag: u16, value: &[u8]) -> Result<Vec<(&[u8], &[u8])>, DecodeError> {
    let items = items_of(tag, value)?;
    let pairs = items.chunks_exact(2);
    pairs
        .remainder()
        .is_empty()
        .then_some(())
        .ok_or(DecodeError::BadValue { tag })?;
    Ok(pairs.map(|pair| (pair[0], pair[1])).collect())
}

fn texts_of(tag: u16, value: Vec<u8>) -> Result<Vec<String>, DecodeError> {
    items_of(tag, &value)?
        .into_iter()
        .map(|item| text_of(tag, item.to_vec()))
        .collect()
}

fn text_map_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<String, String>, DecodeError> {
    pairs_of(tag, &value)?
        .into_iter()
        .map(|(key, value)| -> Result<(String, String), DecodeError> {
            Ok((text_of(tag, key.to_vec())?, text_of(tag, value.to_vec())?))
        })
        .collect()
}

fn files_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<String, Vec<u8>>, DecodeError> {
    pairs_of(tag, &value)?
        .into_iter()
        .map(|(key, value)| -> Result<(String, Vec<u8>), DecodeError> {
            Ok((text_of(tag, key.to_vec())?, value.to_vec()))
        })
        .collect()
}

fn ports_of(tag: u16, value: Vec<u8>) -> Result<Vec<DeclaredPort>, DecodeError> {
    let ports = value.chunks_exact(4);
    ports
        .remainder()
        .is_empty()
        .then_some(())
        .ok_or(DecodeError::BadLength { tag })?;
    Ok(ports
        .map(|port| DeclaredPort {
            host: u16::from_be_bytes([port[0], port[1]]),
            container: u16::from_be_bytes([port[2], port[3]]),
        })
        .collect())
}

/// The reserved label keys a row may hold.
const RESERVED: [ReservedKey; 5] = [
    ReservedKey::Tenant,
    ReservedKey::Run,
    ReservedKey::ValidationRun,
    ReservedKey::Definition,
    ReservedKey::Instance,
];

/// A label set as a row holds it: the caller's labels, and the reserved ones the scheduler
/// set, each checked as when it was first made.
fn labels_of(tag: u16, value: Vec<u8>) -> Result<Labels, DecodeError> {
    let bad = DecodeError::BadValue { tag };
    let (reserved, caller): (BTreeMap<String, String>, BTreeMap<String, String>) =
        text_map_of(tag, value)?
            .into_iter()
            .partition(|(key, _)| key.starts_with(RESERVED_PREFIX));
    let labels = Labels::from_caller(&caller).map_err(|_| bad)?;
    reserved
        .into_iter()
        .try_fold(labels, |labels, (key, value)| {
            let known = RESERVED
                .into_iter()
                .find(|reserved| reserved.key() == key)
                .ok_or(bad)?;
            labels.with_reserved(known, &value).map_err(|_| bad)
        })
}

fn job_id_of(tag: u16, value: Vec<u8>) -> Result<JobId, DecodeError> {
    JobId::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

fn host_of(tag: u16, value: Vec<u8>) -> Result<HostId, DecodeError> {
    HostId::new(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

fn image_of(tag: u16, value: Vec<u8>) -> Result<ImageDigestRef, DecodeError> {
    ImageDigestRef::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

fn allocation_id_of(tag: u16, value: Vec<u8>) -> Result<AllocationId, DecodeError> {
    value
        .as_slice()
        .try_into()
        .map(AllocationId::from_bits)
        .map_err(|_| DecodeError::BadLength { tag })
}

// ---- enum values -----------------------------------------------------------------------

fn state_code(state: JobState) -> u8 {
    match state {
        JobState::Queued => 0,
        JobState::Placed => 1,
        JobState::Starting => 2,
        JobState::Running => 3,
        JobState::Succeeded => 4,
        JobState::Failed => 5,
        JobState::Cancelled => 6,
        JobState::Lost => 7,
        JobState::FailedToStart(StartFailure::NoCapacity) => 8,
        JobState::FailedToStart(StartFailure::StartTimeout) => 9,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady) => 10,
        JobState::FailedToStart(StartFailure::AgentRefused) => 11,
    }
}

fn state_of(tag: u16, value: Vec<u8>) -> Result<JobState, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(JobState::Queued),
        1 => Ok(JobState::Placed),
        2 => Ok(JobState::Starting),
        3 => Ok(JobState::Running),
        4 => Ok(JobState::Succeeded),
        5 => Ok(JobState::Failed),
        6 => Ok(JobState::Cancelled),
        7 => Ok(JobState::Lost),
        8 => Ok(JobState::FailedToStart(StartFailure::NoCapacity)),
        9 => Ok(JobState::FailedToStart(StartFailure::StartTimeout)),
        10 => Ok(JobState::FailedToStart(StartFailure::ExclusiveNotReady)),
        11 => Ok(JobState::FailedToStart(StartFailure::AgentRefused)),
        _ => Err(DecodeError::Newer),
    }
}

fn exec_code(exec: ExecMode) -> u8 {
    match exec {
        ExecMode::Container => 0,
        ExecMode::SandboxExec => 1,
    }
}

fn exec_of(tag: u16, value: Vec<u8>) -> Result<ExecMode, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(ExecMode::Container),
        1 => Ok(ExecMode::SandboxExec),
        _ => Err(DecodeError::Newer),
    }
}

fn network_code(network: NetworkPolicy) -> u8 {
    match network {
        NetworkPolicy::None => 0,
        NetworkPolicy::AllowList => 1,
        NetworkPolicy::Open => 2,
    }
}

fn network_of(tag: u16, value: Vec<u8>) -> Result<NetworkPolicy, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(NetworkPolicy::None),
        1 => Ok(NetworkPolicy::AllowList),
        2 => Ok(NetworkPolicy::Open),
        _ => Err(DecodeError::Newer),
    }
}

fn step_code(step: AllocationStep) -> u8 {
    match step {
        AllocationStep::Booked => 0,
        AllocationStep::Releasing => 1,
        AllocationStep::Freed => 2,
    }
}

fn step_of(tag: u16, value: Vec<u8>) -> Result<AllocationStep, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(AllocationStep::Booked),
        1 => Ok(AllocationStep::Releasing),
        2 => Ok(AllocationStep::Freed),
        _ => Err(DecodeError::Newer),
    }
}

fn reason_code(reason: EndReason) -> u8 {
    match reason {
        EndReason::Exited => 0,
        EndReason::WallLimit => 1,
        EndReason::Memory => 2,
        EndReason::Cancelled => 3,
        EndReason::HostAbsent => 4,
        EndReason::FailedPrepare => 5,
        EndReason::StartTimeout => 6,
        EndReason::Refused => 7,
    }
}

fn reason_of(tag: u16, value: Vec<u8>) -> Result<EndReason, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(EndReason::Exited),
        1 => Ok(EndReason::WallLimit),
        2 => Ok(EndReason::Memory),
        3 => Ok(EndReason::Cancelled),
        4 => Ok(EndReason::HostAbsent),
        5 => Ok(EndReason::FailedPrepare),
        6 => Ok(EndReason::StartTimeout),
        7 => Ok(EndReason::Refused),
        _ => Err(DecodeError::Newer),
    }
}

// ---- records ---------------------------------------------------------------------------

fn take_resources(fields: &mut Fields) -> Result<Resources, DecodeError> {
    use job_tag as t;
    let cpu_millis = fields.opt(t::CPU_MILLIS, u64_of)?.unwrap_or(0);
    let memory_bytes = fields.opt(t::MEMORY_BYTES, u64_of)?.unwrap_or(0);
    let gpus = fields.opt(t::GPUS, u64_of)?.unwrap_or(0);
    let gpu_class = fields.opt(t::GPU_CLASS, text_of)?;
    let disk_bytes = fields.opt(t::DISK_BYTES, u64_of)?.unwrap_or(0);
    Resources::new(cpu_millis, memory_bytes, gpus, gpu_class)
        .map(|resources| resources.with_disk_bytes(disk_bytes))
        .map_err(|_| DecodeError::BadValue { tag: t::GPUS })
}

fn take_spec(fields: &mut Fields) -> Result<JobSpec, DecodeError> {
    use job_tag as t;
    let image = fields.need(t::IMAGE, image_of)?;
    let command = fields.opt(t::COMMAND, texts_of)?.unwrap_or_default();
    let args = fields.opt(t::ARGS, texts_of)?.unwrap_or_default();
    let env = fields.opt(t::ENV, text_map_of)?.unwrap_or_default();
    let needs = if fields.has(t::WHOLE_HOST) {
        Needs::WholeHost
    } else {
        Needs::Shape(take_resources(fields)?)
    };
    let constraints = Constraints {
        os: fields.opt(t::OS, text_of)?,
        arch: fields.opt(t::ARCH, text_of)?,
        exec: fields.opt(t::EXEC, exec_of)?,
        capabilities: fields
            .opt(t::CAPABILITIES, text_map_of)?
            .unwrap_or_default(),
    };
    let wall_limit_secs = fields.need(t::WALL_LIMIT_SECS, u64_of)?;
    let network = fields.opt(t::NETWORK, network_of)?.unwrap_or_default();
    let ports = fields.opt(t::PORTS, ports_of)?.unwrap_or_default();
    let files = fields.opt(t::FILES, files_of)?.unwrap_or_default();
    let labels = fields.opt(t::LABELS, labels_of)?.unwrap_or_default();
    Ok(JobSpec {
        image,
        command,
        args,
        env,
        needs,
        constraints,
        wall_limit_secs,
        network,
        ports,
        files,
        labels,
    })
}

/// The job a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_job(row: &[u8]) -> Result<JobRecord, DecodeError> {
    use job_tag as t;
    let mut fields = Fields::of(row)?;
    let id = fields.need(t::ID, job_id_of)?;
    let spec = take_spec(&mut fields)?;
    let priority = fields.opt(t::PRIORITY, i32_of)?.unwrap_or(0);
    let state = fields.need(t::STATE, state_of)?;
    let submitted_at = fields.need(t::SUBMITTED_AT, time_of)?;
    let placed_at = fields.opt(t::PLACED_AT, time_of)?;
    let started_at = fields.opt(t::STARTED_AT, time_of)?;
    let ended_at = fields.opt(t::ENDED_AT, time_of)?;
    let host = fields.opt(t::HOST, host_of)?;
    let cancel_requested = fields.opt(t::CANCEL_REQUESTED, bool_of)?.unwrap_or(false);
    let cancel_requested_at = fields.opt(t::CANCEL_REQUESTED_AT, time_of)?;
    let retention_ends = fields.opt(t::RETENTION_ENDS, time_of)?;
    Ok(JobRecord {
        id,
        spec,
        priority,
        state,
        submitted_at,
        placed_at,
        started_at,
        ended_at,
        host,
        cancel_requested,
        cancel_requested_at,
        retention_ends,
        carried: fields.carried(),
    })
}

/// The allocation a row holds.
///
/// # Errors
/// As [`decode_job`].
pub(crate) fn decode_allocation(row: &[u8]) -> Result<AllocationRecord, DecodeError> {
    use alloc_tag as t;
    let mut fields = Fields::of(row)?;
    let job = fields.need(t::JOB, job_id_of)?;
    let allocation_id = fields.need(t::ALLOCATION_ID, allocation_id_of)?;
    let host = fields.need(t::HOST, host_of)?;
    let booked = if fields.has(t::WHOLE_HOST) {
        Booked::WholeHost
    } else {
        Booked::Shape {
            cpu_millis: fields.opt(t::CPU_MILLIS, u64_of)?.unwrap_or(0),
            memory_bytes: fields.opt(t::MEMORY_BYTES, u64_of)?.unwrap_or(0),
            disk_bytes: fields.opt(t::DISK_BYTES, u64_of)?.unwrap_or(0),
            accelerators: fields.opt(t::ACCELERATORS, u64_of)?.unwrap_or(0),
        }
    };
    let step = fields.need(t::STEP, step_of)?;
    let created_at = fields.need(t::CREATED_AT, time_of)?;
    let freed_at = fields.opt(t::FREED_AT, time_of)?;
    let end_reason = fields.opt(t::END_REASON, reason_of)?;
    Ok(AllocationRecord {
        job,
        allocation_id,
        host,
        booked,
        step,
        created_at,
        freed_at,
        end_reason,
        carried: fields.carried(),
    })
}

/// The value of field `tag` in `row`, if the row parses and holds it. Test support.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn field_of(row: &[u8], tag: u16) -> Option<Vec<u8>> {
    Fields::of(row).ok()?.0.remove(&tag)
}

/// `row` with its field `tag` set to `value` (added, or put in place of the one it holds),
/// as a later build might write it; `None` when `row` does not parse. Test support.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_field(row: &[u8], tag: u16, value: &[u8]) -> Option<Vec<u8>> {
    let mut fields = Fields::of(row).ok()?;
    fields.0.insert(tag, value.to_vec());
    Writer(fields.0).finish(&Carried::default()).ok()
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
