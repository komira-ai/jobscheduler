//! The bytes of the job, allocation, mark and taint rows (`mark_fields`, `host_fields`).
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
//! integer is big-endian of its width, a time is the store clock's reading as whole seconds
//! (`u64`) and the nanoseconds within the second (`u32`), text is UTF-8, a list is its items
//! each as `len:u32` and the bytes, and a map is a list of its keys and values in turn, in
//! key order, no key twice. A marker (the whole host) is a field with an empty value. A field
//! whose value is `None` is not written; a list or map that is empty is written empty.
//!
//! **Additive only (B8).** A reader skips a tag it does not know and keeps its bytes, and the
//! record carries them ([`Carried`]) into the next write of the row, so a build one release
//! back can move a job a newer build wrote without losing the newer fields. A field this
//! build knows but the row lacks reads as its default (`None`, empty, zero, the default
//! network policy), so a row an older build wrote reads too. A row whose format byte is above
//! [`FORMAT`], or which holds an enum value this build does not know (but a queue code or event
//! kind, which reads as unspecified: `queue_fields`), is [`DecodeError::Newer`]: the store
//! answers it as an unknown row, never as a missing one.
//! Any other row that does not parse is an error naming what is wrong; nothing here panics or
//! reads past a length.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{HostId, ImageDigestRef, Labels, RESERVED_PREFIX, ReservedKey, Resources};

use crate::error::JobStoreError;
use crate::ids::{AllocationId, ClientName, JobId};
use crate::record::{
    AllocationRecord, AllocationStep, Booked, Constraints, DeclaredPort, EndReason, JobRecord,
    JobSpec, JobState, Needs, NetworkPolicy, StartFailure,
};

mod file_fields;
mod host_fields;
mod mark_fields;
mod queue_fields;
mod result_fields;
pub(crate) mod rootfs_fields;
#[cfg(any(test, feature = "test-support"))]
mod test_support;
mod whole_fields;

#[cfg(any(test, feature = "test-support"))]
pub(crate) use host_fields::job_host_tag;
pub(crate) use host_fields::{decode_taint, encode_taint};
pub(crate) use mark_fields::{decode_mark, encode_mark};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use test_support::{LATER_FORMAT, field_of, rearranged, set_field};

/// The format byte of the rows this build writes.
pub(crate) const FORMAT: u8 = 1;

/// The tags of a job row (the whole-host tags 45 and 46 are in `whole_fields`, the isolation
/// tag 47 in `host_fields`, the queue tags 48 to 53 in `queue_fields`, 54 to 58 in
/// `rootfs_fields`).
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
    /// The files' contents (map of text to bytes), while the job is live. A row holds this
    /// or [`FILE_DIGESTS`], never both with a file in each.
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
    /// Why the job ended, when it has a result (u8). A row without it has no result, and
    /// the tags up to [`RESULT_USAGE`] and [`RESULT_LOGS`] are read only beside it.
    pub const RESULT_REASON: u16 = 31;
    /// The exit code (i32).
    pub const RESULT_EXIT_CODE: u16 = 32;
    /// The tail of standard output (text).
    pub const RESULT_STDOUT: u16 = 33;
    /// The tail of standard error (text).
    pub const RESULT_STDERR: u16 = 34;
    /// Whether the job wrote more than the tails hold (u8, 0 or 1).
    pub const RESULT_TRUNCATED: u16 = 35;
    /// When the job was queued, as the result reports it (time).
    pub const RESULT_QUEUED_AT: u16 = 36;
    /// When it was placed, as the result reports it (time).
    pub const RESULT_PLACED_AT: u16 = 37;
    /// When it began running, as the result reports it (time).
    pub const RESULT_STARTED_AT: u16 = 38;
    /// When it ended, as the result reports it (time).
    pub const RESULT_ENDED_AT: u16 = 39;
    /// What it used: the CPU time (time) and then the peak memory in bytes (u64).
    pub const RESULT_USAGE: u16 = 40;
    /// The earliest instant a time limit may end the job (time).
    pub const DEADLINE_FLOOR: u16 = 41;
    /// The name of the client that submitted the job (text).
    pub const CLIENT: u16 = 42;
    /// The lengths of its output streams: standard output's bytes, standard error's, then the
    /// bytes each dropped (four u64).
    pub const RESULT_LOGS: u16 = 43;
    /// The SHA-256 of each file's contents (map of text to 32 bytes), once the job has ended:
    /// in place of [`FILES`], so a row with it holds no contents. A row a build before this
    /// field rewrote holds it beside an empty [`FILES`].
    pub const FILE_DIGESTS: u16 = 44;
}

/// The tags of an allocation row (the whole-host tags 14 and 15 are in `whole_fields`).
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
    /// The host ports it holds (2 bytes each, ascending, each once); absent when it holds none.
    pub const HOST_PORTS: u16 = 13;
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
    /// A row is stored under the key of another job than the one it names (field 1).
    #[error("the row names another job than its key")]
    WrongKey,
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

    /// The row of format [`FORMAT`]: [`Writer::finish_as`].
    fn finish(self, carried: &Carried) -> Result<Vec<u8>, JobStoreError> {
        self.finish_as(FORMAT, carried)
    }

    /// The row: the format byte `format`, then the fields and `carried`, in tag order.
    fn finish_as(self, format: u8, carried: &Carried) -> Result<Vec<u8>, JobStoreError> {
        let mut fields = carried.0.clone();
        fields.extend(self.0);
        let mut out = vec![format];
        for (tag, value) in fields {
            // The error is built eagerly: no test can allocate a field over 4 GiB, and a closure
            // that never runs would be a line no test covers.
            let len = u32::try_from(value.len())
                .ok()
                .ok_or(JobStoreError::TooLarge { tag })?;
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

/// A time: the seconds as a `u64` and the nanoseconds within the second as a `u32`, so every
/// reading of the store's clock is written as it is.
fn time(at: StoreTime) -> Vec<u8> {
    let since = at.as_duration();
    let mut out = since.as_secs().to_be_bytes().to_vec();
    out.extend_from_slice(&since.subsec_nanos().to_be_bytes());
    out
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
    rootfs_fields::put_image(row, spec.image.as_ref());
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
    row.put_opt(
        t::EXEC,
        constraints
            .exec
            .map(|exec| vec![host_fields::exec_code(exec)]),
    );
    row.put(t::CAPABILITIES, text_map(&constraints.capabilities));
    host_fields::put_isolation(row, constraints.isolation);
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
    file_fields::put_files(row, &spec.files);
    rootfs_fields::put_spec(row, &spec.layers, &spec.mounts, &spec.host_ports);
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
    result_fields::put_result(&mut row, job.result.as_ref());
    rootfs_fields::put_detail(&mut row, job.start_failure_detail.as_deref());
    row.put_opt(t::DEADLINE_FLOOR, job.deadline_floor.map(time));
    row.put_opt(
        t::CLIENT,
        job.client.as_ref().map(|name| text(name.as_str())),
    );
    whole_fields::put_job(&mut row, job);
    queue_fields::put_job(&mut row, &job.queue);
    let format = rootfs_fields::format_of(job.spec.image.as_ref());
    row.finish_as(format, &job.carried)
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
    if !allocation.host_ports.is_empty() {
        row.put(
            t::HOST_PORTS,
            allocation
                .host_ports
                .iter()
                .flat_map(|port| port.to_be_bytes())
                .collect::<Vec<u8>>(),
        );
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
    whole_fields::put_allocation(&mut row, allocation);
    row.finish(&allocation.carried)
}

// ---- reading ---------------------------------------------------------------------------

/// The fields of a row being read, and its format byte; what is left after the known ones
/// are taken is carried.
struct Fields(BTreeMap<u16, Vec<u8>>, u8);

impl Fields {
    /// The fields of `row`, an allocation, mark or taint row, of format [`FORMAT`].
    fn of(row: &[u8]) -> Result<Fields, DecodeError> {
        Self::up_to(row, FORMAT)
    }

    /// The fields of `row`, a job row, also of format `rootfs_fields::FORMAT_NO_IMAGE`.
    fn of_job(row: &[u8]) -> Result<Fields, DecodeError> {
        Self::up_to(row, rootfs_fields::FORMAT_NO_IMAGE)
    }

    /// The fields of `row`, whose format byte is from [`FORMAT`] to `newest`.
    fn up_to(row: &[u8], newest: u8) -> Result<Fields, DecodeError> {
        let (&format, mut rest) = row.split_first().ok_or(DecodeError::Empty)?;
        if format > newest {
            return Err(DecodeError::Newer);
        }
        if format < FORMAT {
            return Err(DecodeError::BadFormat);
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
            .then_some(Fields(fields, format))
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

    /// Whether the row holds the marker `tag`, whose value is empty.
    fn has(&mut self, tag: u16) -> Result<bool, DecodeError> {
        self.opt(tag, |tag, value| {
            value
                .is_empty()
                .then_some(())
                .ok_or(DecodeError::BadValue { tag })
        })
        .map(|marked| marked.is_some())
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
    let bytes: [u8; 12] = value
        .as_slice()
        .try_into()
        .map_err(|_| DecodeError::BadLength { tag })?;
    let [s0, s1, s2, s3, s4, s5, s6, s7, n0, n1, n2, n3] = bytes;
    let secs = u64::from_be_bytes([s0, s1, s2, s3, s4, s5, s6, s7]);
    let nanos = u32::from_be_bytes([n0, n1, n2, n3]);
    if nanos >= NANOS_PER_SEC {
        return Err(DecodeError::BadValue { tag });
    }
    Ok(StoreTime::from_duration(Duration::new(secs, nanos)))
}

/// Nanoseconds in a second: a time's nanoseconds are fewer.
const NANOS_PER_SEC: u32 = 1_000_000_000;

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

/// The keys and values of a map value, borrowed from it.
type Pairs<'a> = Vec<(&'a [u8], &'a [u8])>;

fn pairs_of(tag: u16, value: &[u8]) -> Result<Pairs<'_>, DecodeError> {
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

/// `map` when it kept every one of the `pairs` it was made from: a key written twice is
/// refused rather than one of its values dropped.
fn no_repeats<V>(
    tag: u16,
    pairs: usize,
    map: BTreeMap<String, V>,
) -> Result<BTreeMap<String, V>, DecodeError> {
    if map.len() == pairs {
        Ok(map)
    } else {
        Err(DecodeError::BadValue { tag })
    }
}

fn text_map_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<String, String>, DecodeError> {
    let pairs = pairs_of(tag, &value)?;
    let count = pairs.len();
    let map: BTreeMap<String, String> = pairs
        .into_iter()
        .map(|(key, value)| -> Result<(String, String), DecodeError> {
            Ok((text_of(tag, key.to_vec())?, text_of(tag, value.to_vec())?))
        })
        .collect::<Result<_, DecodeError>>()?;
    no_repeats(tag, count, map)
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

/// The host ports of an allocation row: 2 bytes each, each above the one before it, so none is
/// zero and none repeats.
fn host_ports_of(tag: u16, value: Vec<u8>) -> Result<Vec<u16>, DecodeError> {
    let chunks = value.chunks_exact(2);
    chunks
        .remainder()
        .is_empty()
        .then_some(())
        .ok_or(DecodeError::BadLength { tag })?;
    let ports: Vec<u16> = chunks
        .map(|port| u16::from_be_bytes([port[0], port[1]]))
        .collect();
    let ascending = ports
        .iter()
        .zip(std::iter::once(&0).chain(ports.iter()))
        .all(|(port, before)| port > before);
    ascending
        .then_some(ports)
        .ok_or(DecodeError::BadValue { tag })
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
/// set, each checked as when it was first made. A reserved key this build does not know is
/// one a later build sets: the row is [`DecodeError::Newer`], as for an enum value.
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
                .ok_or(DecodeError::Newer)?;
            labels.with_reserved(known, &value).map_err(|_| bad)
        })
}

fn job_id_of(tag: u16, value: Vec<u8>) -> Result<JobId, DecodeError> {
    JobId::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

fn client_of(tag: u16, value: Vec<u8>) -> Result<ClientName, DecodeError> {
    ClientName::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
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
        JobState::FailedToStart(StartFailure::InputUnavailable) => 12,
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
        12 => Ok(JobState::FailedToStart(StartFailure::InputUnavailable)),
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
    let image = rootfs_fields::take_image(fields)?;
    let command = fields.opt(t::COMMAND, texts_of)?.unwrap_or_default();
    let args = fields.opt(t::ARGS, texts_of)?.unwrap_or_default();
    let env = fields.opt(t::ENV, text_map_of)?.unwrap_or_default();
    let needs = if fields.has(t::WHOLE_HOST)? {
        Needs::WholeHost
    } else {
        Needs::Shape(take_resources(fields)?)
    };
    let constraints = Constraints {
        os: fields.opt(t::OS, text_of)?,
        arch: fields.opt(t::ARCH, text_of)?,
        exec: fields.opt(t::EXEC, host_fields::exec_of)?,
        capabilities: fields
            .opt(t::CAPABILITIES, text_map_of)?
            .unwrap_or_default(),
        isolation: host_fields::take_isolation(fields)?,
    };
    let wall_limit_secs = fields.need(t::WALL_LIMIT_SECS, u64_of)?;
    let network = fields.opt(t::NETWORK, network_of)?.unwrap_or_default();
    let ports = fields.opt(t::PORTS, ports_of)?.unwrap_or_default();
    let files = file_fields::take_files(fields)?;
    let labels = fields.opt(t::LABELS, labels_of)?.unwrap_or_default();
    let (layers, mounts, host_ports) = rootfs_fields::take_spec(fields)?;
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
        layers,
        mounts,
        host_ports,
    })
}

/// The job a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_job(row: &[u8]) -> Result<JobRecord, DecodeError> {
    use job_tag as t;
    let mut fields = Fields::of_job(row)?;
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
    let result = result_fields::take_result(&mut fields)?;
    let start_failure_detail = rootfs_fields::take_detail(&mut fields)?;
    let deadline_floor = fields.opt(t::DEADLINE_FLOOR, time_of)?;
    let client = fields.opt(t::CLIENT, client_of)?;
    let (failed_prepares, prepare_bars) = whole_fields::take_job(&mut fields)?;
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
        result,
        start_failure_detail,
        retention_ends,
        deadline_floor,
        client,
        failed_prepares,
        prepare_bars,
        queue: queue_fields::take_job(&mut fields)?,
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
    let booked = if fields.has(t::WHOLE_HOST)? {
        Booked::WholeHost
    } else {
        Booked::Shape {
            cpu_millis: fields.opt(t::CPU_MILLIS, u64_of)?.unwrap_or(0),
            memory_bytes: fields.opt(t::MEMORY_BYTES, u64_of)?.unwrap_or(0),
            disk_bytes: fields.opt(t::DISK_BYTES, u64_of)?.unwrap_or(0),
            accelerators: fields.opt(t::ACCELERATORS, u64_of)?.unwrap_or(0),
        }
    };
    let host_ports = fields
        .opt(t::HOST_PORTS, host_ports_of)?
        .unwrap_or_default();
    let step = fields.need(t::STEP, step_of)?;
    let created_at = fields.need(t::CREATED_AT, time_of)?;
    let freed_at = fields.opt(t::FREED_AT, time_of)?;
    let end_reason = fields.opt(t::END_REASON, reason_of)?;
    let (prepare_deadline, wall_limit_secs) = whole_fields::take_allocation(&mut fields)?;
    Ok(AllocationRecord {
        job,
        allocation_id,
        host,
        booked,
        host_ports,
        step,
        created_at,
        freed_at,
        end_reason,
        prepare_deadline,
        wall_limit_secs,
        carried: fields.carried(),
    })
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codec_props.rs"]
mod props;
