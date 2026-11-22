//! The core of a per-run supervisor: a process of its own that runs one job to its end and
//! records how it ended, so that the job outlives the agent that started it. Its arms start
//! the supervisor and run the job: on Linux `js-contain`'s `oci::supervise` (a supervisor that
//! runs `crun run`). This crate holds what every arm shares, and it starts no process: the
//! caller starts the supervisor and hands the child over ([`Supervised::handshake`]).
//!
//! **The handshake:** the supervisor makes its files, then writes a ready word ([`READY`])
//! to its standard output. [`Supervised::handshake`] returns once it has read the word; a
//! supervisor that exits without it, says something else, or is silent for [`READY_WAIT`],
//! is killed and reaped, and the start fails.
//!
//! **The exit record:** when the run is over, the supervisor writes `exit.json` in the lease
//! directory whole ([`write_record`]: a temporary file, synced, renamed, the directory synced),
//! and exits. The record holds how the run ended, what each stream wrote (the tail, the byte
//! count, whether the file lacks bytes) and the wall clock time it was written at. Its form is
//! version 1, JSON:
//!
//! ```text
//! {"version":1,"end":{"exited":<i32>}|{"failed":"<text>"},"finished_ms":<u64>,
//!  "stdout":{"tail":"<hex>","total":<u64>,"file_cut":<bool>},"stderr":{...}}
//! ```
//!
//! A reader ignores fields it does not know and refuses another version, so later forms
//! add fields. The record is read through [`read_exit_record`], which opens it without
//! following a link and reads at most 16.1 MiB of a regular file. A stream's tail
//! of more than 4 MiB is recorded as its last 4 MiB.
//!
//! **The process identity:** [`SupervisorId`], a pid and a start time, which name one process
//! for as long as the host runs. On Linux they are read from `/proc`; on macOS from
//! `proc_pidinfo(PROC_PIDTBSDINFO)` and `sysctl(KERN_PROCARGS2)`. The lease directory also
//! names its supervisor in a file of its own ([`supervisor_path`]).
//!
//! **The handle:** [`Supervised`] reads the record. A run is over when the record is there;
//! a supervisor that is gone ([`SupervisorId::alive`]) and wrote none ended without one,
//! which is a failure of the run. It signals nothing by itself: [`Supervised::force`] ends
//! the supervisor's process group (unless the supervisor has written its record, and so only
//! exits), and the job itself is ended by the arm, as for every run. A later process adopts a
//! run by its supervisor's identity ([`Supervised::adopt`]).
//!
//! **The signals:** the supervise mode blocks `SIGHUP`, `SIGINT` and `SIGTERM` before it makes
//! any thread ([`block_stop_signals`]).

use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use serde_json::{Value, json};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as sys;

use sys::stat_of;

/// How one `crun run` ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// `crun run` exited with this status: the container's exit code (`128 + n` if it
    /// was killed by signal `n`), or `crun`'s own failure, told apart by the log.
    Exited(i32),
    /// `crun` could not be run, or its task did not finish.
    Failed(String),
}

/// What one stream of a job wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamTail {
    /// The last bytes: at most the capture's tail limit of them (`js-contain`'s
    /// `LogLimits::tail`), as written (not necessarily UTF-8, and cut anywhere).
    pub bytes: Vec<u8>,
    /// How many bytes the job wrote in all.
    pub total: u64,
    /// Whether the stream's file lacks bytes the job wrote: past the cap, or after the
    /// file could not be written.
    pub file_cut: bool,
}

/// What a job wrote to its two streams, apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogTail {
    /// The standard output.
    pub stdout: StreamTail,
    /// The standard error.
    pub stderr: StreamTail,
}

/// The word a supervisor writes to its standard output once it has made its files.
pub const READY: &[u8] = b"supervising\n";

/// How often [`Supervised::wait`] looks for the end of the run.
pub const SUPERVISED_POLL: Duration = Duration::from_millis(50);

/// How a run ends whose supervisor is gone and left no exit record.
const GONE: &str = "the supervisor ended without an exit record";

/// The most bytes of each stream's tail an exit record holds: the writer keeps the last
/// bytes of a longer tail, so that a record is always one [`read_exit_record`] accepts.
pub(crate) const MAX_TAIL_RECORDED: usize = 4 * 1024 * 1024;

/// The most bytes of an exit record that are read: two streams of [`MAX_TAIL_RECORDED`]
/// bytes in two hexadecimal digits each, and 64 KiB for the rest.
const MAX_RECORD: u64 = (2 * 2 * MAX_TAIL_RECORDED + 64 * 1024) as u64;

/// How long a start waits for the ready word before it gives up on the supervisor, kills
/// its process group and fails the start.
pub const READY_WAIT: Duration = Duration::from_secs(30);

/// The exit record of the lease directory `lease`: `<lease>/exit.json`.
pub fn exit_record_path(lease: &Path) -> PathBuf {
    lease.join("exit.json")
}

/// The file the record is written to before it is renamed into place.
fn staged_record_path(lease: &Path) -> PathBuf {
    lease.join(".exit.json.tmp")
}

/// The identity of a supervisor process: its pid and its start time, which together name one
/// process for as long as the host runs. The start time is in the unit of the host's process
/// table: on Linux clock ticks since boot (field 22 of `/proc/<pid>/stat`), on macOS
/// microseconds since the Unix epoch (`pbi_start_tvsec` and `pbi_start_tvusec` of
/// `proc_bsdinfo`). It is only compared with a start time read on the same host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorId {
    /// The process id.
    pub pid: u32,
    /// The start time of the process, in the unit of the host's process table.
    pub start: u64,
}

impl SupervisorId {
    /// The identity of the process `pid` now, `None` when the process table has no such
    /// process.
    pub(crate) fn of(pid: u32) -> Option<Self> {
        stat_of(pid).map(|(_, start)| Self { pid, start })
    }

    /// Whether the process it names still runs (the process table has the same start
    /// time for its pid, and it is not a zombie).
    #[must_use]
    pub fn alive(&self) -> bool {
        names(stat_of(self.pid), self.start)
    }

    /// Whether the process it names runs as the user `uid` (on Linux the owner of its
    /// `/proc` entry, on macOS its effective user).
    #[must_use]
    pub fn runs_as(&self, uid: u32) -> bool {
        sys::runs_as(self.pid, uid)
    }

    /// Whether the process it names is the supervisor of the lease directory `lease`: its
    /// command line, which the arm that started it sets, names `lease` as the lease
    /// (`<daemon> <mode> <crun> <lease> ...`). Another lease's supervisor names its own.
    #[must_use]
    pub fn supervises(&self, lease: &Path) -> bool {
        sys::supervises(self.pid, lease)
    }
}

/// The file in the lease directory `lease` that names the supervisor started for it.
#[must_use]
pub fn supervisor_path(lease: &Path) -> PathBuf {
    lease.join("supervisor")
}

/// The supervisor the lease directory `lease` names: the pid and start time
/// [`note_supervisor`] wrote there, for a teardown that has no other record of the
/// lease. `None` when there is no such file or it names none. It is opened without following
/// a link or waiting on a pipe. The directory is the job user's: a caller signals the process
/// only when it runs as the job user ([`SupervisorId::runs_as`]) and supervises this lease
/// ([`SupervisorId::supervises`]).
#[must_use]
pub fn recorded_supervisor(lease: &Path) -> Option<SupervisorId> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits().cast_signed())
        .open(supervisor_path(lease))
        .ok()?;
    let mut text = String::new();
    file.take(64).read_to_string(&mut text).ok()?;
    let mut words = text.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let start = words.next()?.parse().ok()?;
    Some(SupervisorId { pid, start })
}

/// Writes `id` to the supervisor file of `lease`, as root into a directory the job user
/// writes: what is in its place is removed (a link is removed, not followed) and the file is
/// made new, so a link put there cannot send the write elsewhere. Best effort: the ledger's
/// record is the agent's own, and this file only serves a teardown that has lost that.
pub fn note_supervisor(lease: &Path, id: SupervisorId) {
    let path = supervisor_path(lease);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .and_then(|mut file| file.write_all(format!("{} {}\n", id.pid, id.start).as_bytes()));
}

/// Whether a process whose process table entry says `found` (its state letter and start
/// time) is the live process that started at `start`.
fn names(found: Option<(char, u64)>, start: u64) -> bool {
    found.is_some_and(|(state, at)| at == start && state != 'Z')
}

/// Whether the command line `args` (its words, in order) names `lease` as the lease: its
/// fourth word, where every arm puts it (`<daemon> <mode> <crun> <lease> ...` on Linux).
fn names_lease<'a>(mut args: impl Iterator<Item = &'a [u8]>, lease: &Path) -> bool {
    args.nth(3) == Some(lease.as_os_str().as_bytes())
}

/// The state (field 3) and the start time (field 22) of a `/proc/<pid>/stat` line. The
/// name (field 2) is in parentheses and may hold anything, so the fields are counted from
/// the last closing parenthesis.
#[cfg(any(test, target_os = "linux"))]
fn parse_stat(text: &str) -> Option<(char, u64)> {
    let (_, after) = text.rsplit_once(')')?;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

/// The state letter `ps` shows for the status `pbi_status` of a macOS `proc_bsdinfo`
/// (`SIDL` 1, `SRUN` 2, `SSLEEP` 3, `SSTOP` 4 and `SZOMB` 5 of `<sys/proc.h>`), so that
/// [`names`] reads both systems alike: `Z` is a zombie, and `?` a status macOS does not
/// define.
#[cfg(any(test, target_os = "macos"))]
fn state_letter(status: u32) -> char {
    match status {
        1 => 'I',
        2 => 'R',
        3 => 'S',
        4 => 'T',
        5 => 'Z',
        _ => '?',
    }
}

/// The words of a command line in the area `sysctl(KERN_PROCARGS2)` gives on macOS: the
/// argument count (a native-endian `i32`), the executable's path, NUL bytes up to the first
/// argument, then that many NUL-terminated arguments (the environment, after them, is not
/// read). `None` when the area is too short to hold the count and the path, or the count
/// is negative; an area cut short yields the words it holds. A first argument that is empty
/// cannot be told from the NUL bytes before it, so it is not a word of the result.
#[cfg(any(test, target_os = "macos"))]
fn arguments(area: &[u8]) -> Option<impl Iterator<Item = &[u8]>> {
    let (count, rest) = area.split_first_chunk::<4>()?;
    let count = usize::try_from(i32::from_ne_bytes(*count)).ok()?;
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path_end..];
    let first = rest
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(rest.len());
    Some(rest[first..].split(|byte| *byte == 0).take(count))
}

/// `SIGKILL`s the process group of the supervisor `id` (the supervisor and what it runs)
/// when `id.alive()` and the lease directory `lease` holds no exit record; nothing otherwise.
/// A supervisor that wrote its record has finished its run and only exits after it, so it is
/// left to exit: a kill then would cut its exit short (in a coverage build, the profile it
/// writes as it exits, which then cannot be read). A record that cannot be read does not
/// count. Never signals a pid whose start time differs (a reused pid).
pub fn end_supervisor(id: SupervisorId, lease: &Path) {
    if matches!(read_exit_record(lease), Ok(Some(_))) {
        return;
    }
    // A pid that is not a positive `i32` names no process: `alive` is false for it too.
    let group = i32::try_from(id.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    if let (true, Some(group)) = (id.alive(), group) {
        // The process may have ended since `alive` looked: there is then nothing to kill.
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

/// How a supervised container ended, as its supervisor wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitRecord {
    /// How `crun run` ended: the container's exit status, or why `crun` could not be run.
    pub end: RunEnd,
    /// What the container wrote, as the supervisor's capture ended with it.
    pub tail: LogTail,
    /// The wall clock, in milliseconds since the Unix epoch, when the supervisor wrote it.
    pub finished_ms: u64,
}

/// The record as the JSON of its form version 1.
/// Each stream's tail is cut to its last `max_tail` bytes.
fn encode(record: &ExitRecord, max_tail: usize) -> String {
    let end = match &record.end {
        RunEnd::Exited(code) => json!({ "exited": code }),
        RunEnd::Failed(why) => json!({ "failed": why }),
    };
    json!({
        "version": 1,
        "end": end,
        "finished_ms": record.finished_ms,
        "stdout": encode_stream(&record.tail.stdout, max_tail),
        "stderr": encode_stream(&record.tail.stderr, max_tail),
    })
    .to_string()
}

fn encode_stream(stream: &StreamTail, max_tail: usize) -> Value {
    let kept = stream.bytes.len().saturating_sub(max_tail);
    json!({
        "tail": hex(&stream.bytes[kept..]),
        "total": stream.total,
        "file_cut": stream.file_cut,
    })
}

/// The bytes as lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|&b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 15)]])
        .map(char::from)
        .collect()
}

/// The bytes of lowercase hexadecimal `text`.
fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let digits: Vec<u8> = text
        .bytes()
        .map(nibble)
        .collect::<Option<_>>()
        .ok_or("the tail is not lowercase hexadecimal".to_owned())?;
    if !digits.len().is_multiple_of(2) {
        return Err("the tail has an odd number of digits".to_owned());
    }
    Ok(digits
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect())
}

/// The member `name` of the object `value`.
fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    value.get(name).ok_or(format!("{name} is missing"))
}

/// The number in the member `name` of `value`.
fn number(value: &Value, name: &str) -> Result<u64, String> {
    field(value, name)?
        .as_u64()
        .ok_or(format!("{name} is not a number"))
}

/// One stream of a record: the member `name` of `value`.
fn decode_stream(value: &Value, name: &str) -> Result<StreamTail, String> {
    let stream = field(value, name)?;
    let tail = field(stream, "tail")?
        .as_str()
        .ok_or(format!("{name}.tail is not text"))?;
    Ok(StreamTail {
        bytes: unhex(tail)?,
        total: number(stream, "total")?,
        file_cut: field(stream, "file_cut")?
            .as_bool()
            .ok_or(format!("{name}.file_cut is not true or false"))?,
    })
}

/// The record in `bytes`, of form version 1; members it does not know are ignored.
fn decode(bytes: &[u8]) -> Result<ExitRecord, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
    let version = value.get("version").and_then(Value::as_u64);
    if version != Some(1) {
        return Err(format!("the form version is {version:?}, not 1"));
    }
    let end = field(&value, "end")?;
    let end = match (end.get("exited"), end.get("failed")) {
        (Some(code), _) => RunEnd::Exited(
            code.as_i64()
                .and_then(|code| i32::try_from(code).ok())
                .ok_or("end.exited is not an exit status".to_owned())?,
        ),
        (None, Some(why)) => RunEnd::Failed(
            why.as_str()
                .ok_or("end.failed is not text".to_owned())?
                .to_owned(),
        ),
        (None, None) => return Err("end is neither exited nor failed".to_owned()),
    };
    Ok(ExitRecord {
        end,
        tail: LogTail {
            stdout: decode_stream(&value, "stdout")?,
            stderr: decode_stream(&value, "stderr")?,
        },
        finished_ms: number(&value, "finished_ms")?,
    })
}

/// The bytes of the record file of `lease`, `None` when there is none.
fn load(lease: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let opened = std::fs::OpenOptions::new()
        .read(true)
        // A link is refused, and a pipe is opened without waiting for a writer.
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        )
        .open(exit_record_path(lease));
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(std::io::Error::other("longer than an exit record can be"));
    }
    Ok(Some(bytes))
}

/// Reads the exit record of `lease`: `Ok(None)` when there is none yet; `Err(text)` when the
/// file is a link, not a regular file, longer than an exit record can be, or not a record.
/// Opened without following a link.
///
/// # Errors
/// The text says which, and names the file.
pub fn read_exit_record(lease: &Path) -> Result<Option<ExitRecord>, String> {
    let path = exit_record_path(lease);
    let loaded = load(lease).map_err(|e| format!("{}: {e}", path.display()))?;
    match loaded {
        None => Ok(None),
        Some(bytes) => decode(&bytes)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
    }
}

/// Writes the exit record of `lease` whole: a temporary file in the directory (mode 0600),
/// synced, renamed over `exit.json`, and the directory synced.
///
/// # Errors
/// A step of the write failed: the temporary file could not be made, written or synced,
/// renamed into place, or the directory synced.
pub fn write_record(lease: &Path, record: &ExitRecord) -> std::io::Result<()> {
    let staged = staged_record_path(lease);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged)?;
    file.write_all(encode(record, MAX_TAIL_RECORDED).as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&staged, exit_record_path(lease))?;
    std::fs::File::open(lease)?.sync_all()
}

/// A container run by a supervisor process: the agent's handle on it. Dropping it does
/// nothing to the supervisor or the container (they outlive the handle, and the agent).
#[derive(Debug)]
pub struct Supervised {
    id: SupervisorId,
    lease: PathBuf,
}

impl Supervised {
    /// The handle of the supervisor `child`, which the caller has just started with its
    /// standard output the write end of the pipe whose read end is `from_child` (the caller
    /// keeping no copy of the write end, so that the child's exit ends the read) and in a
    /// process group of its own, for the run in the lease directory `lease`. Returns once
    /// the child has written `ready` to its stdout. A child that does not say it within
    /// `limit` is killed (its process group) and reaped. A thread waits for the child, so it
    /// never stays a zombie.
    ///
    /// # Errors
    /// The child's start time cannot be read, its answer is not `ready`, or it gave none
    /// within `limit`.
    pub fn handshake(
        mut child: Child,
        from_child: impl std::io::Read + Send + 'static,
        lease: PathBuf,
        ready: &[u8],
        limit: Duration,
    ) -> Result<Self, HandshakeError> {
        match answer(child.id(), from_child, ready, limit) {
            Ok(id) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(Self { id, lease })
            }
            Err(e) => {
                give_up(child);
                Err(e)
            }
        }
    }

    /// The handle of a run an earlier process started under the supervisor `id`, whose
    /// lease directory is `lease`. Starts nothing.
    #[must_use]
    pub fn adopt(lease: &Path, id: SupervisorId) -> Self {
        Self {
            id,
            lease: lease.to_owned(),
        }
    }

    /// The supervisor process.
    #[must_use]
    pub fn id(&self) -> SupervisorId {
        self.id
    }

    /// The lease directory whose exit record this handle reads.
    #[must_use]
    pub fn lease(&self) -> &Path {
        &self.lease
    }

    /// How the run ended, or `None` while it runs (without waiting): the exit record's
    /// end; or, when the supervisor is gone and wrote no record,
    /// `RunEnd::Failed("the supervisor ended without an exit record")`; a record that
    /// cannot be read is `RunEnd::Failed("the exit record cannot be read: <why>")`.
    #[must_use]
    pub fn try_end(&self) -> Option<RunEnd> {
        // Looked at before the record: a supervisor that is gone has written its record
        // already, so a record not there then is a record that never will be.
        let alive = self.id.alive();
        match read_exit_record(&self.lease) {
            Ok(Some(record)) => Some(record.end),
            Ok(None) if alive => None,
            Ok(None) => Some(RunEnd::Failed(GONE.to_owned())),
            Err(why) => Some(RunEnd::Failed(format!(
                "the exit record cannot be read: {why}"
            ))),
        }
    }

    /// [`Supervised::try_end`], or `RunEnd::Failed("the run has not ended")` while it runs.
    #[must_use]
    pub fn run_end(&self) -> RunEnd {
        self.try_end()
            .unwrap_or_else(|| RunEnd::Failed("the run has not ended".to_owned()))
    }

    /// Waits until [`Supervised::try_end`] is `Some`, looking every [`SUPERVISED_POLL`].
    /// Cancel-safe; may be called again.
    pub async fn wait(&mut self) {
        while self.try_end().is_none() {
            tokio::time::sleep(SUPERVISED_POLL).await;
        }
    }

    /// Ends the supervisor by force ([`end_supervisor`], which leaves one that has written
    /// its exit record to exit), then waits. The container itself is ended by the caller's
    /// cgroup kill and `js-contain`'s `Oci::remove`, as before.
    pub async fn force(&mut self) {
        end_supervisor(self.id, &self.lease);
        self.wait().await;
    }

    /// What the container wrote, from the exit record; empty while there is none.
    #[must_use]
    pub fn tail(&self) -> LogTail {
        self.record().map_or_else(LogTail::default, |r| r.tail)
    }

    /// When the supervisor wrote the exit record (wall clock, milliseconds since the Unix
    /// epoch), `None` while there is none.
    #[must_use]
    pub fn finished_ms(&self) -> Option<u64> {
        self.record().map(|r| r.finished_ms)
    }

    /// The exit record, if there is a readable one.
    fn record(&self) -> Option<ExitRecord> {
        read_exit_record(&self.lease).ok().flatten()
    }
}

/// Why a supervisor did not pass its handshake ([`Supervised::handshake`]).
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The process table has no start time for the child, so it cannot be named.
    #[error("the start time of the supervisor cannot be read")]
    Unnamed,
    /// The child said nothing within the limit, given here.
    #[error("the supervisor did not say it is ready within {0:?}")]
    Silent(Duration),
    /// The word could not be read: the child's output ended before the whole word, or the
    /// read failed.
    #[error("the supervisor did not say it is ready: {0}")]
    Unread(#[source] std::io::Error),
    /// The child said something other than the ready word.
    #[error("the supervisor said something other than its ready word")]
    WrongWord,
}

/// Reads the ready word from the child `pid`'s `from_child` and says who the child is. The
/// read is on a thread of its own, so that a child silent for `limit` ends the wait (the
/// thread then ends with the child, when the caller kills it and the pipe closes).
fn answer(
    pid: u32,
    mut from_child: impl std::io::Read + Send + 'static,
    ready: &[u8],
    limit: Duration,
) -> Result<SupervisorId, HandshakeError> {
    let id = SupervisorId::of(pid).ok_or(HandshakeError::Unnamed)?;
    let mut word = vec![0_u8; ready.len()];
    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = from_child.read_exact(&mut word).map(|()| word);
        // The waiter may have given up: nobody then wants the word.
        let _ = said.send(read);
    });
    let word = heard
        .recv_timeout(limit)
        .map_err(|_| HandshakeError::Silent(limit))?
        .map_err(HandshakeError::Unread)?;
    if word == ready {
        Ok(id)
    } else {
        Err(HandshakeError::WrongWord)
    }
}

/// Kills the child's process group (the child is its leader) and the child, and waits for
/// it.
fn give_up(mut child: Child) {
    let group = rustix::process::Pid::from_child(&child);
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    // It may have ended already: both calls then say so, and there is nothing to do.
    let _ = child.kill();
    let _ = child.wait();
}

/// Blocks `SIGHUP`, `SIGINT` and `SIGTERM` on the calling thread: blocked, not ignored, so
/// that a program the supervisor starts through the standard library (which empties the
/// mask in the child) has them as any process does. A supervise mode calls it first,
/// before anything makes a thread, so that every thread has the mask.
pub fn block_stop_signals() {
    // The set is valid, so the call cannot fail.
    let mut set = nix::sys::signal::SigSet::empty();
    for signal in [
        nix::sys::signal::Signal::SIGHUP,
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        set.add(signal);
    }
    let _ = set.thread_block();
}

#[cfg(test)]
mod tests;
