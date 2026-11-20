//! The hosts the job store knows by their `node/<host>` rows (design `multi-instance.md`, G5),
//! as a server that did not take their hello reads them: admission of a job and the hosts
//! listing both judge over these as well as over the hosts the server's own registry holds, so
//! a submit to any instance sees every host.
//!
//! A row is [`Seen`] only when this server serves its pool and the row has a booked shape
//! (otherwise there is nothing to judge or list it by). It is lost when its host was last
//! heard longer ago than the registry's longest grace, by the store's clock as the rows were
//! read, as the placement pass judges a row live ([`js_jobs::JobStore::live_nodes`]); the phase
//! it announced does not make it lost, as the registry's own view of a host does not. A row
//! silent for longer than the registry's [`js_registry::Registry::forget_after`] is left out,
//! as the registry forgets a host lost past its retention (the row itself is deleted by
//! [`js_jobs::JobStore::forget_silent_nodes`], which holds back a row that holds something).

use std::collections::BTreeMap;

use js_jobs::{HostTaint, JobStoreError, NodeRow, ScaleMark};
use js_registry::Registry;
use js_types::{HostId, Shape};

use crate::job_store::ServerJobs;

/// A host known by its row.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    /// The row, as read.
    pub(crate) row: NodeRow,
    /// Whether the host was last heard longer ago than the registry's longest grace.
    pub(crate) lost: bool,
    /// The shape the host is placed against, as the registry would place a host of its pool
    /// that said hello with this claim.
    pub(crate) placed: Shape,
    /// The scale-down mark the host carries (`hostmark/<host>`), when it has one.
    pub(crate) mark: Option<ScaleMark>,
    /// The taint row of the host (`taint/<host>`), when it has one.
    pub(crate) taint: Option<HostTaint>,
}

impl Seen {
    /// The host.
    pub(crate) fn host(&self) -> &HostId {
        &self.row.record.host
    }

    /// Whether the host's hello reported that its agent runs host jobs.
    pub(crate) fn reports_host_jobs(&self) -> bool {
        let claim = self.row.record.claim.as_ref();
        claim.is_some_and(|claim| crate::admit::reports_host_jobs(claim.capabilities()))
    }
}

/// Every host of `jobs`' `node/` rows that `registry` serves the pool of and that has a
/// booked shape, in host order, with the marks and taints the store holds for it.
///
/// # Errors
/// A read of the store failed: the caller has less than the rows and says so.
pub(crate) fn read(registry: &Registry, jobs: &ServerJobs) -> Result<Vec<Seen>, JobStoreError> {
    let (now, rows) = jobs.nodes()?;
    let marks = jobs.host_marks()?;
    let mut taints: BTreeMap<HostId, HostTaint> = jobs.host_taints()?;
    let (grace, forget) = (registry.longest_grace(), registry.forget_after());
    let seen = rows.into_iter().filter_map(|row| {
        let record = &row.record;
        let booked = record.shape.as_ref()?;
        let placed = registry.placement_shape_in(&record.pool, booked, record.claim.as_ref())?;
        if record.heard_at.saturating_add(forget) < now {
            return None;
        }
        let lost = record.heard_at.saturating_add(grace) < now;
        let host = record.host.clone();
        Some(Seen {
            row,
            lost,
            placed,
            mark: marks.get(&host).map(|(mark, _)| *mark),
            taint: taints.remove(&host),
        })
    });
    Ok(seen.collect())
}

#[cfg(test)]
#[path = "node_rows_tests.rs"]
mod tests;
