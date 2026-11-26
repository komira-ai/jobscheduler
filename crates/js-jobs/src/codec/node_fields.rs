//! The bytes of a host's row (`node/<host>`, [`NodeRecord`]): the same format byte and
//! tag-length-value fields as a job row, with the tags of [`node_tag`], and the same additive
//! rules (B8): a tag this build does not know is carried and written back, a known tag the row
//! lacks reads as its default, and a format byte this build does not know is
//! [`DecodeError::Newer`].
//!
//! Every row holds the host, the epoch, the pool and the time it was heard; every other field
//! is optional. Its default is what a host that announced nothing of it reads as: no session
//! digest, no shape, no labels, the phase `quarantined` (a phase text this build does not know
//! reads so too, as on the wire: fail closed), no load, the first listing applied, the last
//! listing clean, no beat applied, no container missed, no agent process (a hello from an agent
//! that sent none), no agent version. The two listing flags are markers named so that their
//! absence is that default (`FIRST_LISTING_PENDING`, `LISTING_UNCLEAN`).
//!
//! The agent version is written with the epoch of the hello that gave it, and read only while
//! that is the row's epoch. A build that does not know the tag carries it, so a hello such a
//! build takes leaves the version of the hello before; the epoch that hello moved on makes
//! that version read as none, never as the version of the agent now in session.
//!
//! A shape is a marker tag and the six tags after it: the resources as a job row writes them
//! and the capabilities as a map of text. A load is a marker and the six tags after it.

use std::collections::BTreeMap;

use js_agent_wire::Phase;
use js_types::{Resources, Shape};

use super::{
    DecodeError, Fields, Writer, host_of, job_id_of, map, no_repeats, pairs_of, text, text_map,
    text_map_of, text_of, time, time_of, u64_of,
};
use crate::error::JobStoreError;
use crate::node::{AppliedBeat, LoadReading, NodeRecord, NodeState, SessionDigest};

/// The tags of a host's row.
pub(crate) mod node_tag {
    /// The host (text).
    pub const HOST: u16 = 1;
    /// The epoch of the last hello (u64).
    pub const EPOCH: u16 = 2;
    /// The digest of the last hello's secret (32 bytes).
    pub const SESSION: u16 = 3;
    /// The pool (text).
    pub const POOL: u16 = 4;
    /// When the row was last heard (time).
    pub const HEARD_AT: u16 = 5;
    /// The phase (text, as on the wire).
    pub const PHASE: u16 = 6;
    /// The labels (map of text).
    pub const LABELS: u16 = 7;
    /// Present (empty) while the first listing since the hello is to be applied.
    pub const FIRST_LISTING_PENDING: u16 = 8;
    /// Present (empty) when the last listing was not clean.
    pub const LISTING_UNCLEAN: u16 = 9;
    /// The epoch of the last applied beat (u64).
    pub const BEAT_EPOCH: u16 = 10;
    /// The highest sequence applied in that epoch (u64).
    pub const BEAT_SEQ: u16 = 11;
    /// The confirmed containers left out by applied beats in a row (map of job id to u32).
    pub const MISSING: u16 = 12;
    /// The agent process whose hello wrote the row (text); absent for an agent that sent none.
    pub const AGENT_PROCESS: u16 = 13;
    /// The agent version of the hello that began the row's epoch: that epoch (u64), then the
    /// version (text); absent for a hello that gave none.
    pub const AGENT_VERSION: u16 = 14;
    /// Present (empty) when the row holds the host's announced shape, in the six tags after.
    pub const CLAIM: u16 = 20;
    /// Present (empty) when the row holds the shape the pool books, in the six tags after.
    pub const SHAPE: u16 = 30;
    /// Present (empty) when the row holds a load, in the six tags after.
    pub const LOAD: u16 = 40;
}

use node_tag as t;

/// The offsets from a shape's marker tag of its fields.
mod shape_at {
    /// Thousandths of a CPU (u64).
    pub const CPU_MILLIS: u16 = 1;
    /// Bytes of memory (u64).
    pub const MEMORY_BYTES: u16 = 2;
    /// Accelerators (u64).
    pub const GPUS: u16 = 3;
    /// The accelerators' class (text).
    pub const GPU_CLASS: u16 = 4;
    /// Bytes of disk (u64).
    pub const DISK_BYTES: u16 = 5;
    /// The capabilities (map of text).
    pub const CAPABILITIES: u16 = 6;
}

/// The offsets from the load's marker tag of its fields.
mod load_at {
    /// The reading's sequence (u64).
    pub const SEQ: u16 = 1;
    /// The agent's monotonic clock (u64).
    pub const MONO_USEC: u16 = 2;
    /// The CPU time (u64).
    pub const CPU_USAGE_USEC: u16 = 3;
    /// The working set (u64).
    pub const WORKING_SET_BYTES: u16 = 4;
    /// The active file cache (u64).
    pub const ACTIVE_FILE_BYTES: u16 = 5;
    /// Present (empty) when the actions cgroup has a CPU limit.
    pub const CPU_LIMITED: u16 = 6;
}

/// Puts `shape`, if any, at the marker `at` and the tags after it.
fn put_shape(row: &mut Writer, at: u16, shape: Option<&Shape>) {
    let Some(shape) = shape else { return };
    let resources = shape.resources();
    row.put(at, Vec::new());
    row.put(
        at + shape_at::CPU_MILLIS,
        resources.cpu_millis().to_be_bytes(),
    );
    row.put(
        at + shape_at::MEMORY_BYTES,
        resources.memory_bytes().to_be_bytes(),
    );
    row.put(at + shape_at::GPUS, resources.gpus().to_be_bytes());
    row.put_opt(at + shape_at::GPU_CLASS, resources.gpu_class().map(text));
    row.put(
        at + shape_at::DISK_BYTES,
        resources.disk_bytes().to_be_bytes(),
    );
    row.put(at + shape_at::CAPABILITIES, text_map(shape.capabilities()));
}

/// The shape at the marker `at`, `None` when the row has no marker there.
fn take_shape(fields: &mut Fields, at: u16) -> Result<Option<Shape>, DecodeError> {
    if !fields.has(at)? {
        return Ok(None);
    }
    let number = |fields: &mut Fields, offset| Ok(fields.opt(at + offset, u64_of)?.unwrap_or(0));
    let cpu_millis = number(fields, shape_at::CPU_MILLIS)?;
    let memory_bytes = number(fields, shape_at::MEMORY_BYTES)?;
    let gpus = number(fields, shape_at::GPUS)?;
    let gpu_class = fields.opt(at + shape_at::GPU_CLASS, text_of)?;
    let disk_bytes = number(fields, shape_at::DISK_BYTES)?;
    let capabilities = fields
        .opt(at + shape_at::CAPABILITIES, text_map_of)?
        .unwrap_or_default();
    let resources = Resources::new(cpu_millis, memory_bytes, gpus, gpu_class)
        .map_err(|_| DecodeError::BadValue {
            tag: at + shape_at::GPUS,
        })?
        .with_disk_bytes(disk_bytes);
    Ok(Some(Shape::new(resources, capabilities)))
}

/// Puts `load`, if any, at [`node_tag::LOAD`] and the tags after it.
fn put_load(row: &mut Writer, load: Option<&LoadReading>) {
    let Some(load) = load else { return };
    row.put(t::LOAD, Vec::new());
    for (offset, value) in [
        (load_at::SEQ, load.seq),
        (load_at::MONO_USEC, load.mono_usec),
        (load_at::CPU_USAGE_USEC, load.cpu_usage_usec),
        (load_at::WORKING_SET_BYTES, load.working_set_bytes),
        (load_at::ACTIVE_FILE_BYTES, load.active_file_bytes),
    ] {
        row.put(t::LOAD + offset, value.to_be_bytes());
    }
    if load.actions_cpu_limited {
        row.put(t::LOAD + load_at::CPU_LIMITED, Vec::new());
    }
}

/// The load at [`node_tag::LOAD`], `None` when the row has no marker there.
fn take_load(fields: &mut Fields) -> Result<Option<LoadReading>, DecodeError> {
    if !fields.has(t::LOAD)? {
        return Ok(None);
    }
    let mut number = |offset| Ok(fields.opt(t::LOAD + offset, u64_of)?.unwrap_or(0));
    let seq = number(load_at::SEQ)?;
    let mono_usec = number(load_at::MONO_USEC)?;
    let cpu_usage_usec = number(load_at::CPU_USAGE_USEC)?;
    let working_set_bytes = number(load_at::WORKING_SET_BYTES)?;
    let active_file_bytes = number(load_at::ACTIVE_FILE_BYTES)?;
    Ok(Some(LoadReading {
        seq,
        mono_usec,
        cpu_usage_usec,
        working_set_bytes,
        active_file_bytes,
        actions_cpu_limited: fields.has(t::LOAD + load_at::CPU_LIMITED)?,
    }))
}

/// The value of the missed containers: each job id and its count, four bytes, in turn.
fn missing_value(missing: &BTreeMap<crate::ids::JobId, u32>) -> Vec<u8> {
    let counts: Vec<(&str, [u8; 4])> = missing
        .iter()
        .map(|(job, count)| (job.as_str(), count.to_be_bytes()))
        .collect();
    map(counts
        .iter()
        .map(|(job, count)| (job.as_bytes(), count.as_slice())))
}

/// The missed containers of a row, no job twice.
fn missing_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<crate::ids::JobId, u32>, DecodeError> {
    let pairs = pairs_of(tag, &value)?;
    let count = pairs.len();
    let mut by_job = BTreeMap::new();
    for (job, times) in pairs {
        let times: [u8; 4] = times
            .try_into()
            .map_err(|_| DecodeError::BadLength { tag })?;
        by_job.insert(text_of(tag, job.to_vec())?, u32::from_be_bytes(times));
    }
    no_repeats(tag, count, by_job)?
        .into_iter()
        .map(|(job, times)| Ok((job_id_of(tag, job.into_bytes())?, times)))
        .collect()
}

/// The session digest a value holds.
fn session_of(tag: u16, value: Vec<u8>) -> Result<SessionDigest, DecodeError> {
    let bytes: [u8; 32] = value
        .try_into()
        .map_err(|_| DecodeError::BadLength { tag })?;
    Ok(SessionDigest::new(bytes))
}

/// The value of [`node_tag::AGENT_VERSION`]: the epoch of the hello that gave `version`, then
/// `version`.
fn version_value(epoch: u64, version: &str) -> Vec<u8> {
    let mut value = epoch.to_be_bytes().to_vec();
    value.extend_from_slice(version.as_bytes());
    value
}

/// The epoch and the agent version a value of [`node_tag::AGENT_VERSION`] holds.
fn version_of(tag: u16, value: Vec<u8>) -> Result<(u64, String), DecodeError> {
    let (epoch, version) = value
        .split_first_chunk::<8>()
        .ok_or(DecodeError::BadLength { tag })?;
    Ok((u64::from_be_bytes(*epoch), text_of(tag, version.to_vec())?))
}

/// The row of `node`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_node(node: &NodeRecord) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    let state = &node.state;
    row.put(t::HOST, text(node.host.as_str()));
    row.put(t::EPOCH, node.epoch.to_be_bytes());
    row.put_opt(
        t::SESSION,
        node.session.map(|digest| digest.as_bytes().to_vec()),
    );
    row.put(t::POOL, text(&node.pool));
    row.put(t::HEARD_AT, time(node.heard_at));
    row.put(t::PHASE, String::from(state.phase).into_bytes());
    row.put(t::LABELS, text_map(&node.labels));
    if state.first_listing_pending {
        row.put(t::FIRST_LISTING_PENDING, Vec::new());
    }
    if !state.last_listing_clean {
        row.put(t::LISTING_UNCLEAN, Vec::new());
    }
    if let Some(beat) = node.last_beat {
        row.put(t::BEAT_EPOCH, beat.epoch.to_be_bytes());
        row.put_opt(t::BEAT_SEQ, beat.seq.map(|seq| seq.to_be_bytes().to_vec()));
    }
    row.put(t::MISSING, missing_value(&state.missing_in_a_row));
    row.put_opt(t::AGENT_PROCESS, node.agent_process.as_deref().map(text));
    row.put_opt(
        t::AGENT_VERSION,
        node.agent_version
            .as_deref()
            .map(|version| version_value(node.epoch, version)),
    );
    put_shape(&mut row, t::CLAIM, node.claim.as_ref());
    put_shape(&mut row, t::SHAPE, node.shape.as_ref());
    put_load(&mut row, state.load.as_ref());
    row.finish(&node.carried)
}

/// The host's record a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_node(row: &[u8]) -> Result<NodeRecord, DecodeError> {
    let mut fields = Fields::of(row)?;
    let host = fields.need(t::HOST, host_of)?;
    let epoch = fields.need(t::EPOCH, u64_of)?;
    let session = fields.opt(t::SESSION, session_of)?;
    let pool = fields.need(t::POOL, text_of)?;
    let heard_at = fields.need(t::HEARD_AT, time_of)?;
    let phase = fields
        .opt(t::PHASE, text_of)?
        .map_or(Phase::Quarantined, Phase::from);
    let labels = fields.opt(t::LABELS, text_map_of)?.unwrap_or_default();
    let first_listing_pending = fields.has(t::FIRST_LISTING_PENDING)?;
    let last_listing_clean = !fields.has(t::LISTING_UNCLEAN)?;
    let beat_seq = fields.opt(t::BEAT_SEQ, u64_of)?;
    let last_beat = match fields.opt(t::BEAT_EPOCH, u64_of)? {
        Some(epoch) => Some(AppliedBeat {
            epoch,
            seq: beat_seq,
        }),
        None if beat_seq.is_some() => {
            return Err(DecodeError::Missing { tag: t::BEAT_EPOCH });
        }
        None => None,
    };
    let missing_in_a_row = fields.opt(t::MISSING, missing_of)?.unwrap_or_default();
    let agent_process = fields.opt(t::AGENT_PROCESS, text_of)?;
    // A version another epoch's hello gave is not that of the agent in this epoch's session.
    let agent_version = fields
        .opt(t::AGENT_VERSION, version_of)?
        .and_then(|(given_at, version)| (given_at == epoch).then_some(version));
    let claim = take_shape(&mut fields, t::CLAIM)?;
    let shape = take_shape(&mut fields, t::SHAPE)?;
    let load = take_load(&mut fields)?;
    Ok(NodeRecord {
        host,
        epoch,
        session,
        pool,
        claim,
        shape,
        labels,
        agent_process,
        agent_version,
        state: NodeState {
            phase,
            load,
            first_listing_pending,
            last_listing_clean,
            missing_in_a_row,
        },
        last_beat,
        heard_at,
        carried: fields.carried(),
    })
}

#[cfg(test)]
#[path = "node_fields_tests.rs"]
mod tests;
