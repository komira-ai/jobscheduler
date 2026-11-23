//! The lease journal: which leases this agent may have lent an account for and not yet seen
//! retired.
//!
//! A lease is written (and synced to disk) before the pool lends it an account, and marked done
//! after the account was retired or after a failed start was undone. A process that dies,
//! stops or is rescued with runs going leaves them in the journal; the account pool's next open
//! retires their accounts, and the next process (or the rescue) lists each lease that has a run
//! as ended and drops the rest (`native/adopt.rs`). The file is append-only text, one
//! `+ <lease>`, `- <lease>` or `= <lease> <run>` per line; a line that is none of them (a torn
//! last line) is ignored, and the leases that are live are the added ones not yet removed.
//!
//! A `=` line is written once the supervisor said the lease's job started: `<run>` is a
//! [`RunRecord`] in JSON, what a later process needs to report the run as the process that
//! started it would (its labels, its scratch directory and home, its declared outputs, its
//! bundle, its account). Fields were only ever added, each with a default, so a record of an
//! earlier release reads, and an earlier release skips the fields it does not know.
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

/// What the journal keeps of a run whose job started, for a later process of the agent to
/// report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The pid of the run's job, as its supervisor reported it.
    pub pid: u32,
    /// When the run started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// A deadline of the run's own, in milliseconds after the start, that a process outside the
    /// agent held; `None` when none did, as for every run now (the lifecycle holds the wall
    /// limit).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// The run's labels, as the lifecycle gave them.
    pub labels: BTreeMap<String, String>,
    /// The run's scratch directory (its log files).
    pub scratch: PathBuf,
    /// The home of the run's account.
    pub home: PathBuf,
    /// The outputs the start declared.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    /// The bundle the run pinned, if any.
    #[serde(default)]
    pub bundle: Option<String>,
    /// The name of the account the run was lent; `None` in a record of an earlier release.
    #[serde(default)]
    pub account: Option<String>,
    /// The uid of that account; `None` in a record of an earlier release.
    #[serde(default)]
    pub uid: Option<u32>,
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

    /// Writes that `lease` may have an account, and syncs the file and its directory.
    ///
    /// # Errors
    ///
    /// The file cannot be written or synced.
    pub fn add(&self, lease: &str) -> io::Result<()> {
        self.append('+', lease)
    }

    /// Writes that `lease` has no account (any more).
    ///
    /// # Errors
    ///
    /// The file cannot be written or synced.
    pub fn done(&self, lease: &str) -> io::Result<()> {
        self.append('-', lease)
    }

    /// Writes the run record of `lease`, whose job started.
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
