//! The host fields of a job and the taint and cordon of its host: the job row's exec mode
//! (tag 14) and isolation (tag 47, [`job_host_tag::ISOLATION`]), the taint row
//! (`taint/<host>`, [`HostTaint`]) and the cordon row (`cordon/<host>`, the holder of the
//! cordon, if it names one), which have the same format byte and tag-length-value fields as
//! the other rows, with the tags of [`taint_tag`] and [`cordon_tag`], and the same
//! additive rules (B8): a tag this build does not know is skipped, and a format byte above
//! this build's is [`DecodeError::Newer`].
//!
//! Isolation is written only for a job that runs on the host (one byte, 1), so the row of a
//! contained job is the bytes a build before this field wrote. A row without the tag reads as
//! contained, and a value this build does not know is [`DecodeError::Newer`].
//!
//! A build from before the tag skips it as it skips any tag it does not know, so it reads a
//! host job as contained, and it does not read the taint table: a server is not rolled back
//! to such a build while a host job is recorded.

use js_types::{CordonHolder, ExecMode};

use super::{Carried, DecodeError, Fields, Writer, byte_of, job_id_of, text_of};
use crate::error::JobStoreError;
use crate::record::{HostTaint, Isolation, ScaleMark};

/// The tag isolation adds to a job row, after the ones of `job_tag` and `job_whole_tag`.
pub(crate) mod job_host_tag {
    /// Present (one byte, 1) when the job runs on the host itself.
    pub const ISOLATION: u16 = 47;
}

/// The tags of a taint row.
pub(crate) mod taint_tag {
    /// The job whose placement tainted the host (text).
    pub const JOB: u16 = 1;
}

/// The tags of a cordon row.
pub(crate) mod cordon_tag {
    /// The holder of the cordon (text); absent when the request that placed it named none.
    pub const HOLDER: u16 = 1;
}

/// The tags of a scale-down mark row (`hostmark/<host>`).
pub(crate) mod hostmark_tag {
    /// The mark (one byte: 1 draining, 2 releasing).
    pub const MARK: u16 = 1;
}

/// The value of [`job_host_tag::ISOLATION`] for a job that runs on the host.
const HOST: u8 = 1;
/// The value a reader takes for a job that runs contained: no build writes it.
const CONTAINED: u8 = 0;

/// Puts `isolation` in `row`: the tag for a job that runs on the host, nothing for a contained
/// one.
pub(super) fn put_isolation(row: &mut Writer, isolation: Isolation) {
    row.put_opt(
        job_host_tag::ISOLATION,
        (isolation == Isolation::Host).then_some(vec![HOST]),
    );
}

/// The isolation of a row: contained when the row lacks the tag.
pub(super) fn take_isolation(fields: &mut Fields) -> Result<Isolation, DecodeError> {
    Ok(fields
        .opt(job_host_tag::ISOLATION, isolation_of)?
        .unwrap_or_default())
}

/// The isolation a value holds: a value this build does not know is a later build's.
fn isolation_of(tag: u16, value: Vec<u8>) -> Result<Isolation, DecodeError> {
    match byte_of(tag, &value)? {
        CONTAINED => Ok(Isolation::Contained),
        HOST => Ok(Isolation::Host),
        _ => Err(DecodeError::Newer),
    }
}

/// The byte of the exec mode `exec`.
pub(super) fn exec_code(exec: ExecMode) -> u8 {
    match exec {
        ExecMode::Container => 0,
        ExecMode::SandboxExec => 1,
    }
}

/// The exec mode a value holds: a value this build does not know is a later build's.
pub(super) fn exec_of(tag: u16, value: Vec<u8>) -> Result<ExecMode, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(ExecMode::Container),
        1 => Ok(ExecMode::SandboxExec),
        _ => Err(DecodeError::Newer),
    }
}

/// The row of `taint`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_taint(taint: &HostTaint) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    row.put(taint_tag::JOB, taint.job.as_str());
    row.finish(&Carried::default())
}

/// The taint a row holds. A tag this build does not know is skipped.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_taint(row: &[u8]) -> Result<HostTaint, DecodeError> {
    let mut fields = Fields::of(row)?;
    let job = fields.need(taint_tag::JOB, job_id_of)?;
    Ok(HostTaint { job })
}

/// The row of the scale-down mark `mark`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_hostmark(mark: ScaleMark) -> Result<Vec<u8>, JobStoreError> {
    let byte = match mark {
        ScaleMark::Draining => 1,
        ScaleMark::Releasing => 2,
    };
    let mut row = Writer::default();
    row.put(hostmark_tag::MARK, vec![byte]);
    row.finish(&Carried::default())
}

/// The scale-down mark a row holds. A tag this build does not know is skipped.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote or a mark this build does not know;
/// another [`DecodeError`] for one that does not parse.
pub(crate) fn decode_hostmark(row: &[u8]) -> Result<ScaleMark, DecodeError> {
    let mut fields = Fields::of(row)?;
    fields.need(hostmark_tag::MARK, |tag, value| {
        match byte_of(tag, &value)? {
            1 => Ok(ScaleMark::Draining),
            2 => Ok(ScaleMark::Releasing),
            _ => Err(DecodeError::Newer),
        }
    })
}

/// The row of a cordon held by `holder` (none: a cordon no holder named). The time the
/// cordon lapses is the row's expiry, not a field.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_cordon(holder: Option<&CordonHolder>) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    row.put_opt(
        cordon_tag::HOLDER,
        holder.map(|holder| holder.as_str().as_bytes().to_vec()),
    );
    row.finish(&Carried::default())
}

/// The holder a cordon row names, if it names one. A tag this build does not know is skipped.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_cordon(row: &[u8]) -> Result<Option<CordonHolder>, DecodeError> {
    let mut fields = Fields::of(row)?;
    fields.opt(cordon_tag::HOLDER, holder_of)
}

/// The cordon holder a value holds.
fn holder_of(tag: u16, value: Vec<u8>) -> Result<CordonHolder, DecodeError> {
    CordonHolder::new(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

#[cfg(test)]
#[path = "host_fields_tests.rs"]
mod tests;
