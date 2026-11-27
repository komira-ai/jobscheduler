//! The host silence the job service shows on a job (`Job.host_silent_since_ms`, design section
//! 6.3), read from the host's `node/<host>` row (design `multi-instance.md`, section 5 and G5):
//! the row is the host's session whichever server took its hello or its beats, so every server
//! shows one silence for a host, and a host whose session another server holds shows its own.
//!
//! A host is silent once its row was last heard longer ago than its pool's grace, by the store's
//! clock, as a row is judged live for placement and listing; the silence is the instant that
//! grace ran out (`heard_at` plus the grace). `GetJob` and `ListJobs` read it beside the jobs, in
//! one call of the tables for every host of an answer, and only for the hosts of jobs that have
//! not ended. A host with no row (never said hello through a release that writes one, or
//! forgotten) shows none, as does a row this build cannot read, which is logged; a read of the
//! store that fails fails the call, as every read of the tables does.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{JobRecord, JobStoreError};
use js_registry::Registry;
use js_types::HostId;

use super::view;
use crate::job_store::ServerJobs;

/// The silence of each host of an answer that has one, by host: the instant it missed its
/// grace, in milliseconds since the Unix epoch.
pub(crate) type Silences = BTreeMap<HostId, u64>;

/// Where the read calls read a host's silence: its row, judged by the grace of its pool as the
/// registry serves it (the longest grace the registry serves for a pool it does not).
#[derive(Clone)]
pub(crate) struct Silence(Registry);

impl Silence {
    /// The silence of the hosts' rows, judged by the graces of `registry`'s pools.
    pub(crate) fn of(registry: Registry) -> Silence {
        Silence(registry)
    }

    /// The silence of each host of `hosts` from its row in `tables`, at the store's time now:
    /// a host whose row was heard within its pool's grace, has no row, or has one this build
    /// cannot read has none. It blocks on the store.
    ///
    /// # Errors
    /// The store's error when its clock or a row cannot be read; a silence read before it is
    /// not answered.
    pub(crate) fn read(
        &self,
        tables: &ServerJobs,
        hosts: &BTreeSet<HostId>,
    ) -> Result<Silences, JobStoreError> {
        let now = tables.now()?;
        let mut silences = Silences::new();
        for host in hosts {
            let row = match tables.read_node(host) {
                Ok(Some(row)) => row,
                Ok(None) => continue,
                Err(error @ JobStoreError::Store(_)) => return Err(error),
                Err(error) => {
                    tracing::warn!(%host, %error, "a host's row this build cannot read shows no silence");
                    continue;
                }
            };
            let record = row.record;
            let grace = self.0.pool_grace(&record.pool);
            let grace = grace.unwrap_or_else(|| self.0.longest_grace());
            let missed = record.heard_at.saturating_add(grace);
            if missed < now {
                silences.insert(host.clone(), view::millis(missed));
            }
        }
        Ok(silences)
    }
}

/// The hosts of the jobs of `jobs` that have not ended, whose silence is read.
pub(crate) fn holding<'a>(jobs: impl IntoIterator<Item = &'a JobRecord>) -> BTreeSet<HostId> {
    let live = jobs.into_iter().filter(|job| !job.state.is_ended());
    live.filter_map(|job| job.host.clone()).collect()
}

#[cfg(test)]
#[path = "silence_tests.rs"]
mod tests;
