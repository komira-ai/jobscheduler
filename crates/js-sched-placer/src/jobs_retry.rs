//! The queued jobs a pass may place now, and the hosts each may go to, for a job retried after
//! an attempt that ended in a way that retries (`docs/design/job-retry-and-completion-callback.md`,
//! sections 3.4 to 3.6).
//!
//! A pass offers the engine a queued job ([`placeable`]) only when:
//!
//! - its backoff has ended: the time its retry made its next attempt placeable
//!   ([`js_jobs::AttemptTrail::retry_not_before`], on the store's clock) is at or before the
//!   pass's time; a job never retried has none;
//! - the allocation of its previous attempt is freed: the job holds no live (booked or
//!   releasing) allocation, so a job keeps one allocation at a time and the store's placement,
//!   which refuses a job whose allocation is not freed, is never tried and failed on every
//!   pass. The allocation is freed when its host confirms the container gone or the provider
//!   reads the host absent; a retry whose previous host went silent waits, at most until its
//!   queue wait ends.
//!
//! A job held back is not walked and not queued for a whole host, so it takes no room and
//! keeps no host from the jobs behind it, and the pass writes no status for it: it keeps the
//! one its retry wrote (`WAITING_FOR_RETRY`).
//!
//! A host that held one of the job's earlier attempts ([`js_jobs::AttemptTrail::hosts`]) takes a
//! later one only if its agent reports `start-fence` ([`OpenHost::start_fence`]): an agent
//! without it may judge a start by its job id alone and drop the later attempt's start as one
//! it already ran. The job is offered with each such host that does not report it (or is not
//! open now, whose agent the pass cannot read) barred for life, beside the bars its row holds
//! ([`js_jobs::JobRecord::prepare_bars`], among them the 15-minute bar of the host of the
//! attempt its retry ended, which the walk honours as any other). The bar is the pass's own:
//! the row is not written.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationRecord, Exclusion, JobId, JobRecord};
use js_types::HostId;
use js_types::sched::FarmTime;

use super::whole_jobs::farm_of;
use crate::room::OpenHost;

/// The jobs of `queued`, in their order, that a pass at `now` may place, over `live` (every
/// live allocation) and `open` (the hosts that may take a job now): each whose backoff has
/// ended and whose previous allocation is freed, with each host that held an earlier attempt
/// and does not report `start-fence` barred for life (see the module docs). `queued` itself,
/// unchanged and not copied, when it holds no job that is held back or barred so: a job never
/// retried is neither.
pub(super) fn placeable<'a>(
    queued: &'a [JobRecord],
    live: &[AllocationRecord],
    open: &BTreeMap<HostId, OpenHost>,
    now: FarmTime,
) -> Cow<'a, [JobRecord]> {
    let held: BTreeSet<&JobId> = live.iter().map(|allocation| &allocation.job).collect();
    let waiting = |job: &JobRecord| {
        let backoff = job.attempt.retry_not_before();
        backoff.is_some_and(|not_before| farm_of(not_before) > now) | held.contains(&job.id)
    };
    let fenced = |host: &HostId| open.get(host).is_some_and(|offer| offer.start_fence);
    let unfenced = |job: &JobRecord| -> Vec<HostId> {
        let hosts = job.attempt.hosts().iter();
        hosts.filter(|&host| !fenced(host)).cloned().collect()
    };
    let changed = |job: &JobRecord| waiting(job) | !unfenced(job).is_empty();
    if !queued.iter().any(changed) {
        return Cow::Borrowed(queued);
    }
    let ready = queued.iter().filter(|job| !waiting(job)).map(|job| {
        let mut job = job.clone();
        for host in unfenced(&job) {
            job.prepare_bars.insert(host, Exclusion::ForLife);
        }
        job
    });
    Cow::Owned(ready.collect())
}

#[cfg(test)]
#[path = "jobs_retry_tests.rs"]
mod tests;
