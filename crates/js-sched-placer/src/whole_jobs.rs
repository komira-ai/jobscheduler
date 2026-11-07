//! The queued jobs that need the whole host, queued, marked and committed by the engine
//! under `--placement js-sched` (design sections 5.4, 6.2 and 6.7).
//!
//! A queued whole-host job is an ordinary row of the job table, and the job table is the one
//! record of it: the engine keeps a scheduler operation for each, which is a cache of the
//! table. A pass over the tables ([`Engine::whole_jobs`], called by the placer's pass) hands
//! the engine the queued jobs, the stored marks and the hosts that said hello, and the engine
//! answers the writes the pass makes, in order ([`WholeJobStep`]); the caller commits each and
//! tells the engine how the commit went ([`Engine::whole_job_placed`],
//! [`Engine::whole_job_failed`]). The pass:
//!
//! 1. Takes the pool's whole-node settings when the registry's own round has not set them.
//!    With none, only step 2 runs.
//! 2. Unmarks every stored mark whose job is not a queued whole-host job (the table holds
//!    none in normal operation: a cancel, a placement and a failed prepare delete the mark in
//!    the commit that writes the job).
//! 3. Cancels the operation of every job that is no longer a queued whole-host job, and of
//!    one the scheduler finished.
//! 4. Queues each queued whole-host job the engine does not queue yet, as a whole-machine
//!    request held to the time the job was submitted, with the bars its failed prepares set
//!    and the mark the table holds for it. A mark is taken back only after its host said
//!    hello to this server; one whose host has not said hello waits for it until `claim_by`
//!    (the server's start plus `regain_grace`), its row standing meanwhile and the job not
//!    queued; after `claim_by` the mark is deleted and the job is queued without it, to be
//!    marked again. The deadline of a stored mark was raised at boot by the store
//!    ([`js_jobs::JobStore::raise_at_boot`]): the engine takes it as it is stored.
//! 5. Runs one placement round of the queue, in the term of the whole-node leases.
//! 6. Reads the round, for each job in queue order: a job granted a quiet host that may take
//!    a job now is committed there; a job granted any other host keeps the grant; a job with a
//!    mark has it written when it differs from the stored one (at millisecond precision, so an
//!    unchanged mark is never rewritten); a job with no mark and a stored mark has the mark
//!    deleted.
//!
//! A failed prepare of a job is decided by [`failed_prepare`] with the lease path's rules (the
//! registry's table of causes and `max_replace`); the count and the bars it gives are written
//! to the job's row with the requeue, so a restart loses neither. A job placed by backfill on
//! a host a whole-host job marks has its wall limit cut to the time left in the mark's tail
//! ([`backfill_wall`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_jobs::{Booked, Exclusion, FailedPrepare, JobId, JobRecord, MarkRecord, Needs};
use js_lease::Timestamp;
use js_registry::{WholeNodeConfig, prepare_exclusion};
use js_sched::{Bar, Event, Request, RestoredMark, RestoredWhole};
use js_state::StoreTime;
use js_types::HostId;
use js_types::sched::{
    FarmTime, LeaseKind, OperationId, Qos, Resources, WaiterId, WorkKey, WorkerId,
};
use js_whole::cost::DEFAULT_BACKFILL_MARGIN_MS;

use super::prepare::{bar_of, stronger};
use super::{Engine, Placed};
use crate::map;

/// What a pass hands the engine for the queued whole-host jobs.
#[derive(Debug, Clone, Copy)]
pub struct WholeJobs<'a> {
    /// Every queued job, in queue order ([`js_jobs::JobStore::queued`]); jobs that do not need
    /// the whole host are passed over.
    pub queued: &'a [JobRecord],
    /// Every stored mark ([`js_jobs::JobStore::marks`]).
    pub marks: &'a [MarkRecord],
    /// The hosts that said hello to this server (a session since its start).
    pub hello: &'a BTreeSet<HostId>,
    /// The hosts that may take a job now (the pass's open hosts).
    pub open: &'a BTreeSet<HostId>,
    /// The pool's whole-node settings; `None`: the pool has none, and no whole-host job is
    /// placed.
    pub settings: Option<&'a WholeNodeConfig>,
    /// Until when a mark the store held at this server's start waits for its host's hello:
    /// the server's start plus `regain_grace_ms` (design section 6.7, step 3).
    pub claim_by: FarmTime,
}

/// What the pass writes for one whole-host job, in the order the engine decides them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WholeJobStep {
    /// Write this mark ([`js_jobs::JobStore::put_mark`]): made, moved, or its deadline, tail or
    /// exclusions changed.
    Mark(MarkRecord),
    /// Delete the job's mark ([`js_jobs::JobStore::delete_mark`]).
    Unmark(JobId),
    /// Commit the job placed on the host with the whole host booked
    /// ([`js_jobs::JobStore::place_with`]).
    Commit {
        /// The job.
        job: JobId,
        /// The host its grant is on.
        host: HostId,
    },
}

/// Why a whole-host prepare failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareCause {
    /// The agent refused it with this code (`exclusive_refused`).
    Refused(RefusalReason),
    /// No ready report came by the allocation's prepare deadline.
    Timeout,
}

impl Engine {
    /// One pass of the whole-host jobs at `now` (see the module documentation): the writes it
    /// makes, in order.
    pub fn whole_jobs(&mut self, input: &WholeJobs<'_>, now: FarmTime) -> Vec<WholeJobStep> {
        // The registry's own round, when it has run, set the settings.
        let rounded = self.round.is_some();
        if let Some(settings) = input.settings.filter(|_| !rounded) {
            self.take_settings(settings);
        }
        let queue: Vec<&JobRecord> = input
            .queued
            .iter()
            .filter(|job| job.spec.needs == Needs::WholeHost)
            .collect();
        let wanted: BTreeSet<&JobId> = queue.iter().map(|job| &job.id).collect();
        let marks = input.marks.iter();
        let unwanted = marks.filter(|mark| !wanted.contains(&mark.job));
        let mut steps: Vec<WholeJobStep> = unwanted.map(|mark| unmark(&mark.job)).collect();
        if self.whole_settings.is_none() {
            return steps;
        }
        self.cancel_jobs(&wanted);
        steps.extend(self.queue_jobs(input, &queue, now));
        self.whole_tick(now);
        steps.extend(self.decide_jobs(input, &queue, now));
        steps
    }

    /// The commit of `job` on `host` landed: its pending grant becomes the job's hold, booking
    /// the whole host and the host ports `host_ports` the allocation holds, and the job leaves
    /// the queue. A job whose grant is gone or is on another host (a host the registry forgot
    /// since the pass decided) leaves the queue without a hold; the next pass takes the
    /// allocation up from the table. A job the engine does not queue changes nothing.
    pub fn whole_job_placed(&mut self, job: &JobId, host: &HostId, host_ports: Vec<u16>) {
        let Some(operation) = self.whole_jobs.remove(job) else {
            return;
        };
        let worker = map::worker(host);
        match self.pending(operation).filter(|(_, on)| *on == worker) {
            Some((lease, _)) => {
                let placed = Placed { lease, operation };
                self.hold_job(job, host, (Booked::WholeHost, host_ports), placed);
            }
            None => {
                let _ = self.sched.cancel_whole(operation);
            }
        }
    }

    /// The commit of `job` failed: its grant is put back and it stays queued, keeping its queue
    /// time. A job with no grant, or one the engine does not queue, changes nothing.
    pub fn whole_job_failed(&mut self, job: &JobId) {
        let operation = self.whole_jobs.get(job).copied();
        let grant = operation.and_then(|operation| self.pending(operation));
        if let Some((lease, _)) = grant {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// Cancels and forgets the operation of every job that is not in `wanted`, the queued
    /// whole-host jobs, and of every one the scheduler no longer holds unfinished: the pass
    /// queues that job again.
    fn cancel_jobs(&mut self, wanted: &BTreeSet<&JobId>) {
        let sched = &mut self.sched;
        self.whole_jobs.retain(|job, operation| {
            let state = sched.state(*operation);
            let live = state.is_some_and(|state| !state.is_done());
            let keep = wanted.contains(job) & live;
            if !keep {
                let _ = sched.cancel_whole(*operation);
            }
            keep
        });
    }

    /// Queues each of `queue` the engine does not queue yet (step 4 of the module
    /// documentation) and answers the marks it deletes.
    fn queue_jobs(
        &mut self,
        input: &WholeJobs<'_>,
        queue: &[&JobRecord],
        now: FarmTime,
    ) -> Vec<WholeJobStep> {
        let stored = stored_marks(input);
        let mut steps = Vec::new();
        for job in queue {
            if self.whole_jobs.contains_key(&job.id) {
                continue;
            }
            let held = match stored.get(&job.id).copied() {
                Some(mark) if input.hello.contains(&mark.host) => Some(mark),
                Some(_) if now < input.claim_by => continue,
                Some(_) => {
                    steps.push(unmark(&job.id));
                    None
                }
                None => None,
            };
            let operation = self.submit_job(job, held, now);
            self.whole_jobs.insert(job.id.clone(), operation);
        }
        steps
    }

    /// Queues `job`, a whole-host job, at the back of the scheduler's queue as a whole-machine
    /// request held to the time it was submitted, with the bars its failed
    /// prepares set and those of `mark`, and `mark` itself: its operation.
    fn submit_job(
        &mut self,
        job: &JobRecord,
        mark: Option<&MarkRecord>,
        now: FarmTime,
    ) -> OperationId {
        let operation = self.sched.next_operation();
        let request = Request {
            key: WorkKey::new("job", job.id.as_str()),
            qos: Qos::Batch,
            kind: LeaseKind::WholeMachine,
            // Every host is booked whole, and the pass queues only a job the pool's hosts
            // can run: any worker fits it.
            resources: Resources::new(0, 0),
            needs: js_caps::Request::default(),
            hermetic: false,
            do_not_cache: true,
            whole_worker: false,
            pin: None,
        };
        let submit = Event::Submit {
            waiter: WaiterId(operation.0),
            request,
        };
        self.feed(now, submit);
        let wall = Duration::from_secs(job.spec.wall_limit_secs);
        self.sched.set_max_wall(operation, wall);
        let bars = bars_of(job, mark);
        let queued_at = farm_of(job.submitted_at);
        let restored = |mark| RestoredWhole {
            operation,
            queued_at,
            bars: bars.clone(),
            mark,
        };
        let mark = mark.map(|mark| RestoredMark {
            worker: map::worker(&mark.host),
            made: farm_of(mark.made),
            deadline: farm_of(mark.deadline),
            tail_end: mark.tail_end.map(farm_of),
        });
        // A mark the scheduler cannot hold (its host is not a worker, or another request
        // marks it) is let go: the pass deletes it and the job marks again.
        if self.sched.restore_whole(restored(mark)).is_err() {
            let _ = self.sched.restore_whole(restored(None));
        }
        operation
    }

    /// What the round decided for each job of `queue` the engine queues (step 6 of the module
    /// documentation).
    fn decide_jobs(
        &self,
        input: &WholeJobs<'_>,
        queue: &[&JobRecord],
        now: FarmTime,
    ) -> Vec<WholeJobStep> {
        let stored = stored_marks(input);
        let mut steps = Vec::new();
        for job in queue {
            // A job whose stored mark waits for its host's hello is not queued.
            let Some(operation) = self.whole_jobs.get(&job.id).copied() else {
                continue;
            };
            if let Some((_, worker)) = self.pending(operation) {
                let host = map::host_id(&worker).ok();
                let host = host.filter(|host| input.open.contains(host));
                let commit = host.map(|host| WholeJobStep::Commit {
                    job: job.id.clone(),
                    host,
                });
                steps.extend(commit);
                continue;
            }
            let mark = self.job_mark(&job.id, operation, now);
            steps.extend(mark_step(&job.id, mark, stored.get(&job.id).copied()));
        }
        steps
    }

    /// The mark `operation`, the queued operation of `job`, holds now, as the table keeps it:
    /// the host, when it was made, its deadline and tail end, and the hosts the operation is
    /// barred from at `now`.
    fn job_mark(&self, job: &JobId, operation: OperationId, now: FarmTime) -> Option<MarkRecord> {
        let mark = self.sched.whole_mark(operation)?;
        // A worker is named for a host, so its name is a host id.
        let host = map::host_id(&mark.worker).ok();
        host.map(|host| {
            let made = store_of(mark.made);
            let mut record = MarkRecord::new(job.clone(), host, made, store_of(mark.deadline));
            record.tail_end = mark.tail_end.map(store_of);
            let barred = self.sched.whole_barred(operation, now);
            record.exclusions = barred.into_iter().filter_map(excluded).collect();
            record
        })
    }
}

/// The write that deletes the mark of `job`.
fn unmark(job: &JobId) -> WholeJobStep {
    WholeJobStep::Unmark(job.clone())
}

/// The write the round's `mark` of `job` calls for, given the `stored` one: the mark when it
/// differs (or there is none stored), the delete of the stored one when the round holds none,
/// and nothing when they agree.
fn mark_step(
    job: &JobId,
    mark: Option<MarkRecord>,
    stored: Option<&MarkRecord>,
) -> Option<WholeJobStep> {
    match (mark, stored) {
        (Some(mark), Some(stored)) if reading(stored) == reading(&mark) => None,
        (Some(mark), _) => Some(WholeJobStep::Mark(mark)),
        (None, Some(_)) => Some(unmark(job)),
        (None, None) => None,
    }
}

/// The host and the exclusion of `bar` on `worker`, as the table keeps them.
fn excluded((worker, bar): (WorkerId, Bar)) -> Option<(HostId, Exclusion)> {
    let host = map::host_id(&worker).ok();
    host.map(|host| (host, exclusion_of_bar(bar)))
}

/// The stored marks of `input`, by job.
fn stored_marks<'a>(input: &WholeJobs<'a>) -> BTreeMap<&'a JobId, &'a MarkRecord> {
    input.marks.iter().map(|mark| (&mark.job, mark)).collect()
}

/// A mark as the pass compares it: store times at the millisecond the scheduler counts in,
/// the exclusions as bars.
#[derive(PartialEq)]
struct Reading<'a> {
    host: &'a HostId,
    made: FarmTime,
    deadline: FarmTime,
    tail_end: Option<FarmTime>,
    bars: BTreeMap<WorkerId, Bar>,
}

/// `mark` as the pass compares it.
fn reading(mark: &MarkRecord) -> Reading<'_> {
    Reading {
        host: &mark.host,
        made: farm_of(mark.made),
        deadline: farm_of(mark.deadline),
        tail_end: mark.tail_end.map(farm_of),
        bars: bars_of_exclusions(&mark.exclusions),
    }
}

/// The bars of `exclusions`, by worker.
fn bars_of_exclusions(exclusions: &BTreeMap<HostId, Exclusion>) -> BTreeMap<WorkerId, Bar> {
    let mut bars = BTreeMap::new();
    for (host, exclusion) in exclusions {
        bars.insert(map::worker(host), bar_of_exclusion(*exclusion));
    }
    bars
}

/// The hosts `job` is kept off, as bars: those its failed prepares set and those of `mark`,
/// the stronger of two bars on one host.
fn bars_of(job: &JobRecord, mark: Option<&MarkRecord>) -> BTreeMap<WorkerId, Bar> {
    let marked = mark.into_iter().flat_map(|mark| mark.exclusions.iter());
    let mut bars = BTreeMap::new();
    for (host, exclusion) in job.prepare_bars.iter().chain(marked) {
        let worker = map::worker(host);
        let kept = bars.get(&worker).copied();
        bars.insert(worker, stronger(kept, bar_of_exclusion(*exclusion)));
    }
    bars
}

/// The bar of `exclusion`: the kinds are the scheduler's, with the ends on the store's clock.
fn bar_of_exclusion(exclusion: Exclusion) -> Bar {
    match exclusion {
        Exclusion::Backoff(end) => Bar::Backoff(farm_of(end)),
        Exclusion::Until(end) => Bar::Until(farm_of(end)),
        Exclusion::ForLife => Bar::ForLife,
    }
}

/// The exclusion `bar` is kept as in the table.
fn exclusion_of_bar(bar: Bar) -> Exclusion {
    match bar {
        Bar::Backoff(end) => Exclusion::Backoff(store_of(end)),
        Bar::Until(end) => Exclusion::Until(store_of(end)),
        Bar::ForLife => Exclusion::ForLife,
    }
}

/// `at` on the scheduler's clock: the store's and the scheduler's both count milliseconds
/// from the Unix epoch in the server.
fn farm_of(at: StoreTime) -> FarmTime {
    let millis = u64::try_from(at.as_duration().as_millis());
    FarmTime::from_millis(millis.unwrap_or(u64::MAX))
}

/// `at` on the store's clock.
fn store_of(at: FarmTime) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(at.as_millis()))
}

/// How the failed prepare of `job` on `host` at `now` ends under `settings` (the lease path's
/// rules, [`super::prepare`]): the job's failed prepares plus this one; the bars the job keeps
/// are its own with the host's exclusion merged in, the registry's table of causes
/// ([`prepare_exclusion`], a back-off ending `reserve_backoff` after `now`) taking the
/// stronger bar of the two on the host; and the job is terminal when the count reaches
/// `max_replace`. The flag says whether the cause taints the host (the table gives it no
/// exclusion): the caller taints it.
pub fn failed_prepare(
    job: &JobRecord,
    host: &HostId,
    cause: PrepareCause,
    settings: &WholeNodeConfig,
    now: StoreTime,
) -> (FailedPrepare, bool) {
    let failed_prepares = job.failed_prepares.saturating_add(1);
    let refused = match cause {
        PrepareCause::Refused(reason) => Some(reason),
        PrepareCause::Timeout => None,
    };
    let now_ms = farm_of(now).as_millis();
    let backoff = now_ms.saturating_add(settings.reserve_backoff_ms);
    let exclusion = prepare_exclusion(refused, Timestamp::from_millis(backoff));
    let bar = exclusion.map(bar_of);
    let mut prepare_bars = job.prepare_bars.clone();
    if let Some(bar) = bar {
        let kept = prepare_bars.get(host).map(|kept| bar_of_exclusion(*kept));
        prepare_bars.insert(host.clone(), exclusion_of_bar(stronger(kept, bar)));
    }
    let failed = FailedPrepare {
        failed_prepares,
        prepare_bars,
        terminal: failed_prepares >= settings.max_replace,
    };
    (failed, bar.is_none())
}

/// The wall limit of a job placed by backfill on a host marked with a tail ending at
/// `tail_end`, at `now`: `requested`, cut to the time left in the tail less the backfill
/// margin ([`DEFAULT_BACKFILL_MARGIN_MS`]), never longer; zero when less than the margin is
/// left.
pub fn backfill_wall(requested: Duration, tail_end: FarmTime, now: FarmTime) -> Duration {
    let left = tail_end.saturating_duration_since(now);
    let margin = Duration::from_millis(DEFAULT_BACKFILL_MARGIN_MS);
    requested.min(left.saturating_sub(margin))
}

#[cfg(test)]
#[path = "whole_jobs_tests.rs"]
mod tests;
