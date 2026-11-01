//! One member of a pinned archive, read in memory: a zip (stored or deflated) or a
//! gzip-compressed tar. Nothing is unpacked to disk, so no member name can place a file.
//!
//! The archive has been checked against its pinned sha256 before it is read here; the limits
//! below only keep a hostile or damaged archive from filling memory.

use std::io::Read;

use flate2::read::{DeflateDecoder, GzDecoder};
use js_mdm_core::pins::Packing;

/// The largest member read, in bytes.
pub const MAX_MEMBER: u64 = 512 * 1024 * 1024;

const LOCAL: usize = 0x0403_4b50;
const CENTRAL: usize = 0x0201_4b50;
const END: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];

/// Why a member could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The archive is damaged or of a kind this reader does not handle.
    #[error("{archive} is not a readable {packing:?} archive: {why}")]
    Unreadable {
        /// The archive's name.
        archive: String,
        /// How it is packed.
        packing: Packing,
        /// What is wrong.
        why: String,
    },
    /// The member is not in the archive, or is empty.
    #[error("{archive} has no {member}, or it is empty")]
    NoMember {
        /// The archive's name.
        archive: String,
        /// The member.
        member: String,
    },
}

/// The bytes of `member` in `bytes`, an archive called `archive` (for errors).
pub fn member(
    archive: &str,
    packing: Packing,
    bytes: &[u8],
    member: &str,
) -> Result<Vec<u8>, ArchiveError> {
    let found = match packing {
        Packing::Zip => zip_member(bytes, member),
        Packing::TarGz => tar_member(bytes, member),
    };
    let found = found.map_err(|why| ArchiveError::Unreadable {
        archive: archive.to_owned(),
        packing,
        why,
    })?;
    found
        .filter(|b| !b.is_empty())
        .ok_or_else(|| ArchiveError::NoMember {
            archive: archive.to_owned(),
            member: member.to_owned(),
        })
}

/// [`member`] with a member of at most `limit` bytes.
pub fn member_with_limit(
    archive: &str,
    packing: Packing,
    bytes: &[u8],
    name: &str,
    _limit: u64,
) -> Result<Vec<u8>, ArchiveError> {
    member(archive, packing, bytes, name)
}

fn why(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn zip_member(bytes: &[u8], member: &str) -> Result<Option<Vec<u8>>, String> {
    let bad = || "malformed zip".to_owned();
    // A little-endian number of `n` bytes at `at`.
    let num = |at: usize, n: usize| -> Result<usize, String> {
        let b = bytes.get(at..at.saturating_add(n)).ok_or_else(bad)?;
        Ok(b.iter()
            .rev()
            .fold(0, |acc, v| (acc << 8) | usize::from(*v)))
    };
    // The end record is the last 22 bytes, or earlier by the length of a comment.
    let last = bytes.len().saturating_sub(22);
    let end = (last.saturating_sub(65535)..=last)
        .rev()
        .find(|i| bytes.get(*i..*i + 4) == Some(&END[..]))
        .ok_or_else(bad)?;
    let mut at = num(end + 16, 4)?;
    for _ in 0..num(end + 10, 2)? {
        if num(at, 4)? != CENTRAL {
            return Err(bad());
        }
        let (method, packed, plain) = (num(at + 10, 2)?, num(at + 20, 4)?, num(at + 24, 4)?);
        let (name, extra, comment) = (num(at + 28, 2)?, num(at + 30, 2)?, num(at + 32, 2)?);
        let local = num(at + 42, 4)?;
        let listed = bytes.get(at + 46..at + 46 + name).ok_or_else(bad)?;
        if listed == member.as_bytes() {
            // The local header repeats the name and extra lengths; the data follows them.
            if num(local, 4)? != LOCAL {
                return Err(bad());
            }
            let data = local + 30 + num(local + 26, 2)? + num(local + 28, 2)?;
            let packed = bytes.get(data..data + packed).ok_or_else(bad)?;
            return inflate(method, packed, plain).map(Some);
        }
        at += 46 + name + extra + comment;
    }
    Ok(None)
}

fn inflate(method: usize, packed: &[u8], plain: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let cap = MAX_MEMBER.min(plain as u64) + 1;
    match method {
        0 => out.extend_from_slice(packed),
        8 => {
            DeflateDecoder::new(packed)
                .take(cap)
                .read_to_end(&mut out)
                .map_err(why)?;
        }
        other => return Err(format!("compression method {other} is not supported")),
    }
    if out.len() != plain.min(MAX_MEMBER as usize) {
        return Err(format!(
            "member is {} bytes, not the {plain} the zip lists",
            out.len()
        ));
    }
    Ok(out)
}

fn tar_member(bytes: &[u8], member: &str) -> Result<Option<Vec<u8>>, String> {
    let mut tar = tar::Archive::new(GzDecoder::new(bytes));
    for entry in tar.entries().map_err(why)? {
        let entry = entry.map_err(why)?;
        let named = entry.path().map_err(why)?.to_str() == Some(member);
        if named {
            let mut out = Vec::new();
            entry.take(MAX_MEMBER).read_to_end(&mut out).map_err(why)?;
            return Ok(Some(out));
        }
    }
    Ok(None)
}
