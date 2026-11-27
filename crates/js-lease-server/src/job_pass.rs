//! The job pass of the tick: the half of a job's life that only time and host presence drive
//! (design section 6.1, "the one place time acts").
//!
//! [`run`] is one pass over the job and allocation tables, in the order of section 6.1:
//!
//! 1. **End what ran out of time.** First the prepares ([`job_whole::expire_prepares`]): a
//!    placed whole-host job whose host did not report ready by the allocation's prepare
//!    deadline fails its prepare, going back to queued or, after `max_replace` of them,
//!    failing to start with `exclusive-not-ready`, its allocation freed in the same commit
//!    (no container exists to confirm). Then ([`job_expiry::expire`]): a queued job past its
//!    pool's `max_queue_wait` (counted from its submit, or for a later attempt from the end of
//!    its retry's backoff) fails to start with `no-capacity`; a placed or starting job past its
//!    pool's `start_timeout` fails to start with `start-timeout`, or is queued for its next
//!    attempt when its retry policy leaves one ([`crate::job_retry`]), its allocation moving to
//!    releasing in the same commit (the stop rides on each heartbeat reply while
//!    the host lists the job, [`crate::per_job::stops_for`]). Then the retention sweep
//!    deletes each ended job past its retention end with its result
//!    ([`js_jobs::JobStore::tick`]); a job that has not ended is never deleted, and no log
//!    object is touched.
//! 2. **Settle allocations** ([`job_loss::lose_absent`]): each host that holds a live
//!    allocation is read once; a host the provider reads absent has its allocations freed and
//!    its jobs lost, or queued for their next attempt when their retry policy leaves one.
//! 3. **End the jobs of a host silent past the limit** ([`job_loss::lose_silent`]): a host
//!    whose `node/` row was last heard the server's `silence_limit` (30 minutes by default) or
//!    more before `now` has its placed, starting and running jobs ended as lost, with the cause
//!    `host-silent` and no retry, their allocations moved to releasing, not freed. It comes
//!    after the second step, so a host both absent and silent is settled as absent.
//!
//! After the passes, whether they succeeded or not, the tick trims the job store's change feed to its newest
//! `--job-feed-retention` changes ([`crate::job_store::FeedTrim`]); a failed trim is logged
//! and never fails the pass.
//!
//! Presence and marks (`registry.advance()`) and placement follow in the tick
//! ([`crate::api::Shared::tick`]). Every state change is one store transaction carrying the
//! allocation change that goes with it (section 8, row 25). Every deadline the pass judges was
//! stamped by the store's clock, by whichever server wrote it, so the pass judges them by the
//! same clock: `now` is the store's reading ([`js_jobs::JobStore::now`]), taken once per pass,
//! and the duty claims read it too. A pass whose store cannot read its clock decides nothing.
//! The server's own clock only measures how long the pass took.
//!
//! The third step counts a host's silence from the later of its row's last hearing and the
//! server's [`SilenceFloor`]: the store's time when this server's pass first read the store's
//! clock after the server booted or after a pass that failed. A host is never judged silent for
//! a time no server could hear it or the store could not be read: after any such outage each
//! host has the whole limit to be heard again. See [`crate::job_loss`] for why no job runs
//! twice.
//!
//! The pass holds each queue-wait and start deadline to at least the job's deadline floor,
//! which [`js_jobs::JobStore::raise_at_boot`] raises, with the retention ends, to the boot plus
//! 120 s in one transaction when the server opens the job store, before its first tick
//! ([`crate::job_store::open`]): an outage longer than a queue wait, a start timeout or a
//! retention window fails no queued job, ends no start and deletes no result at the first
//! tick. The same transaction replaces by their digests the files' contents that a row of an
//! ended job still holds.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::{JobId, JobStoreError, StartFailure};
use js_registry::{Registry, WholeNodeConfig};
use js_state::StoreTime;
use js_types::Clock;
use js_types::HostId;
use tracing::{info, warn};

use crate::agents::Agents;
use crate::job_expiry::{self, JobLimits};
use crate::job_loss::{self, Observe, Seen};
use crate::job_store::ServerJobs;
use crate::job_whole;
use crate::metrics::MetricsSink;
use crate::sched::place::SchedPlacer;
use crate::store_health::{FailureKind, Pass};

/// The periodic duty of the job pass ([`run`]), as the servers claim it ([`js_jobs::JobStore::claim_duty`]).
const JOB_PASS: &str = "job-pass";

/// The periodic duty of the feed's trim, as the servers claim it.
const FEED_TRIM: &str = "feed-trim";

/// What one pass changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Passed {
    /// The whole-host jobs whose prepare ran out of time: each went back to queued, failed to
    /// start with `exclusive-not-ready`, or, with a cancel intent, was cancelled.
    pub(crate) unprepared: Vec<JobId>,
    /// The jobs that ran out of time, with how each failed to start.
    pub(crate) expired: Vec<(JobId, StartFailure)>,
    /// The ended jobs deleted at their retention end.
    pub(crate) deleted: Vec<JobId>,
    /// The jobs lost because the provider read their host absent.
    pub(crate) lost: Vec<JobId>,
    /// The jobs lost because their host was silent past the server's `silence_limit`.
    pub(crate) silent: Vec<JobId>,
}

impl Passed {
    /// Whether the pass changed nothing.
    fn is_empty(&self) -> bool {
        self.unprepared.is_empty()
            & self.expired.is_empty()
            & self.deleted.is_empty()
            & self.lost.is_empty()
            & self.silent.is_empty()
    }
}

/// One pass over `jobs` at `now`, under `limits` and the whole-node `rules`, reading each
/// host's presence through `observer` and counting each host's silence from no earlier than
/// `floor` (the steps of the module docs), and what it changed. Each start failure and loss is
/// told to `metrics` as its commit lands.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the next pass reads the tables again.
pub(crate) fn run(
    jobs: &ServerJobs,
    (limits, rules): (JobLimits, &WholeNodeConfig),
    (observer, metrics): (&dyn Observe, &dyn MetricsSink),
    (now, floor): (StoreTime, StoreTime),
) -> Result<Passed, JobStoreError> {
    let unprepared = job_whole::expire_prepares(jobs, rules, now, metrics)?;
    let expired = job_expiry::expire(jobs, limits, now, metrics)?;
    let deleted = jobs.tick()?;
    let lost = job_loss::lose_absent(jobs, observer, metrics)?;
    let silence = (limits.silence_limit, floor);
    let silent = job_loss::lose_silent(jobs, silence, now, metrics)?;
    Ok(Passed {
        unprepared,
        expired,
        deleted,
        lost,
        silent,
    })
}

/// Where a server counts a host's silence from (the third step of the module docs): the store's
/// time when its job pass first read the store's clock after the server booted or after a pass
/// that failed; unset until then.
#[derive(Debug, Default)]
pub(crate) struct SilenceFloor(Mutex<Option<StoreTime>>);

impl SilenceFloor {
    /// The floor, set to `now` when it is unset.
    fn at(&self, now: StoreTime) -> StoreTime {
        let mut floor = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *floor.get_or_insert(now)
    }

    /// Unsets the floor, after a pass that failed: the next pass counts from its own reading.
    fn reset(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// What the server's provider reads of a host: absent when the registry removed it
/// ([`crate::sched::place::SchedPlacer::gone`]); present while the registry hears it; unknown
/// while it is silent or the registry does not hold it. Silence is never absence.
struct Presence<'a> {
    registry: &'a Registry,
    gone: &'a BTreeMap<HostId, u64>,
}

impl Observe for Presence<'_> {
    fn observe(&self, host: &HostId) -> Result<Seen, String> {
        let seen = if self.gone.contains_key(host) {
            Seen::Absent
        } else if self.registry.own_host(host).is_some()
            & self.registry.silent_since(host).is_none()
        {
            Seen::Present
        } else {
            Seen::Unknown
        };
        Ok(seen)
    }
}

/// `millis` since the Unix epoch as a reading of the store's clock, which counts from there:
/// for the tests whose server and store read one manual clock. The server itself never turns
/// its own clock into the store's.
#[cfg(test)]
pub(crate) fn store_time(millis: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(millis))
}

impl Agents {
    /// The job pass of this tick ([`run`]) when the server holds the job tables, at the
    /// store's clock (read once, after the duty is claimed), under the pool's limits, with the
    /// hosts the registry removed read absent and each host's silence counted from no earlier
    /// than the server's [`SilenceFloor`]; then those removals are settled. A claim, a clock
    /// reading or a pass that fails unsets the floor. A failed pass
    /// is logged and changes no removal; the next tick passes again. A store that cannot read
    /// its clock fails the claim or the pass before any write: no job is ended or deleted by a
    /// time the store did not give. The pass's duration, the start failures and losses it
    /// made, and a failure of it, are counted on the agent side's meter ([`Agents::meter`]).
    ///
    /// What the pass says of the store, for [`crate::store_health`]: how long it ran on the
    /// server's clock, or the kind of the store's failure; `None` when the pass failed for a
    /// reason that is not the store's. Without the job tables, nothing, and `None`.
    pub(crate) fn job_pass(&self) -> Option<Pass> {
        let Some(jobs) = &self.jobs else {
            return None;
        };
        let gone = self
            .placer
            .as_ref()
            .map(|placer| placer.gone())
            .unwrap_or_default();
        let observer = Presence {
            registry: &self.registry,
            gone: &gone,
        };
        let began = self.clock.now();
        let meter = self.meter();
        let sink = meter.sink();
        // One server runs the pass in each period: the one whose advance of the duty's row
        // commits. The others leave it, having learnt that the store answers. A server whose
        // registry removed a host holds what only its own pass settles (the host is read absent
        // here, and unknown to every other server), so it runs the pass whether or not another
        // server claimed the period: the pass's writes are conditional on the rows it read, so
        // two passes in one period do no wrong, and one that is never run would leave the
        // removed host's jobs unended.
        let claimed = gone.is_empty();
        if claimed {
            match jobs.claim_duty(JOB_PASS, self.duty_period()) {
                Ok(true) => {}
                Ok(false) => return None,
                Err(error) => {
                    self.silence_floor.reset();
                    sink.store_write_failed();
                    warn!(%error, "the job pass could not be claimed; the next tick tries again");
                    return FailureKind::of(&error).map(Pass::Failed);
                }
            }
        }
        let now = match jobs.now() {
            Ok(now) => now,
            Err(error) => {
                self.silence_floor.reset();
                sink.store_write_failed();
                warn!(%error, "the store's clock could not be read; the job pass decides nothing until the next tick");
                return FailureKind::of(&error).map(Pass::Failed);
            }
        };
        let rules = self.whole_rules();
        let limits = (self.job_limits, &rules);
        let times = (now, self.silence_floor.at(now));
        let (passed, took) = meter.timed(|| run(jobs, limits, (&observer, sink), times));
        sink.job_pass_took(took);
        let ran = Duration::from_millis(self.clock.now().millis().saturating_sub(began.millis()));
        let seen = match passed {
            Ok(passed) => {
                if !passed.is_empty() {
                    info!(
                        unprepared = passed.unprepared.len(),
                        expired = passed.expired.len(),
                        deleted = passed.deleted.len(),
                        lost = passed.lost.len(),
                        silent = passed.silent.len(),
                        "the job pass ended or deleted jobs"
                    );
                }
                let settle = |placer: &Arc<SchedPlacer>| placer.settle_gone(&gone);
                self.placer.iter().for_each(settle);
                // Only the server that claimed the period forgets rows: one per period.
                if claimed {
                    self.forget_silent_rows(jobs);
                }
                Some(Pass::Ran(ran))
            }
            Err(error) => {
                self.silence_floor.reset();
                sink.store_write_failed();
                warn!(%error, "the job pass failed; the next tick passes again");
                FailureKind::of(&error).map(Pass::Failed)
            }
        };
        self.trim_feed();
        seen
    }

    /// Forgets the `node/` rows of the hosts silent for longer than the registry's
    /// [`Registry::forget_after`] that hold nothing ([`js_jobs::JobStore::forget_silent_nodes`]),
    /// so a pool's cap counts only rows not forgotten. The hosts forgotten are logged; a store
    /// that fails is logged, and the next period forgets them.
    fn forget_silent_rows(&self, jobs: &ServerJobs) {
        match jobs.forget_silent_nodes(self.registry.forget_after()) {
            Ok(forgotten) => {
                if !forgotten.is_empty() {
                    let hosts: Vec<&str> = forgotten.iter().map(HostId::as_str).collect();
                    info!(hosts = ?hosts, "the rows of silent hosts were forgotten");
                }
            }
            Err(error) => {
                warn!(%error, "silent hosts' rows could not be forgotten; the next period tries again")
            }
        }
    }

    /// Trims the job store's change feed to the newest `--job-feed-retention` changes, and
    /// logs how many rows went, when this server claims the duty for the period that begins
    /// now by the store's clock. A store that fails is logged; the next tick trims again.
    fn trim_feed(&self) {
        let (Some(feed), Some(jobs)) = (&self.job_feed, &self.jobs) else {
            return;
        };
        match jobs.claim_duty(FEED_TRIM, self.duty_period()) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                warn!(%error, "the job store's change feed trim could not be claimed");
                return;
            }
        }
        match feed.pass() {
            Ok(0) => {}
            Ok(trimmed) => info!(trimmed, "the job store's change feed was trimmed"),
            Err(error) => warn!(%error, "trimming the job store's change feed failed"),
        }
    }
}

#[cfg(test)]
#[path = "job_pass_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "job_pass_clock_tests.rs"]
mod clock_tests;

#[cfg(test)]
#[path = "job_pass_forget_tests.rs"]
mod forget_tests;

#[cfg(test)]
#[path = "job_pass_silence_tests.rs"]
mod silence_tests;
