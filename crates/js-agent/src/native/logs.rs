//! The tail of a native run's log files: what the job wrote to its standard output and
//! standard error, read from the files the runtime gave it as those streams.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::Path;

use js_agent_wire::LogTail;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;

use crate::cli_runtime::tail_of;
use crate::facts::Streams;

/// The file inside a run's scratch directory that is the job's standard input: empty.
pub(super) const STDIN: &str = "stdin";
/// The file inside a run's scratch directory that is the job's standard output.
pub(super) const STDOUT: &str = "stdout";
/// The file inside a run's scratch directory that is the job's standard error.
pub(super) const STDERR: &str = "stderr";

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
