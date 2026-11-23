//! Why a host was not given a queued job: what the walk over the queue learns of the
//! scheduler's refusals ([`js_sched::Scheduler::take_refusals`]) and what it says of them.
//!
//! After each placement round of the walk the engine takes the refusals of that round, which
//! are those of the one job just offered, and logs each at debug level with the job, the host,
//! the kind and the figures. It also keeps, per host and kind, how many refusals there were
//! and the figures of the latest, so the pass can say at info level, once per host, why the
//! host was not filled, and count each refusal by kind in the server's metrics
//! ([`crate::engine::jobs::PassMetrics::refused`]). The book holds a few entries per host,
//! whatever the number of jobs.
//!
//! A host the walk's own ledger ([`crate::room::Room`]) finds full by the memory its
//! allocations book is refused before the scheduler sees it, and is booked the same way as a
//! `booked-memory` refusal with the ledger's figures ([`crate::room::Room::memory_refusal`]).
//!
//! A host that fails the scheduler's other checks (a lease cap, a request larger than the
//! host, a host that must be empty) is refused with no kind: the scheduler does not say which
//! of those held, so it is not counted here.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use js_jobs::JobId;
use js_sched::{HostRefusal, Refusal, RefusalKind};
use js_types::HostId;
use js_types::sched::WorkerId;
use tracing::{debug, info, warn};

use super::Engine;
use super::jobs::PassMetrics;
use crate::map;

/// How often a host refused for one kind in a walk, and the latest of them.
#[derive(Debug, Clone)]
struct Tally {
    count: u64,
    job: JobId,
    latest: Refusal,
}

/// The refusals of a walk, per host and kind, until the pass takes them.
#[derive(Debug, Default)]
pub(crate) struct Book {
    hosts: BTreeMap<WorkerId, BTreeMap<RefusalKind, Tally>>,
    dropped: u64,
}

/// What the book held when the pass took it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refused {
    /// Per host, per kind: the refusals and the latest one's job and figures.
    hosts: BTreeMap<HostId, BTreeMap<RefusalKind, (u64, JobId, Refusal)>>,
    /// Refusals the scheduler's buffer could not keep.
    dropped: u64,
}

impl Engine {
    /// Takes the refusals of the round just run for `job`, logs each and adds it to the book.
    pub(super) fn book_refusals(&mut self, job: &JobId) {
        let taken = self.sched.take_refusals();
        self.refused.dropped = self.refused.dropped.saturating_add(taken.dropped);
        for HostRefusal { worker, refusal } in taken.kept {
            self.book_refusal(job, worker, refusal);
        }
    }

    /// Logs that `worker` refused `job` for `refusal` and adds it to the book.
    pub(super) fn book_refusal(&mut self, job: &JobId, worker: WorkerId, refusal: Refusal) {
        debug!(
            job = %job,
            host = %worker,
            reason = refusal.kind.as_str(),
            detail = %figures(&refusal),
            "a host refused a queued job"
        );
        let tallies = self.refused.hosts.entry(worker).or_default();
        let tally = tallies.entry(refusal.kind).or_insert_with(|| Tally {
            count: 0,
            job: job.clone(),
            latest: refusal,
        });
        tally.count = tally.count.saturating_add(1);
        tally.job = job.clone();
        tally.latest = refusal;
    }

    /// The refusals the walks since the last call booked, emptying the book.
    pub fn take_refused(&mut self) -> Refused {
        let book = std::mem::take(&mut self.refused);
        // Every worker of the engine is a host's (`map::worker`), so each name is a host id.
        let hosts = book.hosts.into_iter().filter_map(|(worker, kinds)| {
            let kinds = kinds
                .into_iter()
                .map(|(kind, t)| (kind, (t.count, t.job, t.latest)))
                .collect();
            map::host_id(&worker).ok().map(|host| (host, kinds))
        });
        Refused {
            hosts: hosts.collect(),
            dropped: book.dropped,
        }
    }
}

impl Refused {
    /// Whether no host refused a job.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.dropped == 0
    }

    /// The refusals counted for `host` and `kind`.
    #[must_use]
    pub fn count(&self, host: &HostId, kind: RefusalKind) -> u64 {
        self.hosts
            .get(host)
            .and_then(|kinds| kinds.get(&kind))
            .map_or(0, |(count, _, _)| *count)
    }

    /// The job and figures of the latest refusal of `host` for `kind`.
    #[must_use]
    pub fn latest(&self, host: &HostId, kind: RefusalKind) -> Option<(&JobId, &Refusal)> {
        let (_, job, latest) = self.hosts.get(host)?.get(&kind)?;
        Some((job, latest))
    }

    /// Tells `metrics` each refusal by host and kind, and logs one info line per host: its
    /// count for each kind, with the job and the figures of the latest refusal of that kind.
    pub fn announce(&self, metrics: &dyn PassMetrics) {
        for (host, kinds) in &self.hosts {
            let mut said = String::new();
            for (kind, (count, job, latest)) in kinds {
                for _ in 0..*count {
                    metrics.refused(host, kind.as_str());
                }
                if !said.is_empty() {
                    said.push_str("; ");
                }
                let _ = write!(
                    said,
                    "{} x{count} (latest job {job}: {})",
                    kind.as_str(),
                    figures(latest)
                );
            }
            info!(host = %host, refusals = %said, "a host refused queued jobs in this pass");
        }
        if self.dropped > 0 {
            warn!(
                dropped = self.dropped,
                "refusals the scheduler could not keep were counted and not logged"
            );
        }
    }
}

/// The figures of a refusal: `counted + new > limit`, with the unit of its kind.
fn figures(refusal: &Refusal) -> String {
    let (unit, scale) = match refusal.kind {
        RefusalKind::Cpu => ("m", 1),
        RefusalKind::PredictedMemory | RefusalKind::BookedMemory => ("MiB", 1 << 20),
        RefusalKind::ColdCap => ("cold", 1),
    };
    let (counted, new, limit) = (
        refusal.counted / scale,
        refusal.new / scale,
        refusal.limit / scale,
    );
    let mut text = format!("counted {counted}{unit} + new {new}{unit} > limit {limit}{unit}");
    if refusal.kind == RefusalKind::Cpu {
        let _ = write!(text, " (measured {}m)", refusal.measured_cpu_millis);
    }
    text
}

#[cfg(test)]
#[path = "refusals_tests.rs"]
mod tests;
