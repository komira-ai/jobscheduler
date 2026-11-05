//! The progress record of one job's upload: `logs-upload.json` in the lease directory, which a
//! restarted agent reads when it adopts the job, to go on from where the last one stopped.
//!
//! The record is JSON, mode `0600`:
//!
//! ```json
//! {"version":1,
//!  "sink":{"endpoint":"…","region":"…","bucket":"…","prefix":"…/<job id>/","path_style":false},
//!  "streams":{"stdout":S,"stderr":S}}
//! ```
//!
//! with `S` = `{"next":N,"cut":null|{"offset":a,"len":n},"chunks":[offset,…],
//! "gap_done":bool,"final_done":bool}`: `next` is the offset up to which the store has
//! acknowledged the stream's chunks, `cut` the chunk being put, `chunks` the offsets of the
//! chunks put and not yet deleted, and the two flags whether the gap marker and the final are
//! stored. It holds no secret: the sink names only where the logs go ([`LogSink`]), and the
//! credential is the host's, held in memory only.
//!
//! It is written whole, as a temporary file renamed into place (no `fsync`: the lease directory
//! is the agent's own, and a record lost with the host is a record that is not there), and only
//! by the uploader's thread. A reader ignores fields it does not know and refuses another
//! version.

use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use js_agent_wire::LogSink;
use serde::{Deserialize, Serialize};

use super::sink::Sink;

/// The record's name in the lease directory.
pub(crate) const RECORD_FILE: &str = "logs-upload.json";

/// The name of a record being written, before it is renamed into place.
const RECORD_TEMP: &str = ".logs-upload.json.tmp";

/// The form of the record this agent writes, and the only one it reads.
pub(super) const VERSION: u32 = 1;

/// The most bytes of a record that are read: a longer file is cut there, and is then no
/// record. The offsets of the chunks are its bulk, some twenty bytes each, so this holds about
/// three million.
const RECORD_MAX: u64 = 64 << 20;

/// What the record says of one stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Progress {
    /// The offset up to which the stream's chunks are stored.
    pub(super) next: u64,
    /// The chunk being put.
    pub(super) cut: Option<Cut>,
    /// The offsets of the chunks put and not yet deleted, in order.
    pub(super) chunks: Vec<u64>,
    /// Whether the gap marker is stored.
    pub(super) gap_done: bool,
    /// Whether the final is stored.
    pub(super) final_done: bool,
}

/// A chunk being put: its first byte's offset and its length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Cut {
    /// The offset of its first byte.
    pub(super) offset: u64,
    /// Its length in bytes.
    pub(super) len: usize,
}

/// The two streams of a record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Streams {
    /// Standard output.
    pub(super) stdout: Progress,
    /// Standard error.
    pub(super) stderr: Progress,
}

/// The record as it is read.
#[derive(Deserialize)]
struct Record {
    sink: LogSink,
    streams: Streams,
}

/// The record as it is written.
#[derive(Serialize)]
struct Written<'a> {
    version: u32,
    sink: &'a LogSink,
    streams: Streams,
}

/// The record's version alone, read before the rest so that another version is refused as
/// such, whatever its form.
#[derive(Deserialize)]
struct Form {
    version: u32,
}

/// Reads the record in the lease directory `dir` of the job `job_id`: `Ok(None)` when there is
/// none, and the sink and the two streams when there is one this agent can go on from.
///
/// # Errors
///
/// Why the file that is there is not taken: it is a link or not a regular file, it is not
/// owned by `owner` (the agent's user), it is not a record (or is longer than one can be), it is
/// of another version, its sink cannot be used, or its prefix is not the job's.
pub(super) fn read(
    dir: &Path,
    job_id: &str,
    owner: u32,
) -> Result<Option<(Sink, Streams)>, String> {
    // A link is refused, not followed, and a pipe is opened without waiting for a writer.
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(RECORD_FILE));
    let file = match opened {
        Ok(file) => file,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(format!("it cannot be opened: {why}")),
    };
    let body = load(file, owner).map_err(|why| why.to_string())?;
    let form: Form =
        serde_json::from_slice(&body).map_err(|why| format!("it is not a record: {why}"))?;
    if form.version != VERSION {
        return Err(format!("its form is version {}", form.version));
    }
    let record: Record =
        serde_json::from_slice(&body).map_err(|why| format!("it is not a record: {why}"))?;
    let sink = Sink::of_wire(&record.sink)?;
    let last = sink
        .prefix()
        .as_str()
        .trim_end_matches('/')
        .rsplit('/')
        .next();
    if last != Some(job_id) {
        return Err(format!(
            "its prefix {:?} is not job {job_id}'s",
            sink.prefix().as_str()
        ));
    }
    Ok(Some((sink, record.streams)))
}

/// The bytes of the opened record `file`, which must be a regular file owned by `owner`; at
/// most [`RECORD_MAX`] of them.
fn load(file: std::fs::File, owner: u32) -> io::Result<Vec<u8>> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("it is not a regular file"));
    }
    if meta.uid() != owner {
        return Err(io::Error::other(format!(
            "it is owned by user {}, not the agent's",
            meta.uid()
        )));
    }
    let mut body = Vec::new();
    file.take(RECORD_MAX).read_to_end(&mut body)?;
    Ok(body)
}

/// Writes `body` whole as the record of the lease directory `dir`: a new file, never one that
/// is there (a link left in its place is not followed), renamed over the record.
fn write(dir: &Path, body: &[u8]) -> io::Result<()> {
    let temp = dir.join(RECORD_TEMP);
    // A file a dead process left under this name; one that stays makes `create_new` fail.
    let _ = fs::remove_file(&temp);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)
        .and_then(|mut file| file.write_all(body))
        .and_then(|()| fs::rename(&temp, dir.join(RECORD_FILE)));
    // Nothing is left when the rename went through; a file that was not renamed is nobody's.
    let _ = fs::remove_file(&temp);
    written
}

/// The writer of one job's record, shared by the uploader's thread, which writes it, and the
/// handle, which stops it before the lease directory is removed. A write and the stop hold one
/// lock, so no write is under way once the stop has returned: the removal of the directory
/// never meets a file being made in it.
#[derive(Clone)]
pub(super) struct Recorder(Arc<Mutex<Keeping>>);

/// What the recorder holds.
struct Keeping {
    /// The lease directory; none once the record is given up or left (or for an uploader that
    /// keeps none).
    dir: Option<PathBuf>,
    /// The job's sink, as the record says it.
    sink: LogSink,
    /// The bytes last written.
    last: Option<Vec<u8>>,
    /// The job's prefix, for the log line.
    prefix: String,
}

impl Recorder {
    /// A recorder that writes the record of the job of `sink` in `dir`, or none.
    pub(super) fn new(dir: Option<PathBuf>, sink: &Sink) -> Self {
        Self(Arc::new(Mutex::new(Keeping {
            dir,
            sink: sink.wire(),
            last: None,
            prefix: sink.prefix().as_str().to_owned(),
        })))
    }

    /// Writes the record of `streams` (stdout, stderr), unless it is what was last written. The
    /// first write that fails is logged once and ends the record: the file is removed, so a
    /// restart finds no record that lags behind the store, and the upload goes on without one.
    pub(super) fn note(&self, [stdout, stderr]: [Progress; 2]) {
        let mut keeping = self.lock();
        let Some(dir) = keeping.dir.clone() else {
            return;
        };
        let written = Written {
            version: VERSION,
            sink: &keeping.sink,
            streams: Streams { stdout, stderr },
        };
        let body = serde_json::to_vec(&written).unwrap_or_default();
        if keeping.last.as_ref() == Some(&body) {
            return;
        }
        match write(&dir, &body) {
            Ok(()) => keeping.last = Some(body),
            Err(why) => {
                tracing::warn!(
                    prefix = keeping.prefix.as_str(),
                    "live logs: the progress record could not be written, and the upload goes \
                     on without one: {why}"
                );
                keeping.dir = None;
                let _ = fs::remove_file(dir.join(RECORD_FILE));
            }
        }
    }

    /// Writes no record from now on, and removes the one written: a lease directory that
    /// outlives a removal that failed holds no record that lags behind the store. A write
    /// under way has ended when this returns.
    pub(super) fn leave(&self) {
        let mut keeping = self.lock();
        keeping.dir.take().iter().for_each(|dir| {
            let _ = fs::remove_file(dir.join(RECORD_FILE));
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keeping> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
