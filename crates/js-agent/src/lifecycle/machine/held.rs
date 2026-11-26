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
//!   first, written whole (as a result's file is), and **`retired.log`**, the ids added
//!   since, one JSON object a line, appended and synced. An id added is appended to the log
//!   while the log, with it, stays within a sixteenth of `retired.json`; otherwise the list
//!   is written whole and the log removed. A list of a few ids is so always written whole,
//!   and one of 4096 (some 370 KB with a node's ids, a line some 90 bytes) costs a line of
//!   the log for each id and a whole write for each 250 or so, where it cost a whole write,
//!   synced, for each. The log is read after the list, its ids added in order; its last
//!   line, when no newline ends it, is a write that did not finish and is left out, and the
//!   next id added writes the list whole. An agent of an earlier release reads
//!   `retired.json` alone, the list as it was at its last whole write.
//!
//! [`Lifecycle::adopt`] reads them all. A result whose id is a taken-over job's is set on that
//! job, which then makes no second result when its container is seen to have exited; any
//! other becomes a job that holds nothing and is listed until the result is acknowledged.
//! An unfinished write (`<id>.json.tmp`) is removed without a finding. A file that cannot
//! be read or understood is removed and reported as a `held-record-unreadable` finding, and
//! the agent starts all the same; a retired list that cannot be read starts empty, and a log
//! that cannot be read, or a line of it, adds nothing and is removed by the next whole
//! write. A file
//! that cannot be written is a `held-record-failed` finding (once, until a call writes
//! everything again): the result is still held in memory and the write is tried again at
//! every call. A result's file is removed only after the retired list that names its id is
//! on disk.
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

use super::Lifecycle;
use super::job::{Job, Keyed, Retired};
use super::listing::entry_len;
use crate::lifecycle::taint::StateRoot;
use crate::lifecycle::{Event, Finding, code};

/// The directory of the held results, in the state root.
const HELD_DIR: &str = "held";

/// The end of the name of a held result's file.
const RESULT_SUFFIX: &str = ".json";

/// The end of the name of a file that is being written, before it is renamed.
pub(super) const TEMP_SUFFIX: &str = ".tmp";

/// The file of the retired list, in the state root.
const RETIRED_FILE: &str = "retired.json";

/// The most bytes of a held result's file that are read. A result is a few kilobytes: its
/// tails are at most 8 KiB a stream, and six times that once escaped.
const RESULT_MAX: u64 = 1 << 20;

/// The log of the ids retired since the retired list was written whole, in the state root.
const RETIRED_LOG: &str = "retired.log";

/// The most bytes of the retired list that are read: at most 4096 pairs of two ids. The log
/// is held to a sixteenth of the list, so the same bound holds it.
const RETIRED_MAX: u64 = 4 << 20;

/// The log holds at most the list's length divided by this, in bytes: a whole write then
/// comes once the log has grown by a sixteenth of the list.
const LOG_SHARE: u64 = 16;

/// What a result's log tail looked like when it was written: the lengths of the two
/// streams, none when it has no tail. A tail is only ever cut, so a cut changes the mark.
type Mark = Option<(usize, usize)>;

/// A held result as its file says it: with its `input_failure` when it has one, which an
/// agent of an earlier release, which does not read it, ignores.
#[derive(Serialize)]
struct Written<'a> {
    keyed: Keyed,
    result: &'a RunResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_failure: Option<&'a str>,
}

/// A held result as it is read back; fields it does not know are ignored, and a file an
/// earlier release wrote has no `input_failure`.
#[derive(Deserialize)]
pub(super) struct Stored {
    keyed: Keyed,
    result: RunResult,
    #[serde(default)]
    input_failure: Option<String>,
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
    /// The count of [`Retired::added`] the list and its log hold.
    retired_added: u64,
    /// The length of `retired.json` as it was last written or read; 0 when there is none.
    list_bytes: u64,
    /// The length of `retired.log`; 0 when there is none.
    log_bytes: u64,
    /// Whether the log may be appended to: not when it ends in a write that did not finish,
    /// or holds a line that is no id, until the next whole write removes it.
    log_whole: bool,
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
pub(super) fn read_capped(path: &Path, max: u64) -> io::Result<String> {
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
pub(super) fn write_whole(dir: &Path, name: &str, body: &[u8]) -> io::Result<()> {
    let temp = dir.join(format!("{name}{TEMP_SUFFIX}"));
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

/// The result, keying and input failure in the file `name` at `path`.
fn read_result(path: &Path, name: &str) -> Result<Stored, String> {
    let text = read_capped(path, RESULT_MAX).map_err(|why| why.to_string())?;
    let stored: Stored = serde_json::from_str(&text).map_err(|why| why.to_string())?;
    stored.result.validate().map_err(|why| why.to_string())?;
    match name == format!("{}{RESULT_SUFFIX}", stored.result.run_id) {
        true => Ok(stored),
        false => Err("the file is named for another run".to_owned()),
    }
}

/// Whether the next file is read, `weight` being the bytes its job's entry would add to a
/// heartbeat beside those counted in `used` (0 for a job the agent already holds): a file
/// is read while its entry fits `room`, and a file for a job the agent already holds is
/// always read.
fn admit(weight: usize, used: &mut usize, room: usize) -> bool {
    let read = weight == 0 || *used + weight <= room;
    *used += weight * usize::from(read);
    read
}

/// A finding about a file of the state root that could not be used.
fn unreadable(events: &mut Vec<Event>, what: &str, why: &str) {
    events.push(Event::Finding(Finding {
        run_id: None,
        code: code::HELD_UNREADABLE,
        message: format!("{what}: {why}"),
    }));
}

/// The lines of the retired list's log for `entries`: one JSON object each, as the list
/// holds it, ended by a newline.
/// (Two strings always serialise; a failure would leave the line empty, which a read leaves
/// out as no id.)
fn lines_of<'a>(entries: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Vec<u8> {
    let mut lines = Vec::new();
    for (job_id, allocation) in entries {
        let entry = RetiredId {
            job_id: job_id.to_owned(),
            allocation_id: allocation.map(str::to_owned),
        };
        lines.extend(serde_json::to_vec(&entry).unwrap_or_default());
        lines.push(b'\n');
    }
    lines
}

impl Records {
    /// The records of the state root `state`, none known to be on disk.
    pub(super) fn new(state: &StateRoot) -> Self {
        Self {
            root: state.path().to_owned(),
            written: BTreeMap::new(),
            retired_added: 0,
            list_bytes: 0,
            log_bytes: 0,
            log_whole: true,
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

    /// Reads the held results, as many of them that add a job as have entries that fit `room`
    /// bytes of a heartbeat (`listing`), and the retired list. A result whose id is in
    /// `taken` is a job already held: it is always read and does not count against `room`. A file that cannot be used, and every file past `room`, is
    /// removed and reported; an unfinished write (`*.tmp`) of a dead process is removed
    /// without a word.
    pub(super) fn load(
        &mut self,
        room: usize,
        taken: &BTreeSet<&str>,
        events: &mut Vec<Event>,
    ) -> (Vec<Stored>, Retired) {
        let mut found = Vec::new();
        match self.names() {
            Err(why) => unreadable(events, "the held results", &why.to_string()),
            Ok(names) => {
                let mut used = 0;
                for name in names {
                    let path = self.dir().join(&name);
                    if name.ends_with(TEMP_SUFFIX) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    let id = name.strip_suffix(RESULT_SUFFIX).unwrap_or(&name);
                    let weight = match taken.contains(id) {
                        true => 0,
                        false => entry_len(id),
                    };
                    let read = match admit(weight, &mut used, room) {
                        true => read_result(&path, &name),
                        false => Err("the agent can hold no more runs".to_owned()),
                    };
                    match read {
                        Ok(stored) => {
                            let mark = mark_of(&stored.result);
                            self.written.insert(stored.result.run_id.clone(), mark);
                            found.push(stored);
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

    /// Reads the retired list and then its log; empty when there is neither or they cannot
    /// be used.
    fn load_retired(&mut self, events: &mut Vec<Event>) -> Retired {
        let mut retired = Retired::default();
        let list = match read_capped(&self.root.join(RETIRED_FILE), RETIRED_MAX) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => None,
            read => Some(read.map_err(|why| why.to_string()).and_then(|text| {
                let list = serde_json::from_str::<RetiredList>(&text);
                list.map(|list| (list, text.len()))
                    .map_err(|why| why.to_string())
            })),
        };
        match list {
            None => {}
            Some(Ok((list, len))) => {
                self.list_bytes = u64::try_from(len).unwrap_or(u64::MAX);
                list.retired.iter().for_each(|entry| {
                    retired.insert_with(&entry.job_id, entry.allocation_id.as_deref());
                });
            }
            Some(Err(why)) => {
                let why = format!("{why}; it starts empty");
                unreadable(events, "the retired list", &why);
            }
        }
        self.load_log(&mut retired, events);
        self.retired_added = retired.added();
        retired
    }

    /// Adds the ids of the retired list's log to `retired`, in order. The last line, when no
    /// newline ends it, is left out without a word; a line that is no id is left out with a
    /// finding. Either way the log is appended to no more: the next id written writes the
    /// list whole.
    fn load_log(&mut self, retired: &mut Retired, events: &mut Vec<Event>) {
        let read = File::open(self.root.join(RETIRED_LOG)).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(RETIRED_MAX)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let bytes = match read {
            Err(why) if why.kind() == io::ErrorKind::NotFound => return,
            Err(why) => {
                self.log_whole = false;
                return unreadable(events, "the retired log", &why.to_string());
            }
            Ok(bytes) => bytes,
        };
        self.log_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        // Bytes, not text: a write cut inside a character spoils its own line alone.
        let ended = bytes.ends_with(b"\n");
        let lines: Vec<&[u8]> = bytes
            .strip_suffix(b"\n")
            .unwrap_or(&bytes)
            .split(|byte| *byte == b'\n')
            .filter(|_| !bytes.is_empty())
            .collect();
        let finished = match ended {
            true => lines.len(),
            false => lines.len().saturating_sub(1),
        };
        self.log_whole = ended || lines.is_empty();
        for (n, line) in lines.into_iter().take(finished).enumerate() {
            match serde_json::from_slice::<RetiredId>(line) {
                Ok(entry) => retired.insert_with(&entry.job_id, entry.allocation_id.as_deref()),
                Err(why) => {
                    self.log_whole = false;
                    let why = format!("line {}: {why}; it is left out", n + 1);
                    unreadable(events, "the retired log", &why);
                }
            }
        }
    }

    /// Writes the file of the result of `job`.
    fn write_result(&self, job: &Job, result: &RunResult) -> io::Result<()> {
        let dir = self.dir();
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let name = format!("{}{RESULT_SUFFIX}", result.run_id);
        let input_failure = job.input_failure.as_deref();
        let keyed = job.keyed;
        serde_json::to_vec(&Written {
            keyed,
            result,
            input_failure,
        })
        .map_err(io::Error::other)
        .and_then(|body| write_whole(&dir, &name, &body))
    }

    /// Writes the ids added to `retired` since the last write: appended to the log while the
    /// log, with them, stays within a sixteenth of the list on disk and may be appended to;
    /// otherwise the list whole, and then the log removed.
    fn write_retired(&mut self, retired: &Retired) -> io::Result<()> {
        let added = retired.added().saturating_sub(self.retired_added);
        let fresh = usize::try_from(added).unwrap_or(usize::MAX);
        let skip = retired.entries().count().checked_sub(fresh);
        let lines = skip.map(|skip| lines_of(retired.entries().skip(skip)));
        let grown = lines.as_ref().map(|lines| {
            let len = u64::try_from(lines.len()).unwrap_or(u64::MAX);
            self.log_bytes.saturating_add(len)
        });
        match (lines, grown) {
            (Some(lines), Some(grown))
                if self.log_whole && grown.saturating_mul(LOG_SHARE) <= self.list_bytes =>
            {
                self.append_log(&lines)
                    .inspect_err(|_| self.log_whole = false)?;
                self.log_bytes = grown;
                Ok(())
            }
            _ => self.write_list(retired),
        }
    }

    /// Appends `lines` to the retired list's log and syncs it (and the state root when the
    /// log is new).
    fn append_log(&self, lines: &[u8]) -> io::Result<()> {
        let mut log = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.root.join(RETIRED_LOG))?;
        log.write_all(lines)?;
        log.sync_data()?;
        match self.log_bytes {
            0 => sync_dir(&self.root),
            _ => Ok(()),
        }
    }

    /// Writes the retired list whole, then removes its log, whose ids it holds: a crash
    /// between the two leaves a log whose ids the list holds, read again at the next start;
    /// on a full list that adds back no more than the ids the list had dropped as the oldest.
    fn write_list(&mut self, retired: &Retired) -> io::Result<()> {
        let list = RetiredList {
            retired: retired
                .entries()
                .map(|(job_id, allocation)| RetiredId {
                    job_id: job_id.to_owned(),
                    allocation_id: allocation.map(str::to_owned),
                })
                .collect(),
        };
        let body = serde_json::to_vec(&list).map_err(io::Error::other)?;
        write_whole(&self.root, RETIRED_FILE, &body)?;
        self.list_bytes = u64::try_from(body.len()).unwrap_or(u64::MAX);
        let removed = match fs::remove_file(self.root.join(RETIRED_LOG)) {
            Ok(()) => true,
            Err(why) if why.kind() == io::ErrorKind::NotFound => false,
            Err(why) => return Err(why),
        };
        // The log is gone: a log made from now on is new, and its first append syncs the
        // state root, whether or not the sync of its removal below goes through.
        self.log_bytes = 0;
        self.log_whole = true;
        match removed {
            true => sync_dir(&self.root),
            false => Ok(()),
        }
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
                match self.write_result(job, result) {
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
        let listed = match self.retired_added != retired.added() {
            true => match self.write_retired(retired) {
                Ok(()) => {
                    self.retired_added = retired.added();
                    true
                }
                Err(why) => {
                    failed.push((None, format!("the retired list cannot be written: {why}")));
                    false
                }
            },
            false => true,
        };
        // A result file goes only once its id is on the retired list on disk: a crash
        // between the two must leave the id held or retired, never neither.
        let gone = gone.into_iter().filter(|_| listed);
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
        let room = self.listing_room();
        let taken: BTreeSet<&str> = self.jobs.iter().map(|job| job.run_id.as_str()).collect();
        let (results, retired) = self.records.load(room, &taken, events);
        self.retired = retired;
        for Stored {
            keyed,
            result,
            input_failure,
        } in results
        {
            let taken = self.jobs.iter().position(|job| job.run_id == result.run_id);
            match taken {
                Some(index) => {
                    self.jobs[index].result = Some(result);
                    self.jobs[index].input_failure = input_failure;
                }
                None => self.jobs.push(Job::held(keyed, result, input_failure)),
            }
        }
    }
}
