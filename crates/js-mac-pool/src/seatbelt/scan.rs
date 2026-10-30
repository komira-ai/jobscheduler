//! The setuid and setgid files of a host, found by walking directories.

use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

/// The setuid and setgid bits of a file's mode.
const PRIVILEGED_BITS: u32 = 0o6000;

/// Every regular file under `roots` (links are not followed) whose mode has the setuid or
/// setgid bit, sorted, once each. A root that does not exist, and a directory that cannot
/// be read, hold nothing for this purpose: what cannot be read cannot be listed, and the
/// caller runs as the user the pool's files are visible to.
///
/// The start-up self-test refuses the allow model when this finds a file the base set does
/// not list ([`SelfTest::new`](super::SelfTest::new)): the base set grants whole system
/// directories, reads gate `exec`, and a file the image added would be runnable.
#[must_use]
pub fn scan_privileged(roots: &[&str]) -> Vec<String> {
    let mut stack: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let mut found: Vec<String> = Vec::new();
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file() && meta.mode() & PRIVILEGED_BITS != 0 {
            found.push(path.to_string_lossy().into_owned());
        }
    }
    found.sort();
    found.dedup();
    found
}
