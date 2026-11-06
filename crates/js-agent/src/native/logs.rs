//! A native run's log files and their live upload.
//!
//! The runtime makes the job's three standard streams as files in the run's scratch
//! directory, which only the agent can enter: an empty one for standard input and one each for
//! standard output and error. The job writes the two log files itself, through the
//! descriptors the helper gives it, so they hold the whole of each stream (up to the run's
//! file-size limit, `--native-max-file-mib`) whether or not an agent runs.
//!
//! **The tail** a result carries is the end of each file ([`tail`]): the last
//! [`MAX_LOG_STREAM_LEN`] bytes, the same text a capture's ring of the stream's last bytes
//! gives.
//!
//! **The live upload.** A run whose start carried a sink, on a runtime given
//! [`LiveLogs`], has an uploader ([`crate::logs`]) from the moment its program started
//! ([`start`]). It reads at most [`LiveLogs::file_cap`] (`--log-file-cap`) bytes of each file,
//! so what reaches the store is what `crun`'s capture, which keeps the first `file_cap` bytes
//! in its file and drops the rest, would upload for the same output: chunks up to the cap and,
//! for a longer stream, a gap marker for the rest and no final. The end ([`end`]) is told at
//! the first poll that reads the run's exit, before its result is reported, with each file's
//! length as the stream's total; the result never waits for the upload. A run whose exit was
//! never read (the helper's connection ended without one, or it was removed before) is told
//! its end at its removal, after the kill ([`close`]), when its files are final. The progress record
//! is kept in the scratch directory, and the runtime stops it before the directory is removed.
//!
//! **After a restart** the run is adopted (`native/adopt.rs`) with its scratch directory, and
//! the helper kept its process and the job kept writing its files. A run whose directory holds
//! a progress record has its upload go on from it ([`adopted`]): a chunk in flight is put again
//! at the same key, and what the job wrote while no agent ran goes out as ordinary chunks, so
//! nothing is skipped or put twice. Its end is told as any run's. A directory without a record,
//! or with one that is not taken, uploads nothing more.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use js_agent_wire::LogTail;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;

use crate::cli_runtime::tail_of;
use crate::facts::Streams;
use crate::logs::{LiveLogs, RunLogs, Sink};

/// The file inside a run's scratch directory that is the job's standard input: empty.
pub(super) const STDIN: &str = "stdin";
/// The file inside a run's scratch directory that is the job's standard output.
pub(super) const STDOUT: &str = "stdout";
/// The file inside a run's scratch directory that is the job's standard error.
pub(super) const STDERR: &str = "stderr";

/// Opens (and empties) the three standard streams of a run in its scratch directory `dir`:
/// standard input, output and error, in that order. The job writes to them, the runtime reads
/// them back, and a standard input made of one reads as an end of file.
///
/// # Errors
///
/// A file cannot be opened; the text names its path.
pub(super) fn open(dir: &Path) -> Result<[File; 3], String> {
    let open = |name: &str| {
        let path = dir.join(name);
        open_log(&path).map_err(|why| format!("{}: {why}", path.display()))
    };
    Ok([open(STDIN)?, open(STDOUT)?, open(STDERR)?])
}

/// Opens (and empties) the file `path` for reading and writing.
fn open_log(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

/// The last [`MAX_LOG_STREAM_LEN`] bytes of the file at `path`, and whether older bytes
/// were left out.
fn end_of(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let keep = MAX_LOG_STREAM_LEN as u64;
    let start = file.metadata()?.len().saturating_sub(keep);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(keep).read_to_end(&mut bytes)?;
    Ok((bytes, start > 0))
}

/// The tail of the two log files in `dir`, as a result carries it.
pub(super) fn tail(dir: &Path) -> io::Result<LogTail> {
    let (stdout, cut_out) = end_of(&dir.join(STDOUT))?;
    let (stderr, cut_err) = end_of(&dir.join(STDERR))?;
    Ok(tail_of(&Streams {
        stdout,
        stderr,
        dropped: cut_out || cut_err,
    }))
}

/// The two log files of the scratch directory `dir`: standard output, then error.
fn files(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join(STDOUT), dir.join(STDERR))
}

/// What the job wrote to each stream: the lengths of its two log files (stdout, stderr); a
/// file that cannot be read counts as empty.
fn totals(dir: &Path) -> (u64, u64) {
    let (stdout, stderr) = files(dir);
    let length = |path: &Path| fs::metadata(path).map_or(0, |meta| meta.len());
    (length(&stdout), length(&stderr))
}

/// The live logs of a run whose program has started with its log files in `dir`: with
/// `logs` and a `sink`, its uploader is started, reading at most the file cap of each file
/// and keeping its record in `dir`; otherwise nothing.
pub(super) fn start(logs: Option<&LiveLogs>, sink: Option<&Sink>, dir: &Path) -> RunLogs {
    let mut run = RunLogs::asked(logs, sink);
    let (stdout, stderr) = files(dir);
    run.start_capped(logs, (&stdout, &stderr), dir);
    run
}

/// Tells the uploader of `run` that the run has ended, with what its log files in `dir` hold
/// as the totals. Nothing waits.
pub(super) fn end(run: &mut RunLogs, dir: &Path) {
    run.end(totals(dir));
}

/// At the removal of a run, after its processes were killed, so its files in `dir` hold all it
/// wrote: a run whose end no poll told (one the helper lost track of, or one removed before
/// its exit was read) is told it now, and the progress record stops and is removed before the
/// directory is. The upload goes on.
pub(super) fn close(run: &mut RunLogs, dir: &Path) {
    if run.report().is_none() {
        end(run, dir);
    }
    run.leave();
}

/// The live logs of a run adopted after a restart, whose log files are in `dir` and whose id
/// is `run_id` (its label): with `logs`, a progress record of the agent's user there gives it
/// its uploader again, going on from where the record says each stream is (see the module
/// documentation); otherwise, or with a record that is not taken (logged once), nothing.
pub(super) fn adopted(logs: Option<&LiveLogs>, dir: &Path, run_id: Option<&str>) -> RunLogs {
    let mut run = RunLogs::adopted(None);
    if let Some((logs, run_id)) = logs.zip(run_id) {
        let owner = rustix::process::geteuid().as_raw();
        let (stdout, stderr) = files(dir);
        run.resume_capped(logs, (&stdout, &stderr), dir, (run_id, owner));
    }
    run
}
