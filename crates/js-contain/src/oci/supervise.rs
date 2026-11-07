//! A container run by a supervisor process of its own, so that it outlives the agent.
//!
//! [`Oci::start`] runs `crun run` on a thread of the agent: when the agent ends, so do the
//! pipes the container writes to and the thread that waits for it. [`Oci::start_supervised`]
//! runs the same machinery (the init's report file, the capture of the output, `crun run`)
//! in a process of its own instead: the daemon's binary in the supervise mode
//! (`<daemon> <modes.supervise> <crun> <lease dir> <name> <file cap> <tail bytes> <drain
//! grace ms>`, [`run`]), in a process group of its own, started with the job drop and the
//! environment scrub every `crun` gets, with `SIGHUP`, `SIGINT` and `SIGTERM` blocked. It
//! is not a child the agent has to keep: an agent that ends leaves it and the container
//! running, and a later agent finds it again ([`Oci::adopt_supervised`]).
//!
//! **The handshake:** the supervisor makes the init's report file and the capture files,
//! then writes a ready word to its standard output. [`Oci::start_supervised`] returns once
//! it has read the word; a supervisor that exits without it, says something else, or is
//! silent for [`READY_WAIT`], is killed and reaped, and the start fails.
//!
//! **The exit record:** when `crun run` has returned and the drain of the output is over,
//! the supervisor writes `exit.json` in the lease directory whole (a temporary file,
//! synced, renamed, the directory synced), and exits. The record holds how the run ended,
//! what each stream wrote (the tail, the byte count, whether the file lacks bytes) and the
//! wall clock time it was written at. Its form is version 1, JSON:
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
//! **The handle:** [`Supervised`] reads the record. A run is over when the record is there;
//! a supervisor that is gone ([`SupervisorId::alive`]) and wrote none ended without one,
//! which is a failure of the run. It signals nothing by itself: [`Supervised::force`] ends
//! the supervisor's process group (unless the supervisor has written its record, and so only
//! exits), and the container is ended, as for every run, by the kill of the lease's cgroup
//! and [`Oci::remove`].

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::crun::{Crun, RunEnd, no_scrub};
use super::logs::{Capture, Launch, LogLimits, LogTail, StreamTail};
use super::{Oci, init, logs};
use crate::container::ContainerSpec;

/// The word a supervisor writes to its standard output once it has made its files.
pub(crate) const READY: &[u8] = b"supervising\n";

/// How often [`Supervised::wait`] looks for the end of the run.
pub const SUPERVISED_POLL: Duration = Duration::from_millis(50);

/// How a run ends whose supervisor is gone and left no exit record.
const GONE: &str = "the supervisor ended without an exit record";

/// Why a start fails whose supervisor said something else than the ready word.
const WRONG_WORD: &str = "the supervisor said something other than its ready word";

/// The most bytes of each stream's tail an exit record holds: the writer keeps the last
/// bytes of a longer tail, so that a record is always one [`read_exit_record`] accepts.
pub(crate) const MAX_TAIL_RECORDED: usize = 4 * 1024 * 1024;

/// The most bytes of an exit record that are read: two streams of [`MAX_TAIL_RECORDED`]
/// bytes in two hexadecimal digits each, and 64 KiB for the rest.
const MAX_RECORD: u64 = (2 * 2 * MAX_TAIL_RECORDED + 64 * 1024) as u64;

/// How long [`Oci::start_supervised`] waits for the ready word before it gives up on the
/// supervisor, kills its process group and fails the start.
pub const READY_WAIT: Duration = Duration::from_secs(30);

/// The exit record of the lease directory `lease`: `<lease>/exit.json`.
pub fn exit_record_path(lease: &Path) -> PathBuf {
    lease.join("exit.json")
}

/// The file the record is written to before it is renamed into place.
fn staged_record_path(lease: &Path) -> PathBuf {
    lease.join(".exit.json.tmp")
}

/// The identity of a supervisor process: its pid and its start time in clock ticks since
/// boot (field 22 of `/proc/<pid>/stat`), which together name one process for as long as the
/// host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorId {
    /// The process id.
    pub pid: u32,
    /// The start time of the process, in clock ticks since boot.
    pub start: u64,
}

impl SupervisorId {
    /// The identity of the process `pid` now, `None` when `/proc` has no such process.
    pub(crate) fn of(pid: u32) -> Option<Self> {
        stat_of(pid).map(|(_, start)| Self { pid, start })
    }

    /// Whether the process it names still runs (its `/proc/<pid>/stat` has the same start
    /// time, and it is not a zombie).
    #[must_use]
    pub fn alive(&self) -> bool {
        names(stat_of(self.pid), self.start)
    }

    /// Whether the process it names runs as the user `uid` (the owner of its `/proc` entry).
    #[must_use]
    pub fn runs_as(&self, uid: u32) -> bool {
        std::fs::metadata(format!("/proc/{}", self.pid)).is_ok_and(|meta| meta.uid() == uid)
    }

    /// Whether the process it names is the supervisor of the lease directory `lease`: its
    /// command line, which [`Oci::start_supervised`] sets, names `lease` as the lease
    /// (`<daemon> <mode> <crun> <lease> ...`). Another lease's supervisor names its own.
    #[must_use]
    pub fn supervises(&self, lease: &Path) -> bool {
        std::fs::read(format!("/proc/{}/cmdline", self.pid)).is_ok_and(|cmdline| {
            cmdline.split(|byte| *byte == 0).nth(3) == Some(lease.as_os_str().as_bytes())
        })
    }
}

/// The file in the lease directory `lease` that names the supervisor started for it.
#[must_use]
pub fn supervisor_path(lease: &Path) -> PathBuf {
    lease.join("supervisor")
}

/// The supervisor the lease directory `lease` names: the pid and start time
/// [`Oci::start_supervised`] wrote there, for a teardown that has no other record of the
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
fn note_supervisor(lease: &Path, id: SupervisorId) {
    let path = supervisor_path(lease);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .and_then(|mut file| file.write_all(format!("{} {}\n", id.pid, id.start).as_bytes()));
}

/// Whether a process whose `/proc` entry says `found` (its state letter and start time) is
/// the live process that started at `start`.
fn names(found: Option<(char, u64)>, start: u64) -> bool {
    found.is_some_and(|(state, at)| at == start && state != 'Z')
}

/// The state letter and the start time of the process `pid`, from `/proc`.
fn stat_of(pid: u32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

/// The state (field 3) and the start time (field 22) of a `/proc/<pid>/stat` line. The
/// name (field 2) is in parentheses and may hold anything, so the fields are counted from
/// the last closing parenthesis.
fn parse_stat(text: &str) -> Option<(char, u64)> {
    let (_, after) = text.rsplit_once(')')?;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

/// `SIGKILL`s the process group of the supervisor `id` (the supervisor and its `crun run`)
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
pub(crate) fn write_record(lease: &Path, record: &ExitRecord) -> std::io::Result<()> {
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
    /// Starts `command` as a supervisor (stdin and stderr null, stdout a pipe, a process
    /// group of its own) and returns once it has written `ready` to its stdout. A child
    /// that does not say it within `limit` is killed (its process group) and reaped. A
    /// thread waits for the child, so it never stays a zombie.
    ///
    /// # Errors
    /// The command could not be run, its answer is not `ready`, or it gave none within
    /// `limit`.
    pub(crate) fn spawn(
        command: Command,
        lease: PathBuf,
        ready: &[u8],
        limit: Duration,
    ) -> Result<Self, String> {
        Self::start(command, ready, limit)
            .map(|id| Self { id, lease })
            .map_err(|e| e.to_string())
    }

    fn start(mut command: Command, ready: &[u8], limit: Duration) -> std::io::Result<SupervisorId> {
        let (from_child, to_child) = std::io::pipe()?;
        command
            .stdin(Stdio::null())
            .stdout(to_child)
            .stderr(Stdio::null())
            .process_group(0);
        let spawned = command.spawn();
        // The command holds the write end of the pipe: closed, the child's exit ends the
        // read.
        drop(command);
        let mut child = spawned?;
        match answer(child.id(), from_child, ready, limit) {
            Ok(id) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(id)
            }
            Err(e) => {
                give_up(child);
                Err(e)
            }
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
    /// cgroup kill and [`Oci::remove`], as before.
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

/// Reads the ready word from the child `pid`'s `from_child` and says who the child is. The
/// read is on a thread of its own, so that a child silent for `limit` ends the wait (the
/// thread then ends with the child, when the caller kills it and the pipe closes).
fn answer(
    pid: u32,
    mut from_child: impl std::io::Read + Send + 'static,
    ready: &[u8],
    limit: Duration,
) -> std::io::Result<SupervisorId> {
    let unreadable = std::io::Error::other("the start time of the supervisor cannot be read");
    let id = SupervisorId::of(pid).ok_or(unreadable)?;
    let mut word = vec![0_u8; ready.len()];
    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = from_child.read_exact(&mut word).map(|()| word);
        // The waiter may have given up: nobody then wants the word.
        let _ = said.send(read);
    });
    let word = heard
        .recv_timeout(limit)
        .map_err(|_| {
            std::io::Error::other(format!(
                "the supervisor did not say it is ready within {limit:?}"
            ))
        })?
        .map_err(|e| {
            std::io::Error::other(format!("the supervisor did not say it is ready: {e}"))
        })?;
    if word == ready {
        Ok(id)
    } else {
        Err(std::io::Error::other(WRONG_WORD))
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

impl Oci {
    /// Starts the container of `spec` under a supervisor process (`<daemon> <modes.supervise>
    /// <crun> <lease dir> <name> <file cap> <tail bytes> <drain grace ms>`, the file cap being
    /// `file_cap`, or the runtime's [`LogLimits::file_cap`] when it is `None`, and the tail and
    /// the grace the runtime's), run with the job
    /// drop and the environment scrub every `crun` gets, stdin null, stdout a pipe used once
    /// for the ready word, stderr null, in a process group of its own. Returns once the
    /// supervisor wrote its ready word (it has made the init report and the capture files); a
    /// supervisor that does not answer is killed and reaped, and the call fails. A thread
    /// waits for the child so it never stays a zombie. The supervisor's pid and start time are
    /// then written to the lease directory ([`recorded_supervisor`]). The bundle
    /// ([`Oci::create`]) must be written.
    ///
    /// # Errors
    /// An exit record is already in the lease directory, or the supervisor could not be
    /// started or did not answer (the lease directory has the files of an earlier start, say).
    pub fn start_supervised(
        &self,
        spec: &ContainerSpec,
        file_cap: Option<u64>,
    ) -> Result<Supervised, String> {
        let record = exit_record_path(&spec.dir);
        if record.symlink_metadata().is_ok() {
            return Err(format!(
                "{}: an exit record is already there",
                record.display()
            ));
        }
        let limits = self.run_logs(file_cap);
        let mut command = Command::new(&self.daemon);
        command
            .arg(&self.modes.supervise)
            .arg(self.crun.program())
            .arg(&spec.dir)
            .arg(&spec.name)
            .arg(limits.file_cap.to_string())
            .arg(limits.tail.to_string())
            .arg(limits.drain_grace.as_millis().to_string());
        self.crun.prepare(&mut command);
        let supervised = Supervised::spawn(command, spec.dir.clone(), READY, READY_WAIT)?;
        note_supervisor(&spec.dir, supervised.id());
        Ok(supervised)
    }

    /// The handle of a container an earlier process started under the supervisor `id`, whose
    /// lease directory is `lease`. Starts nothing.
    #[must_use]
    pub fn adopt_supervised(&self, lease: &Path, id: SupervisorId) -> Supervised {
        Supervised {
            id,
            lease: lease.to_owned(),
        }
    }
}

/// Makes the runtime the supervisor drives `crun run` on.
type RuntimeMaker = fn() -> std::io::Result<tokio::runtime::Runtime>;

/// One thread, with the drivers `crun`'s child needs.
fn one_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// What the supervisor was told: `<crun> <lease dir> <name> <file cap> <tail bytes> <drain
/// grace ms>`.
struct Request {
    program: PathBuf,
    lease: PathBuf,
    name: String,
    limits: LogLimits,
}

impl Request {
    fn parse(args: &[OsString]) -> Option<Self> {
        let [program, lease, name, file_cap, tail, grace] = args else {
            return None;
        };
        let number = |arg: &OsString| arg.to_str()?.parse::<u64>().ok();
        Some(Self {
            program: PathBuf::from(program),
            lease: PathBuf::from(lease),
            name: name.to_str()?.to_owned(),
            limits: LogLimits {
                file_cap: number(file_cap)?,
                tail: usize::try_from(number(tail)?).ok()?,
                drain_grace: Duration::from_millis(number(grace)?),
            },
        })
    }
}

/// The supervise mode, its arguments after the mode word (`modes.supervise`) being `<crun>
/// <lease dir> <name> <file cap> <tail bytes> <drain grace ms>`: blocks `SIGHUP`, `SIGINT`
/// and `SIGTERM` (blocked, not ignored, so that the `crun` it starts has them as any
/// process does), makes the init's report file and the capture files, tells its parent it
/// is ready on standard output, runs `crun run` over the lease's bundle as [`Oci::start`]
/// does, and writes the exit record. Returns 0 once the record is written, 1 when a file
/// could not be made, the ready word not written or the record not written, and 2 for
/// arguments it cannot read.
#[must_use]
pub fn run(args: &[OsString]) -> u8 {
    // Before anything makes a thread, so that every thread has the mask. The set is valid,
    // so the call cannot fail.
    let mut set = nix::sys::signal::SigSet::empty();
    for signal in [
        nix::sys::signal::Signal::SIGHUP,
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        set.add(signal);
    }
    let _ = set.thread_block();
    run_with(args, &mut std::io::stdout().lock(), one_thread_runtime)
}

/// [`run`] telling `ready` it is ready, on the runtime `runtime` makes (the place it can
/// fail, a test's to cause), with the signals as they are.
fn run_with(args: &[OsString], ready: &mut dyn std::io::Write, runtime: RuntimeMaker) -> u8 {
    let Some(request) = Request::parse(args) else {
        eprintln!("supervise-run: usage: <crun> <lease> <name> <cap> <tail> <grace>, not {args:?}");
        return 2;
    };
    let (report, capture) = match prepare(&request) {
        Ok(made) => made,
        Err(why) => {
            eprintln!("supervise-run: {why}");
            return 1;
        }
    };
    if let Err(e) = ready.write_all(READY).and_then(|()| ready.flush()) {
        eprintln!("supervise-run: the ready word: {e}");
        return 1;
    }
    let end = crun_run(&request, report, &capture, runtime);
    let record = ExitRecord {
        end,
        tail: capture.tail(),
        finished_ms: wall_ms(),
    };
    match write_record(&request.lease, &record) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("supervise-run: the exit record: {e}");
            1
        }
    }
}

/// The init's report file and the capture files of the lease.
fn prepare(request: &Request) -> Result<(std::fs::File, Arc<Capture>), String> {
    let report = init::create_report(&request.lease)?;
    let capture = Arc::new(Capture::create(&request.lease, request.limits)?);
    Ok((report, capture))
}

/// Runs `crun run` over the lease's bundle on the runtime `runtime` makes, draining the
/// output into `capture`, as [`Oci::start`] does. The process already runs as the job user
/// with the scrubbed environment, so `crun` is started as it is.
fn crun_run(
    request: &Request,
    report: std::fs::File,
    capture: &Arc<Capture>,
    runtime: RuntimeMaker,
) -> RunEnd {
    let crun = Crun::new(request.program.clone(), no_scrub, None);
    let (lease, name) = (request.lease.clone(), request.name.clone());
    let bundle = lease.join("bundle");
    let launch: Launch = Box::new(move |stdout, stderr| {
        Box::pin(async move {
            crun.run(&lease, &name, &bundle, report, stdout, stderr)
                .await
        })
    });
    match runtime() {
        Ok(runtime) => runtime.block_on(logs::supervise(capture.clone(), launch)),
        Err(e) => RunEnd::Failed(format!("the runtime of the supervisor: {e}")),
    }
}

/// The wall clock, in milliseconds since the Unix epoch.
fn wall_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "supervise_tests.rs"]
mod tests;
