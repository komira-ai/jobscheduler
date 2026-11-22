//! The job's outputs committed to tierstore's put ingest: one tar of the declared outputs, put
//! by the agent after the job, so the job is never given a writable put directory.
//!
//! The put directory of a Mac is a plain local directory the node watches
//! (`local_cache.put_ingest`): an object is committed at `<root>/cas/b/<build>/<nonce>` by
//! writing `pack.part`, renaming it to `pack.sha256-<64 hex>.part` (which declares the digest the
//! bytes must have) and renaming that to `pack`. The node stores it as the create-only object
//! `cas/b/<build>/<nonce>/pack` and removes the file. Nothing comes back: the key and the digest
//! are known to whoever committed it.
//!
//! The tar holds each output under its own path; links are kept as links, never followed, and an
//! output reached through a link is refused.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// What was committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Committed {
    /// The object's key.
    pub key: String,
    /// The SHA-256 of its bytes, which the commit declared.
    pub sha256: String,
    /// Its length.
    pub bytes: u64,
}

/// A writer that hashes and counts what passes through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Commits the `outputs` of the job folder's `work` as the object `cas/b/<build>/<nonce>/pack`
/// under `root`. `ready` is called on the job's put directory once it is made, before anything is
/// written in it (on a Mac it gives the directory to the node's group).
///
/// # Errors
/// An output is missing or reached through a link, or a write or rename failed.
pub fn commit(
    work: &Path,
    outputs: &[String],
    root: &Path,
    build: &str,
    nonce: &str,
    ready: &dyn Fn(&Path) -> io::Result<()>,
) -> io::Result<Committed> {
    for output in outputs {
        no_link_on_the_way(work, output)?;
    }
    let dir = root.join("cas").join("b").join(build).join(nonce);
    fs::create_dir_all(&dir)?;
    ready(&dir)?;
    let part = dir.join("pack.part");
    let mut writer = Hashing {
        inner: File::create(&part)?,
        hash: Sha256::new(),
        bytes: 0,
    };
    {
        let mut tar = tar::Builder::new(&mut writer);
        tar.follow_symlinks(false);
        for output in outputs {
            let path = work.join(output);
            if fs::symlink_metadata(&path)?.is_dir() {
                tar.append_dir_all(output, &path)?;
            } else {
                tar.append_path_with_name(&path, output)?;
            }
        }
        tar.finish()?;
    }
    writer.flush()?;
    writer.inner.sync_all()?;
    let sha256 = hex::encode(writer.hash.finalize());
    let declared = dir.join(declared_name(&sha256));
    fs::rename(&part, &declared)?;
    fs::rename(&declared, dir.join("pack"))?;
    Ok(Committed {
        key: format!("cas/b/{build}/{nonce}/pack"),
        sha256,
        bytes: writer.bytes,
    })
}

/// The name that declares the digest `sha256` (hex) of the object being committed.
#[must_use]
pub fn declared_name(sha256: &str) -> String {
    format!("pack.sha256-{sha256}.part")
}

/// Refuses an output any part of whose path, itself included, is a link, and one that is not a
/// regular file or a folder (a FIFO's read would never end).
fn no_link_on_the_way(work: &Path, output: &str) -> io::Result<()> {
    let mut at = work.to_owned();
    let mut kind = None;
    for part in output.split('/') {
        at.push(part);
        let file_type = fs::symlink_metadata(&at)?.file_type();
        if file_type.is_symlink() {
            return Err(io::Error::other(format!("{} is a link", at.display())));
        }
        kind = Some(file_type);
    }
    if kind.is_some_and(|k| !k.is_file() && !k.is_dir()) {
        return Err(io::Error::other(format!(
            "{} is not a regular file or a folder",
            at.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "put_tests.rs"]
mod tests;
