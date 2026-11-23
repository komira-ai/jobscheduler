//! The command line.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::convert::{ConvertError, convert, results};
use crate::report::report;
use crate::rows::{self, RowsError};
use crate::sim::Farm;
use crate::trace::{self, TraceError};

/// `js-cpu-replay`: replays the job service's recorded jobs against ways of packing CPU on a
/// host.
#[derive(Clone, Debug, Parser)]
#[command(name = "js-cpu-replay", version, about)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Turns an export of job records into an anonymous trace.
    Convert {
        /// The records `GetUsage` returned, one JSON object per line.
        #[arg(long)]
        records: PathBuf,
        /// The agents' result lines (`job: result run=...`), one per job.
        #[arg(long)]
        results: Option<PathBuf>,
        /// The job store's rows, one per line as `key<TAB>base64(value)`.
        #[arg(long, requires = "scratch")]
        rows: Option<PathBuf>,
        /// The directory the rows are read through a scratch store in.
        #[arg(long)]
        scratch: Option<PathBuf>,
        /// Leave out the jobs that started before this time, in milliseconds since the Unix
        /// epoch.
        #[arg(long)]
        since_ms: u64,
        /// The trace to write; a name ending in `.gz` is written compressed.
        #[arg(long)]
        out: PathBuf,
    },
    /// Replays a trace on six hosts of 88 threads and prints the report.
    Run {
        /// The trace; a name ending in `.gz` is read compressed.
        trace: PathBuf,
    },
}

/// What can go wrong between the files and the report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file cannot be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        source: io::Error,
    },
    /// A file cannot be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// The reason.
        source: io::Error,
    },
    /// The trace does not read.
    #[error("the trace: {0}")]
    Trace(#[from] TraceError),
    /// The records do not convert.
    #[error(transparent)]
    Convert(#[from] ConvertError),
    /// The store's rows do not read.
    #[error(transparent)]
    Rows(#[from] RowsError),
    /// The output cannot be written.
    #[error("cannot write the output: {0}")]
    Output(#[from] io::Error),
}

/// Whether `path` names a compressed file.
fn compressed(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "gz")
}

/// The text of `path`, uncompressed when its name ends in `.gz`.
fn read(path: &Path) -> Result<String, Error> {
    let failed = |source| Error::Read {
        path: path.to_owned(),
        source,
    };
    let bytes = fs::read(path).map_err(failed)?;
    let mut text = String::new();
    if compressed(path) {
        GzDecoder::new(bytes.as_slice())
            .read_to_string(&mut text)
            .map_err(failed)?;
    } else {
        text = String::from_utf8(bytes)
            .map_err(|error| failed(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    }
    Ok(text)
}

/// Writes `text` to `path`, compressed when its name ends in `.gz`.
fn write(path: &Path, text: &str) -> Result<(), Error> {
    let failed = |source| Error::Write {
        path: path.to_owned(),
        source,
    };
    let bytes = if compressed(path) {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(text.as_bytes()).map_err(failed)?;
        encoder.finish().map_err(failed)?
    } else {
        text.as_bytes().to_vec()
    };
    fs::write(path, bytes).map_err(failed)
}

/// Runs the command; what it says goes to `out`.
///
/// # Errors
/// Whatever stops a read, the conversion, the replay or a write.
pub fn run(cli: &Cli, out: &mut dyn Write) -> Result<(), Error> {
    match &cli.command {
        Command::Convert {
            records,
            results: lines,
            rows: stored,
            scratch,
            since_ms,
            out: to,
        } => {
            let usage = read(records)?;
            let lines = match lines {
                Some(path) => read(path)?,
                None => String::new(),
            };
            let facts = match stored {
                Some(path) => {
                    rows::read(&read(path)?, scratch.as_deref().unwrap_or(Path::new(".")))?
                }
                None => BTreeMap::new(),
            };
            let converted = convert(&usage, &results(&lines), &facts, *since_ms)?;
            write(to, &trace::write(&converted.jobs))?;
            writeln!(
                out,
                "{} records: {} jobs in the trace ({} with a store row, {} with a result line); {} \
                 did not run, {} ran on a host of under a hundredth of them, {} started before \
                 the cut",
                converted.read,
                converted.jobs.len(),
                converted.with_row,
                converted.with_result,
                converted.not_run,
                converted.other_host,
                converted.before_cut,
            )?;
        }
        Command::Run { trace: path } => {
            let jobs = trace::parse(&read(path)?)?;
            out.write_all(report(&jobs, Farm::OURS).as_bytes())?;
        }
    }
    Ok(())
}
