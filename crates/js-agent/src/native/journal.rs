//! The lease journal: which lease users this agent may have asked the helper for and not
//! yet seen deleted.
//!
//! A lease is written (and synced to disk) before `user-create` is sent, and marked done
//! after `user-delete` answered or after a failed start was undone. A process that dies,
//! stops or is rescued with runs going leaves them in the journal; the next process (or the
//! rescue) reads the journal and ends every lease in it. The file is append-only text, one
//! `+ <lease>` or `- <lease>` per line; a line that is neither (a torn last line) is
//! ignored, and the leases that are live are the added ones not yet removed.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// The journal in the file at a path.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// The journal in `path`; the file is made when the first lease is written.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    /// Writes that `lease` may have a user, and syncs the file and its directory.
    ///
    /// # Errors
    ///
    /// The file cannot be written or synced.
    pub fn add(&self, lease: &str) -> io::Result<()> {
        self.append('+', lease)
    }

    /// Writes that `lease` has no user (any more).
    ///
    /// # Errors
    ///
    /// The file cannot be written or synced.
    pub fn done(&self, lease: &str) -> io::Result<()> {
        self.append('-', lease)
    }

    fn append(&self, sign: char, lease: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)?;
        writeln!(file, "{sign} {lease}")?;
        file.sync_all()?;
        let dir = self.path.parent().unwrap_or(Path::new("/"));
        File::open(dir)?.sync_all()
    }

    /// The leases written and not marked done, in the order they sort. A file that is not
    /// there is an empty journal.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read.
    pub fn live(&self) -> io::Result<Vec<String>> {
        let text = match fs::read_to_string(&self.path) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => String::new(),
            other => other?,
        };
        let mut live = BTreeSet::new();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("+", lease)) => live.insert(lease.to_owned()),
                Some(("-", lease)) => live.remove(lease),
                _ => false,
            };
        }
        Ok(live.into_iter().collect())
    }

    /// Replaces the journal with one that holds exactly `live`: written beside it, synced
    /// and renamed over it.
    ///
    /// # Errors
    ///
    /// The new file cannot be written or moved into place.
    pub fn rewrite(&self, live: &[String]) -> io::Result<()> {
        let temp = self.path.with_extension("new");
        let mut file = File::create(&temp)?;
        for lease in live {
            writeln!(file, "+ {lease}")?;
        }
        file.sync_all()?;
        fs::rename(&temp, &self.path)
    }
}
