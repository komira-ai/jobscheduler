//! The file fields of a job row: tag 19, the files' contents while the job is live, or tag
//! 44, the SHA-256 of each once it has ended ([`JobFiles`]). A row holds one of them; a row
//! written before tag 44 holds tag 19 alone and reads as live files, whatever the job's state.

use std::collections::BTreeMap;

use super::{DecodeError, Fields, Writer, job_tag as t, map, no_repeats, pairs_of, text_of};
use crate::record::{FileDigest, JobFiles};

/// Puts `files` in `row`: the contents under [`t::FILES`], or the digests under
/// [`t::FILE_DIGESTS`], never both.
pub(super) fn put_files(row: &mut Writer, files: &JobFiles) {
    match files {
        JobFiles::Contents(files) => row.put(
            t::FILES,
            map(files.iter().map(|(k, v)| (k.as_bytes(), v.as_slice()))),
        ),
        JobFiles::Digests(files) => row.put(
            t::FILE_DIGESTS,
            map(files
                .iter()
                .map(|(k, v)| (k.as_bytes(), v.bytes().as_slice()))),
        ),
    }
}

/// The files of a row: the contents of [`t::FILES`], or the digests of [`t::FILE_DIGESTS`]. A
/// build before the digests that rewrote a row holding them read no files and wrote an empty
/// `FILES` beside them, which reads as the digests; contents beside digests are refused,
/// since no build writes them.
pub(super) fn take_files(fields: &mut Fields) -> Result<JobFiles, DecodeError> {
    let contents = fields.opt(t::FILES, contents_of)?;
    match fields.opt(t::FILE_DIGESTS, digests_of)? {
        None => Ok(JobFiles::Contents(contents.unwrap_or_default())),
        Some(digests) if contents.is_none_or(|contents| contents.is_empty()) => {
            Ok(JobFiles::Digests(digests))
        }
        Some(_) => Err(DecodeError::BadValue { tag: t::FILES }),
    }
}

/// A map of text to bytes.
fn contents_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<String, Vec<u8>>, DecodeError> {
    let pairs = pairs_of(tag, &value)?;
    let count = pairs.len();
    let map: BTreeMap<String, Vec<u8>> = pairs
        .into_iter()
        .map(|(key, value)| -> Result<(String, Vec<u8>), DecodeError> {
            Ok((text_of(tag, key.to_vec())?, value.to_vec()))
        })
        .collect::<Result<_, DecodeError>>()?;
    no_repeats(tag, count, map)
}

/// A map of text to 32 bytes.
fn digests_of(tag: u16, value: Vec<u8>) -> Result<BTreeMap<String, FileDigest>, DecodeError> {
    let pairs = pairs_of(tag, &value)?;
    let count = pairs.len();
    let map: BTreeMap<String, FileDigest> = pairs
        .into_iter()
        .map(
            |(key, value)| -> Result<(String, FileDigest), DecodeError> {
                let bytes: [u8; 32] = value
                    .try_into()
                    .map_err(|_| DecodeError::BadLength { tag })?;
                Ok((text_of(tag, key.to_vec())?, FileDigest::from_bytes(bytes)))
            },
        )
        .collect::<Result<_, DecodeError>>()?;
    no_repeats(tag, count, map)
}

#[cfg(test)]
#[path = "file_fields_tests.rs"]
mod tests;
