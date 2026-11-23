//! What the job store's own rows say of each job, read through the job store.
//!
//! An export of the job service's store holds each job's row (`job/<id>`) as its stored
//! bytes, one row per line as `key<TAB>base64(value)`. The rows are written into a scratch
//! SQLite state store and read back with the job store's own reader (`JobStore::get_job`), so a
//! row is read exactly as the server reads it. A job's class, fallback class and client are
//! the estimator keys the placer sizes it by (`JobSizes::key_of`, `fallback_key_of`,
//! `client_key_of` in `js-sched-placer`); its times are the store's.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_jobs::{JobId, JobRecord, JobStore, RandomIds, ResultReason};
use js_sched_placer::sizes::JobSizes;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Cond, Store as _, StoreTime, SystemClock, Txn};

use crate::trace::End;

/// The scratch store's file in the directory it is given.
pub const SCRATCH_FILE: &str = "job-rows.sqlite";

/// The rows written in one transaction.
const BATCH: usize = 512;

/// What a job's row says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Its class's estimator key, as text.
    pub class: String,
    /// Its fallback class's key, as text, when it names one.
    pub fallback: Option<String>,
    /// Its client's key, as text.
    pub client: String,
    /// How it ended.
    pub end: End,
    /// Its CPU time, in microseconds.
    pub cpu_usec: Option<u64>,
    /// Its most CPU over about five seconds, in millicores.
    pub peak_milli: Option<u64>,
    /// The `some` total of its `cpu.pressure`, in microseconds.
    pub pressure_usec: Option<u64>,
    /// When it was submitted, in milliseconds of the store's clock.
    pub queued_ms: u64,
    /// When it was last placed.
    pub placed_ms: Option<u64>,
    /// When it began running.
    pub started_ms: Option<u64>,
    /// When it ended.
    pub ended_ms: Option<u64>,
}

/// Why the rows do not read.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RowsError {
    /// A line is not a row.
    #[error("line {line} of the rows: {what}")]
    Line {
        /// The 1-based line number.
        line: usize,
        /// What is wrong with it.
        what: &'static str,
    },
    /// The scratch store failed.
    #[error("the scratch store: {0}")]
    Store(String),
    /// A job's row does not read.
    #[error("the row of {job}: {reason}")]
    Row {
        /// The job.
        job: String,
        /// The reader's reason.
        reason: String,
    },
}

/// The facts of every job row in `text`, by job id, read through a scratch store made in the
/// directory `scratch` (its file [`SCRATCH_FILE`] is written over). Lines of other rows are
/// left out.
///
/// # Errors
/// [`RowsError`] for a line that is not a row, a scratch store that fails, or a row the job
/// store does not read.
pub fn read(text: &str, scratch: &Path) -> Result<BTreeMap<String, Facts>, RowsError> {
    let mut rows = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let bad = |what| RowsError::Line { line: at + 1, what };
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once('\t').ok_or(bad("no tab"))?;
        if !key.starts_with(js_jobs::keys::JOB) {
            continue;
        }
        let bytes = STANDARD
            .decode(value.trim())
            .map_err(|_| bad("not base64"))?;
        rows.push((key.to_owned(), bytes));
    }
    let store = |error: &dyn std::fmt::Display| RowsError::Store(error.to_string());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    let path = scratch.join(SCRATCH_FILE);
    let opened = SqliteStore::open(&path, clock, Options::default()).map_err(|e| store(&e))?;
    for batch in rows.chunks(BATCH) {
        let txn = batch.iter().fold(Txn::new(), |txn, (key, value)| {
            txn.put(key.clone(), value.clone(), Cond::Any)
        });
        let refused = RowsError::Store("the scratch store did not take the rows".to_owned());
        opened.txn(txn).ok().and_then(Result::ok).ok_or(refused)?;
    }
    let unopened = RowsError::Store("the job store did not open on the scratch store".to_owned());
    let jobs = JobStore::new(Arc::new(opened), RandomIds)
        .ok()
        .ok_or(unopened)?;
    let mut facts = BTreeMap::new();
    for (key, _) in &rows {
        let id = &key[js_jobs::keys::JOB.len()..];
        let failed = |reason: String| RowsError::Row {
            job: id.to_owned(),
            reason,
        };
        let job_id = JobId::parse(id).map_err(|e| failed(e.to_string()))?;
        let job = jobs.get_job(&job_id).map_err(|e| failed(e.to_string()))?;
        facts.extend(job.map(|job| (id.to_owned(), facts_of(&job))));
    }
    Ok(facts)
}

/// A time of the store's clock in milliseconds.
fn ms(time: StoreTime) -> u64 {
    u64::try_from(time.as_duration().as_millis()).unwrap_or(u64::MAX)
}

/// A duration in microseconds.
fn usec(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// What `job` says.
fn facts_of(job: &JobRecord) -> Facts {
    let result = job.result.as_ref();
    let usage = result.and_then(|result| result.usage);
    let end = match result.map(|result| (result.reason, result.exit_code)) {
        Some((ResultReason::Exit, Some(0))) => End::Ok,
        Some((ResultReason::Memory, _)) => End::Memory,
        _ => End::Failed,
    };
    Facts {
        class: format!("{:?}", JobSizes::key_of(job)),
        fallback: JobSizes::fallback_key_of(job).map(|key| format!("{key:?}")),
        client: format!("{:?}", JobSizes::client_key_of(job)),
        end,
        cpu_usec: usage.map(|usage| usec(usage.cpu_time)),
        peak_milli: usage
            .and_then(|usage| usage.cpu_peak)
            .map(|peak| peak.millicores),
        pressure_usec: usage.and_then(|usage| usage.cpu_pressure_some).map(usec),
        queued_ms: ms(job.submitted_at),
        placed_ms: job.placed_at.map(ms),
        started_ms: job.started_at.map(ms),
        ended_ms: job.ended_at.map(ms),
    }
}
