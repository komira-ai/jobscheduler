//! What survives a restart of the agent beside the containers: the results the server has
//! not acknowledged and the ids that must not start again, kept in the state root.
//!
//! - **`held/<id>.json`**, one file for each result the lifecycle holds, `<id>` the run id
//!   or the job id. The file holds the result and how the job is keyed (`run` or `job`).
//!   It is written whole (a temporary file, synced, renamed over, the directory synced)
//!   at the end of the [`Lifecycle::apply`] or [`Lifecycle::step`] that made the result,
//!   so before any heartbeat can carry it, and again when the result changes (its log tail
//!   is cut to fit a heartbeat). It is removed at the end of the call in which the
//!   server's acknowledgement dropped the result; an answer the agent does not know keeps
//!   it. The directory is `0700` and holds at most [`limits::MAX_HELD`] files, which the
//!   lifecycle's own bound on jobs already keeps it to.
//! - **`retired.json`**, the retired list as pairs of job id and allocation id (the
//!   allocation is absent when the agent was told to stop a job it did not hold), oldest
//!   first, rewritten whole whenever an id is added.
//!
//! [`Lifecycle::adopt`] reads both. A result whose id is a taken-over job's is set on that
//! job, which then makes no second result when its container is seen to have exited; any
//! other becomes a job that holds nothing and is listed until the result is acknowledged.
//! A file that cannot be read or understood is removed and reported as a
//! `held-record-unreadable` finding, and the agent starts all the same; a retired list that
//! cannot be read starts empty. A file that cannot be written is a `held-record-failed`
//! finding (once, until a call writes everything again): the result is still held in
//! memory and the write is tried again at every call.
//!
//! [`Lifecycle::apply`]: super::Lifecycle::apply
//! [`Lifecycle::step`]: super::Lifecycle::step
//! [`Lifecycle::adopt`]: super::Lifecycle::adopt
//! [`limits::MAX_HELD`]: js_agent_wire::limits::MAX_HELD

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use js_agent_wire::RunResult;
use serde::{Deserialize, Serialize};

use super::job::{Job, Keyed, Retired};
use super::{Lifecycle, MAX_JOBS};
use crate::lifecycle::taint::StateRoot;
use crate::lifecycle::{Event, Finding, code};

/// The directory of the held results, in the state root.
const HELD_DIR: &str = "held";

/// The end of the name of a held result's file.
const RESULT_SUFFIX: &str = ".json";

/// The file of the retired list, in the state root.
const RETIRED_FILE: &str = "retired.json";

/// The most bytes of a held result's file that are read. A result is a few kilobytes: its
/// tails are at most 8 KiB a stream, and six times that once escaped.
const RESULT_MAX: u64 = 1 << 20;

/// The most bytes of the retired list that are read: at most 4096 pairs of two ids.
const RETIRED_MAX: u64 = 4 << 20;

/// What a result's log tail looked like when it was written: the lengths of the two
/// streams, none when it has no tail. A tail is only ever cut, so a cut changes the mark.
type Mark = Option<(usize, usize)>;

/// A held result as its file says it.
#[derive(Serialize)]
struct Written<'a> {
    keyed: Keyed,
    result: &'a RunResult,
}

/// A held result as it is read back; fields it does not know are ignored.
#[derive(Deserialize)]
struct Stored {
    keyed: Keyed,
    result: RunResult,
}

/// The retired list as its file says it.
#[derive(Serialize, Deserialize)]
struct RetiredList {
    retired: Vec<RetiredId>,
}

/// One retired job id with its allocation, when it is known.
#[derive(Serialize, Deserialize)]
struct RetiredId {
    job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allocation_id: Option<String>,
}

/// What did not get written in a call: the run it is about, when it is about one, and why.
type Failures = Vec<(Option<String>, String)>;

/// The files of the held results and the retired list, and what is known to be on disk.
pub(super) struct Records {
    root: PathBuf,
    /// The results with a file, by id, with the tail each was written with.
    written: BTreeMap<String, Mark>,
    /// The count of [`Retired::added`] the file holds.
    retired_added: u64,
    /// A failure to write was reported and no call has written everything since.
    told: bool,
}

/// How a result's log tail looks, for [`Mark`].
fn mark_of(result: &RunResult) -> Mark {
    let lengths = |tail: &js_agent_wire::LogTail| (tail.stdout.len(), tail.stderr.len());
    result.log_tail.as_ref().map(lengths)
}

/// The text of the file at `path`, at most `max` bytes of it: a longer file is cut, and
/// what is left of a file that is longer than any record is no record.
fn read_capped(path: &Path, max: u64) -> io::Result<String> {
    File::open(path).and_then(|file| {
        let mut text = String::new();
        file.take(max).read_to_string(&mut text).map(|_| text)
    })
}

/// Makes what was created or removed in `dir` durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir).and_then(|dir| dir.sync_all())
}

/// Writes `body` as the file `name` of `dir`: to a temporary file, synced, renamed over the
/// file, and the directory synced. The file is the old one or the new one, never a part.
fn write_whole(dir: &Path, name: &str, body: &[u8]) -> io::Result<()> {
    let temp = dir.join(format!("{name}.tmp"));
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(body).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temp, dir.join(name)))
        .and_then(|()| sync_dir(dir))
}

/// The result and keying in the file `name` at `path`.
fn read_result(path: &Path, name: &str) -> Result<(Keyed, RunResult), String> {
    let text = read_capped(path, RESULT_MAX).map_err(|why| why.to_string())?;
    let stored: Stored = serde_json::from_str(&text).map_err(|why| why.to_string())?;
    stored.result.validate().map_err(|why| why.to_string())?;
    match name == format!("{}{RESULT_SUFFIX}", stored.result.run_id) {
        true => Ok((stored.keyed, stored.result)),
        false => Err("the file is named for another run".to_owned()),
    }
}

/// A finding about a file of the state root that could not be used.
fn unreadable(events: &mut Vec<Event>, what: &str, why: &str) {
    events.push(Event::Finding(Finding {
        run_id: None,
        code: code::HELD_UNREADABLE,
        message: format!("{what}: {why}"),
    }));
}

impl Records {
    /// The records of the state root `state`, none known to be on disk.
    pub(super) fn new(state: &StateRoot) -> Self {
        Self {
            root: state.path().to_owned(),
            written: BTreeMap::new(),
            retired_added: 0,
            told: false,
        }
    }

    /// The directory of the held results.
    fn dir(&self) -> PathBuf {
        self.root.join(HELD_DIR)
    }

    /// The file of the held result of `id`.
    fn file_of(&self, id: &str) -> PathBuf {
        self.dir().join(format!("{id}{RESULT_SUFFIX}"))
    }

    /// The names in the directory of held results, sorted; none when it is not there.
    fn names(&self) -> io::Result<Vec<String>> {
        match fs::read_dir(self.dir()) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(why) => Err(why),
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                Ok(names)
            }
        }
    }

    /// Reads the held results, at most `room` of them, and the retired list. A file that
    /// cannot be used, and every file past `room`, is removed and reported.
    pub(super) fn load(
        &mut self,
        room: usize,
        events: &mut Vec<Event>,
    ) -> (Vec<(Keyed, RunResult)>, Retired) {
        let mut found = Vec::new();
        match self.names() {
            Err(why) => unreadable(events, "the held results", &why.to_string()),
            Ok(names) => {
                for (index, name) in names.into_iter().enumerate() {
                    let path = self.dir().join(&name);
                    let read = match index < room {
                        true => read_result(&path, &name),
                        false => Err("the agent can hold no more runs".to_owned()),
                    };
                    match read {
                        Ok((keyed, result)) => {
                            self.written
                                .insert(result.run_id.clone(), mark_of(&result));
                            found.push((keyed, result));
                        }
                        Err(why) => {
                            let _ = fs::remove_file(&path);
                            unreadable(events, &format!("the held result {name}"), &why);
                        }
                    }
                }
            }
        }
        (found, self.load_retired(events))
    }

    /// Reads the retired list; empty when there is none or it cannot be used.
    fn load_retired(&mut self, events: &mut Vec<Event>) -> Retired {
        let mut retired = Retired::default();
        let list = match read_capped(&self.root.join(RETIRED_FILE), RETIRED_MAX) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => return retired,
            read => read.map_err(|why| why.to_string()).and_then(|text| {
                serde_json::from_str::<RetiredList>(&text).map_err(|why| why.to_string())
            }),
        };
        match list {
            Ok(list) => list.retired.iter().for_each(|entry| {
                retired.insert_with(&entry.job_id, entry.allocation_id.as_deref());
            }),
            Err(why) => {
                let why = format!("{why}; it starts empty");
                unreadable(events, "the retired list", &why);
            }
        }
        self.retired_added = retired.added();
        retired
    }

    /// Writes the file of a result.
    fn write_result(&self, keyed: Keyed, result: &RunResult) -> io::Result<()> {
        let dir = self.dir();
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let name = format!("{}{RESULT_SUFFIX}", result.run_id);
        serde_json::to_vec(&Written { keyed, result })
            .map_err(io::Error::other)
            .and_then(|body| write_whole(&dir, &name, &body))
    }

    /// Writes the retired list.
    fn write_retired(&self, retired: &Retired) -> io::Result<()> {
        let list = RetiredList {
            retired: retired
                .entries()
                .map(|(job_id, allocation)| RetiredId {
                    job_id: job_id.to_owned(),
                    allocation_id: allocation.map(str::to_owned),
                })
                .collect(),
        };
        serde_json::to_vec(&list)
            .map_err(io::Error::other)
            .and_then(|body| write_whole(&self.root, RETIRED_FILE, &body))
    }

    /// Removes the file of a result; a file that is not there is removed already.
    fn remove_result(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.file_of(id)) {
            Ok(()) => sync_dir(&self.dir()),
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(why) => Err(why),
        }
    }

    /// Brings the files to what `jobs` and `retired` hold: a file for every result that has
    /// none or changed, none for a result that is gone, the retired list when it grew.
    /// Returns what could not be done; it is tried again by the next call.
    pub(super) fn sync(&mut self, jobs: &[Job], retired: &Retired) -> Failures {
        let mut failed = Vec::new();
        for job in jobs {
            let Some(result) = &job.result else {
                continue;
            };
            let mark = mark_of(result);
            if self.written.get(&job.run_id) != Some(&mark) {
                match self.write_result(job.keyed, result) {
                    Ok(()) => {
                        self.written.insert(job.run_id.clone(), mark);
                    }
                    Err(why) => failed.push((
                        Some(job.run_id.clone()),
                        format!("the result cannot be written: {why}"),
                    )),
                }
            }
        }
        let held: BTreeSet<&str> = jobs
            .iter()
            .filter(|job| job.result.is_some())
            .map(|job| job.run_id.as_str())
            .collect();
        let gone: Vec<String> = self
            .written
            .keys()
            .filter(|id| !held.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            match self.remove_result(&id) {
                Ok(()) => {
                    self.written.remove(&id);
                }
                Err(why) => {
                    failed.push((Some(id), format!("the result cannot be removed: {why}")));
                }
            }
        }
        if self.retired_added != retired.added() {
            match self.write_retired(retired) {
                Ok(()) => self.retired_added = retired.added(),
                Err(why) => {
                    failed.push((None, format!("the retired list cannot be written: {why}")));
                }
            }
        }
        failed
    }

    /// Reports `failed` as findings, unless an earlier call reported failures and none has
    /// written everything since.
    pub(super) fn report(&mut self, failed: Failures, events: &mut Vec<Event>) {
        let any = !failed.is_empty();
        if any && !self.told {
            events.extend(failed.into_iter().map(|(run_id, message)| {
                Event::Finding(Finding {
                    run_id,
                    code: code::HELD_FAILED,
                    message,
                })
            }));
        }
        self.told = any;
    }
}

impl Lifecycle {
    /// Writes down the results and retired ids the call just made, and takes back the
    /// files of the results it dropped. Called at the end of every call that changes them,
    /// so that a result is on disk before a heartbeat can carry it.
    pub(super) fn persist_held(&mut self, events: &mut Vec<Event>) {
        let failed = self.records.sync(&self.jobs, &self.retired);
        self.records.report(failed, events);
    }

    /// Reads the held results and the retired list back: a result of a job taken over is
    /// set on that job, any other makes a job that holds nothing and owes the result.
    pub(super) fn restore_held(&mut self, events: &mut Vec<Event>) {
        let room = MAX_JOBS.saturating_sub(self.jobs.len());
        let (results, retired) = self.records.load(room, events);
        self.retired = retired;
        for (keyed, result) in results {
            let taken = self.jobs.iter().position(|job| job.run_id == result.run_id);
            match taken {
                Some(index) => self.jobs[index].result = Some(result),
                None => self.jobs.push(Job::held(keyed, result)),
            }
        }
    }
}
