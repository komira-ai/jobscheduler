//! The completion callback status the job service shows on a job (`Job.callback_status`,
//! `docs/design/job-retry-and-completion-callback.md`, section 5.5). It is not in the job's
//! record: the store keeps it in the job's `callback/` row, so `GetJob`, `ListJobs` and
//! `SubmitJob` read it beside the record, in one call of the tables for all the jobs of an
//! answer, and only for the jobs that asked for a callback. A job without one reads nothing and
//! shows no status. A job whose rows went between the read of its record and the read of its
//! status (swept at its retention end) shows none either.

use std::collections::BTreeMap;

use js_jobs::{CallbackStatus, JobId, JobRecord, JobStoreError};

use crate::job_store::ServerJobs;

/// The status of each job of an answer that has one, by its id.
pub(crate) type Statuses = BTreeMap<JobId, CallbackStatus>;

/// The ids of the jobs of `jobs` that asked for a callback, whose status is read.
pub(crate) fn asking<'a>(jobs: impl IntoIterator<Item = &'a JobRecord>) -> Vec<JobId> {
    let asked = jobs.into_iter().filter(|job| job.spec.callback.is_some());
    asked.map(|job| job.id.clone()).collect()
}

/// The status of each job of `asked` from `tables`: a job whose rows are gone has none.
///
/// # Errors
/// The store's error when a row cannot be read; a status read before it is not answered.
pub(crate) fn read(tables: &ServerJobs, asked: &[JobId]) -> Result<Statuses, JobStoreError> {
    let mut statuses = Statuses::new();
    for id in asked {
        match tables.callback_status(id) {
            Ok(Some(status)) => {
                statuses.insert(id.clone(), status);
            }
            Ok(None) | Err(JobStoreError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(statuses)
}

#[cfg(test)]
#[path = "statuses_tests.rs"]
mod tests;
