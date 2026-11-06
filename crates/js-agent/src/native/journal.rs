//! The lease journal: which lease users this agent may have asked the helper for and not
//! yet seen deleted.
//!
//! A lease is written (and synced to disk) before `user-create` is sent, and marked done
//! after `user-delete` answered or after a failed start was undone. A process that dies,
//! stops or is rescued with runs going leaves them in the journal; the next process (or the
//! rescue) reads the journal and ends every lease in it that has no run. The file is
//! append-only text, one `+ <lease>`, `- <lease>` or `= <lease> <run>` per line; a line that
//! is none of them (a torn last line) is ignored, and the leases that are live are the added
//! ones not yet removed.
//!
//! A `=` line is written once the helper has started the lease's process: `<run>` is a
//! [`RunRecord`] in JSON, what a later process needs to find the run again (through the
//! helper's `watch`) and to finish it as the process that started it would (its labels, its
//! scratch directory and home, its declared outputs, its bundle). A live lease with such a
//! line is not ended by the next process: it is listed for adoption (`native/adopt.rs`).
//! The kind of line is new; a journal reader from before it skips it as it skips a torn line,
//! and so ends the run as it always did, and a journal written before it has no such line.
//!
//! An append and a [`retire`](Journal::retire) hold an exclusive `flock` on `<journal>.lock`
//! while they work, so a worker that is still alive (wedged, not dead) cannot add a lease
//! between the read and the rewrite of a retirement and lose it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use js_agent_wire::OutputSpec;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

/// What the journal keeps of a run the helper started, for a later process of the agent to
/// adopt it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The pid the helper reported for the run's process.
    pub pid: u32,
    /// When the run started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// The run's own deadline the helper was asked to hold, in milliseconds after the start;
    /// `None` when it holds none.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// The run's labels, as the lifecycle gave them.
    pub labels: BTreeMap<String, String>,
    /// The run's scratch directory (its log files).
    pub scratch: PathBuf,
    /// The home of the run's user.
    pub home: PathBuf,
    /// The outputs the start declared.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    /// The bundle the run pinned, if any.
    #[serde(default)]
    pub bundle: Option<String>,
}

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

    /// Writes what is needed to adopt the run of `lease`, which the helper started.
    ///
    /// # Errors
    ///
    /// The file cannot be written or synced.
    pub fn started(&self, lease: &str, run: &RunRecord) -> io::Result<()> {
        let json = serde_json::to_string(run)?;
        self.append('=', &format!("{lease} {json}"))
    }

    fn append(&self, sign: char, lease: &str) -> io::Result<()> {
        self.locked(|| {
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.path)?;
            writeln!(file, "{sign} {lease}")?;
            file.sync_all()?;
            let dir = self.path.parent().unwrap_or(Path::new("/"));
            File::open(dir)?.sync_all()
        })
    }

    /// Runs `work` holding the exclusive lock of the journal. The lock is released by
    /// unlocking before the file closes: an `flock` belongs to the open file description,
    /// which a child forked meanwhile would keep alive.
    fn locked<T>(&self, work: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let lock = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.path.with_extension("lock"))?;
        flock(&lock, FlockOperation::LockExclusive)?;
        let done = work();
        let _ = flock(&lock, FlockOperation::Unlock);
        done
    }

    /// The leases written and not marked done, in the order they sort. A file that is not
    /// there is an empty journal.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read.
    pub fn live(&self) -> io::Result<Vec<String>> {
        Ok(self.read()?.0.into_iter().collect())
    }

    /// The live leases that have a run record, each with its last one, in the order they
    /// sort. A record that does not parse (a torn line) is skipped.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read.
    pub fn runs(&self) -> io::Result<BTreeMap<String, RunRecord>> {
        let (live, mut runs) = self.read()?;
        runs.retain(|lease, _| live.contains(lease));
        Ok(runs)
    }

    /// The live leases, and the last run record of each lease that has one.
    fn read(&self) -> io::Result<(BTreeSet<String>, BTreeMap<String, RunRecord>)> {
        let text = match fs::read_to_string(&self.path) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => String::new(),
            other => other?,
        };
        let mut live = BTreeSet::new();
        let mut runs = BTreeMap::new();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("+", lease)) => {
                    live.insert(lease.to_owned());
                }
                Some(("-", lease)) => {
                    live.remove(lease);
                    runs.remove(lease);
                }
                Some(("=", rest)) => {
                    let parsed = rest.split_once(' ').and_then(|(lease, json)| {
                        let run = serde_json::from_str::<RunRecord>(json).ok()?;
                        Some((lease.to_owned(), run))
                    });
                    runs.extend(parsed);
                }
                _ => {}
            }
        }
        Ok((live, runs))
    }

    /// Removes `ended` from the journal: the leases it holds now, read under the lock
    /// (so one added since the caller last looked is kept), less `ended`, with the run
    /// record of each kept lease that has one, written beside it, synced and renamed over
    /// it.
    ///
    /// # Errors
    ///
    /// The journal cannot be read, or the new file cannot be written or moved into place.
    pub fn retire(&self, ended: &[String]) -> io::Result<()> {
        self.locked(|| {
            let temp = self.path.with_extension("new");
            let mut file = File::create(&temp)?;
            let (live, runs) = self.read()?;
            for lease in live.iter().filter(|lease| !ended.contains(lease)) {
                writeln!(file, "+ {lease}")?;
                if let Some(run) = runs.get(lease) {
                    writeln!(file, "= {lease} {}", serde_json::to_string(run)?)?;
                }
            }
            file.sync_all()?;
            fs::rename(&temp, &self.path)
        })
    }
}
