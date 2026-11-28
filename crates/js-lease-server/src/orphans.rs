//! The jobs a host lists that the store holds no record of (its orphans), and the guard
//! against a store that lost its tables (design section 8, rows 8 and 37).
//!
//! A listed job the store holds no live allocation of on the host is judged by what the store
//! records of it ([`crate::per_job`]): its end, its cancel, or the end of the host's attempt
//! and a later one is a reason to stop it, sent with the stop. Nothing recorded is no reason:
//! a server that came up on an empty, restored or older table, on a wrong database or on a
//! replica that is behind would otherwise tell every host to stop every job it runs within
//! one heartbeat. So such a job is an orphan of the host, not a stop:
//!
//! - **Named and kept.** The reply names it in `unknown_jobs` and the host keeps running it.
//!   The host's row keeps it with the time a beat first listed it ([`js_jobs::Orphan`]), so
//!   its age is the same on every server over the store and goes on across a hello. The host
//!   takes no job while it lists one (its listing is not clean), so the work the store does
//!   not count is never booked over. A job that has ended on the host (its result is listed)
//!   is no orphan: its result is answered stale, and the host drops it.
//! - **Ended only after a bound, one at a time.** The orphan listed longest, once listed for
//!   longer than the most any job may run (`--max-job-wall`, [`JobLimits::orphan_after`]), is
//!   told to stop with [`StopReason::OrphanExpired`], one orphan of a host at a time, so a
//!   whole host's work never ends in one beat. A job that runs past the most a job may run
//!   holds no claim the store could have given it; every agent ends a job at its own wall
//!   limit first, so the bound is a backstop.
//! - **An operator's verb.** An operator who knows the store is right ends an orphan at once
//!   (`EndOrphan`): the host's row marks it, and every beat after tells the host to stop it
//!   with [`StopReason::Operator`] until it is no longer listed.
//! - **The guard.** When more than half of what a host lists is orphans (jobs unknown to the
//!   store that still run there), the store is more likely wrong than the host: the server
//!   sends that host no stop at all
//!   (an operator's excepted), counts the beat (`js_unknown_listing_guarded_total`) and logs
//!   an error the first time the beat finds orphans. Half is the line because no healthy
//!   store gives it: an orphan is the rare job lost by a partition or a store fault, so a
//!   host whose listing is mostly orphans is the sign of a table lost or rolled back, where
//!   even the records the store does hold (a cancel, an end) may be older than the host's
//!   work. A host whose listing is mostly known still has its records acted on. Under the guard
//!   no orphan is ended at the bound either: it ends at its wall limit on the agent, or when an
//!   operator ends it. A host with one job, an orphan, is guarded too: a lone host cannot tell a
//!   lost table from a lost job, and holding is the side that loses no work.
//!
//! What the guard does not reach: a stop the dispatch queue carries (written when a server
//! decided an end, so a record of its own), and the registry's `Cancel` of the runs an agent of
//! a previous release lists by run id (`js_registry`, which holds no run at all).

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::StopReason;
use js_jobs::{JobStoreError, Orphan};
use js_state::StoreTime;
use js_types::HostId;
use tracing::{error, warn};

use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;

/// What a heartbeat's listing decided beside the store's live allocations: the stops of
/// listed jobs the host holds no live allocation for, each with its reason, and whether the
/// guard holds every other stop of the host. The default decides nothing: a reply built again
/// from the rows alone (a held reply, one to a beat a newer one overtook) sends only what
/// the live allocations say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Judged {
    /// The listed jobs the host is told to stop that it holds no live allocation for, by job
    /// id: each one the store records a reason for, and the orphan ended now, if any.
    pub(crate) stops: BTreeMap<String, StopReason>,
    /// Whether more than half of the host's listing is orphans: then the host is told to stop
    /// nothing but the orphans an operator ended.
    pub(crate) guarded: bool,
}

/// What [`judge`] reads of a listing besides the host's orphans.
pub(crate) struct Listing<'a> {
    /// How many jobs the host listed.
    pub(crate) listed: usize,
    /// The listed jobs the store holds no live allocation of on the host and records a
    /// reason to stop, each with it.
    pub(crate) stopped: &'a BTreeMap<String, StopReason>,
}

/// The orphans of the host the listing came from, with `before` the ones its row held: every
/// listed job of `unknown` that has not ended on the host, with the time a beat first listed
/// it (`before`'s, or the store's time now for one first listed by this beat) and whether an
/// operator asked to end it. An orphan no longer listed is dropped. The store's clock is read
/// only when the listing has an orphan; `None` when it had none.
///
/// # Errors
/// The [`JobStoreError`] of the read of the store's clock.
pub(crate) fn held(
    jobs: &ServerJobs,
    before: &BTreeMap<String, Orphan>,
    unknown: &BTreeMap<String, bool>,
) -> Result<(BTreeMap<String, Orphan>, Option<StoreTime>), JobStoreError> {
    let running: Vec<&String> = unknown
        .iter()
        .filter(|(_, ended)| !**ended)
        .map(|(job, _)| job)
        .collect();
    if running.is_empty() {
        return Ok((BTreeMap::new(), None));
    }
    let now = jobs.now()?;
    let orphans = running.into_iter().map(|job| {
        let orphan = before.get(job).copied().unwrap_or(Orphan {
            first_seen: now,
            end_asked: false,
        });
        (job.clone(), orphan)
    });
    Ok((orphans.collect(), Some(now)))
}

/// What the listing of the host `host` decided ([`Judged`]), with `orphans` its orphans as
/// [`held`] gave them, read at `now`, and `before` the ones its row held: the guard is up when
/// more than half of the listing is orphans; the stops are then the orphans an operator ended
/// alone, and otherwise those, every job of `listing.stopped`, and the orphan listed longest
/// once it is past [`JobLimits::orphan_after`] (one at a time). The orphans this beat found
/// first are counted and logged, and a beat the guard holds is counted.
pub(crate) fn judge(
    (host, metrics): (&HostId, &dyn MetricsSink),
    limits: &JobLimits,
    listing: &Listing<'_>,
    (orphans, before, now): (
        &BTreeMap<String, Orphan>,
        &BTreeMap<String, Orphan>,
        Option<StoreTime>,
    ),
) -> Judged {
    let guarded = orphans.len() * 2 > listing.listed;
    let found: Vec<&String> = orphans
        .keys()
        .filter(|job| !before.contains_key(*job))
        .collect();
    if !found.is_empty() {
        metrics.unknown_jobs_found(host, found.len());
        warn!(%host, jobs = ?found, "the host lists jobs the store holds no record of: they are kept running as orphans, and the host takes no job while it lists them");
    }
    if guarded {
        metrics.unknown_listing_guarded(host);
        if !found.is_empty() {
            let (orphans, listed) = (orphans.len(), listing.listed);
            error!(%host, orphans, listed, "more than half of what the host lists is unknown to the store: the store may have lost or rolled back its tables, and the host is told to stop nothing until an operator looks");
        }
    }
    let ended = orphans
        .iter()
        .filter(|(_, orphan)| orphan.end_asked)
        .map(|(job, _)| (job.clone(), StopReason::Operator));
    let mut stops: BTreeMap<String, StopReason> = ended.collect();
    if guarded {
        return Judged { stops, guarded };
    }
    stops.extend(listing.stopped.iter().map(|(job, why)| (job.clone(), *why)));
    if let Some(job) = now.and_then(|now| expired(orphans, now, limits.orphan_after)) {
        stops.insert(job, StopReason::OrphanExpired);
    }
    Judged { stops, guarded }
}

/// The orphan of `orphans` listed longest (the lowest id among those first listed together)
/// when it has been listed for `after` or longer at `now` and no operator ended it; `None`
/// otherwise.
fn expired(orphans: &BTreeMap<String, Orphan>, now: StoreTime, after: Duration) -> Option<String> {
    let waiting = orphans.iter().filter(|(_, orphan)| !orphan.end_asked);
    let oldest = waiting.min_by_key(|(job, orphan)| (orphan.first_seen, *job));
    oldest
        .filter(|(_, orphan)| now.since(orphan.first_seen) >= after)
        .map(|(job, _)| job.clone())
}

#[cfg(test)]
#[path = "orphans_tests.rs"]
mod tests;
