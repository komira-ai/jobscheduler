//! The two ways a job becomes lost, alone, over the job tables on a manual clock.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::{INJECTED, Interfere};
use js_jobs::{
    AllocationStep, EndReason, JobEnd, JobId, JobState, JobStore, JobStoreError, LostCause, NewJob,
    NodeHello, Output, Retention, StartFailure,
};
use js_state::fake::Fake;
use js_state::{ManualClock, Store, StoreError, StoreTime};
use js_types::HostId;

use super::{Observe, RowRead, Seen, lose_absent, lose_silent, lose_unlisted};
use crate::agent_api::job_rig::{BOOKED, spec, tables_on};
use crate::job_store::{ServerIds, ServerJobs};
use crate::metrics::NoMetrics;

/// A provider whose reading of each host the test sets: unknown unless set.
struct Provider(BTreeMap<&'static str, Result<Seen, String>>);

impl Observe for Provider {
    fn observe(&self, host: &HostId, _: &RowRead) -> Result<Seen, String> {
        let read = self.0.get(host.as_str()).cloned();
        read.unwrap_or(Ok(Seen::Unknown))
    }
}

fn tables() -> (Arc<ManualClock>, ServerJobs) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(3600));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    (clock, jobs)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The job `text` placed on `on`, then moved to `state` (placed, starting or running).
fn at(jobs: &ServerJobs, text: &str, on: &str, state: JobState) -> JobId {
    let id = JobId::parse(text).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    jobs.place(&id, host(on), BOOKED).unwrap();
    let steps = [
        (JobState::Placed, JobState::Starting),
        (JobState::Starting, JobState::Running),
    ];
    for (from, to) in steps {
        if jobs.get_job(&id).unwrap().unwrap().state != state {
            jobs.transition(&id, from, to).unwrap();
        }
    }
    id
}

fn states(jobs: &ServerJobs, id: &JobId) -> (JobState, AllocationStep, Option<EndReason>) {
    let allocation = jobs.get_allocation(id).unwrap().unwrap();
    let state = jobs.get_job(id).unwrap().map(|job| job.state);
    (
        state.unwrap_or(JobState::Lost),
        allocation.step,
        allocation.end_reason,
    )
}

// Catches a free on an unknown or failed read, or on a host merely present; a free of a host
// that was not read absent; a cancel intent turned into a cancel; an ended job's end replaced;
// and an allocation left booked under a job whose row is gone.
#[test]
fn only_an_absent_host_loses_its_jobs_and_frees_its_allocations() {
    let (clock, jobs) = tables();
    let running = at(&jobs, "job-run", "host-a", JobState::Running);
    let cancelled = at(&jobs, "job-cancel", "host-a", JobState::Running);
    jobs.request_cancel(&cancelled).unwrap();
    let swept = at(&jobs, "job-swept", "host-a", JobState::Running);
    let exited = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    jobs.end_job(&swept, exited).unwrap();
    clock.advance(Retention::DEFAULT.window());
    assert_eq!(jobs.tick().unwrap(), std::slice::from_ref(&swept));
    let timed_out = at(&jobs, "job-timeout", "host-a", JobState::Starting);
    jobs.end_job(
        &timed_out,
        JobEnd::FailedToStart(StartFailure::StartTimeout),
    )
    .unwrap();
    let elsewhere = at(&jobs, "job-b", "host-b", JobState::Running);

    for seen in [
        Ok(Seen::Present),
        Ok(Seen::Unknown),
        Err("failed".to_owned()),
    ] {
        let provider = Provider(BTreeMap::from([
            ("host-a", seen),
            ("host-b", Ok(Seen::Present)),
        ]));
        assert_eq!(
            lose_absent(&jobs, &provider, &NoMetrics).unwrap(),
            Vec::<JobId>::new()
        );
        assert_eq!(states(&jobs, &running).1, AllocationStep::Booked);
    }

    let provider = Provider(BTreeMap::from([("host-a", Ok(Seen::Absent))]));
    let lost = lose_absent(&jobs, &provider, &NoMetrics).unwrap();
    assert_eq!(lost, [cancelled.clone(), running.clone()]);
    let freed = (AllocationStep::Freed, Some(EndReason::HostAbsent));
    assert_eq!(states(&jobs, &running), (JobState::Lost, freed.0, freed.1));
    assert_eq!(
        states(&jobs, &cancelled),
        (JobState::Lost, freed.0, freed.1)
    );
    assert!(jobs.get_job(&cancelled).unwrap().unwrap().cancel_requested);
    let timeout = JobState::FailedToStart(StartFailure::StartTimeout);
    assert_eq!(states(&jobs, &timed_out), (timeout, freed.0, freed.1));
    assert_eq!(jobs.get_job(&swept).unwrap(), None);
    assert_eq!(states(&jobs, &swept).1, AllocationStep::Freed);
    assert_eq!(
        states(&jobs, &elsewhere),
        (JobState::Running, AllocationStep::Booked, None)
    );
    assert_eq!(
        lose_absent(&jobs, &provider, &NoMetrics).unwrap(),
        Vec::<JobId>::new()
    );
}

/// A provider that reads every host absent and, as it reads one, writes the host's row as a
/// hello does: a host whose row comes back between the pass's read of it and the first write
/// of its settlement.
struct Returning<'a>(&'a ServerJobs);

impl Observe for Returning<'_> {
    fn observe(&self, host: &HostId, _: &RowRead) -> Result<Seen, String> {
        let hello = NodeHello {
            session: None,
            pool: "burst".to_owned(),
            claim: None,
            shape: None,
            labels: BTreeMap::new(),
            phase: js_agent_wire::Phase::Serving,
            agent_process: None,
            agent_version: None,
        };
        self.0.hello_node(host, &hello).unwrap();
        Ok(Seen::Absent)
    }
}

// Design `multi-instance.md`, G6. Catches a settlement that loses the jobs of a host whose row
// came back after the pass read it absent: each commit checks the row still as read, so the
// first conflicts and nothing of the host is lost, freed or written. With the row there and
// still read absent, the settlement is held to the row's version and loses the jobs. Catches
// too a settlement whose commits check nothing.
#[test]
fn a_row_that_comes_back_between_the_read_and_the_write_ends_nothing() {
    let (_, jobs) = tables();
    let running = at(&jobs, "job-run", "host-a", JobState::Running);
    let placed = at(&jobs, "job-placed", "host-a", JobState::Placed);
    let rows = |jobs: &ServerJobs| {
        [&running, &placed].map(|id| (jobs.get_job(id).unwrap(), jobs.get_allocation(id).unwrap()))
    };
    let before = rows(&jobs);

    let lost = lose_absent(&jobs, &Returning(&jobs), &NoMetrics).unwrap();
    assert_eq!(lost, Vec::<JobId>::new());
    assert_eq!(
        rows(&jobs),
        before,
        "the settlement wrote a job or an allocation"
    );
    for (id, state) in [(&running, JobState::Running), (&placed, JobState::Placed)] {
        assert_eq!(states(&jobs, id), (state, AllocationStep::Booked, None));
    }

    let provider = Provider(BTreeMap::from([("host-a", Ok(Seen::Absent))]));
    let lost = lose_absent(&jobs, &provider, &NoMetrics).unwrap();
    assert_eq!(lost, [placed, running]);
}

// Catches a placed job taken for lost on a returning host (its start is due again), a job
// the listing names or retires taken for lost, a running or starting job the listing omits
// left as it was, and a job whose row is gone made into anything.
#[test]
fn a_listing_after_a_hello_loses_only_the_jobs_that_ran_and_are_not_named() {
    let (clock, jobs) = tables();
    let running = at(&jobs, "job-run", "host-a", JobState::Running);
    let starting = at(&jobs, "job-start", "host-a", JobState::Starting);
    let placed = at(&jobs, "job-placed", "host-a", JobState::Placed);
    let listed = at(&jobs, "job-listed", "host-a", JobState::Running);
    let retired = at(&jobs, "job-retired", "host-a", JobState::Running);
    let swept = at(&jobs, "job-swept", "host-a", JobState::Running);
    let exited = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    jobs.end_job(&swept, exited).unwrap();
    clock.advance(Retention::DEFAULT.window());
    jobs.tick().unwrap();
    let other = at(&jobs, "job-other", "host-b", JobState::Running);

    let held = BTreeSet::from(["job-listed", "job-retired"]);
    let lost = lose_unlisted(&jobs, &host("host-a"), &held, &NoMetrics).unwrap();
    assert_eq!(lost, [running.clone(), starting.clone()]);
    for id in [&running, &starting] {
        assert_eq!(
            states(&jobs, id),
            (JobState::Lost, AllocationStep::Releasing, None)
        );
    }
    for (id, state) in [
        (&placed, JobState::Placed),
        (&listed, JobState::Running),
        (&retired, JobState::Running),
        (&other, JobState::Running),
    ] {
        assert_eq!(states(&jobs, id), (state, AllocationStep::Booked, None));
    }
    assert_eq!(jobs.get_job(&swept).unwrap(), None);
    assert_eq!(states(&jobs, &swept).1, AllocationStep::Releasing);
}

/// The silence limit of the tests of the third way.
const LIMIT: Duration = Duration::from_secs(600);

/// Job tables on a clock an hour past its origin, the store under them, and a second job store
/// over the same rows, standing for another writer.
fn silent_tables() -> (
    Arc<ManualClock>,
    Arc<Interfere<Fake>>,
    ServerJobs,
    ServerJobs,
) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(3600));
    let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let inner: Arc<dyn Store> = store.inner();
    let other = JobStore::new(Arc::new(inner), ServerIds::random()).unwrap();
    (clock, store, jobs, other)
}

/// Writes the row of `name`'s hello, heard now, and answers its epoch.
fn hello(jobs: &ServerJobs, name: &str) -> u64 {
    let hello = NodeHello {
        session: None,
        pool: "pool-a".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: None,
        agent_version: None,
    };
    jobs.hello_node(&host(name), &hello).unwrap().record.epoch
}

/// The third way over `jobs` at the store's clock now, with no floor.
fn silent_pass(jobs: &ServerJobs) -> Result<Vec<JobId>, JobStoreError> {
    let now = jobs.now().unwrap();
    lose_silent(jobs, (LIMIT, StoreTime::ZERO), now, &NoMetrics)
}

/// The state, the lost cause and the allocation's step of the job `id`.
fn silenced(jobs: &ServerJobs, id: &JobId) -> (JobState, Option<LostCause>, AllocationStep) {
    let job = jobs.get_job(id).unwrap().unwrap();
    let step = jobs.get_allocation(id).unwrap().unwrap().step;
    (job.state, job.lost_cause, step)
}

// Catches a host ended a moment before the limit, or not at it; a job of a host heard within the
// limit, or of a host with no row, ended; a retry under a policy that leaves attempts; a cancel
// intent turned into a cancel or dropped; an ended job's end replaced; an allocation freed rather
// than released; a floor ignored; and a second pass that ends anything again.
#[test]
fn a_host_silent_for_the_limit_has_its_live_jobs_lost_and_its_allocations_released() {
    let (clock, _, jobs, _) = silent_tables();
    hello(&jobs, "host-a");
    let running = at(&jobs, "job-run", "host-a", JobState::Running);
    let starting = at(&jobs, "job-start", "host-a", JobState::Starting);
    let placed = at(&jobs, "job-placed", "host-a", JobState::Placed);
    let asked = at(&jobs, "job-asked", "host-a", JobState::Running);
    jobs.request_cancel(&asked).unwrap();
    let timed_out = at(&jobs, "job-timeout", "host-a", JobState::Starting);
    let start_timeout = JobEnd::FailedToStart(StartFailure::StartTimeout);
    jobs.end_job(&timed_out, start_timeout).unwrap();
    let rowless = at(&jobs, "job-rowless", "host-none", JobState::Running);
    clock.advance(LIMIT / 2);
    hello(&jobs, "host-b");
    let heard = at(&jobs, "job-heard", "host-b", JobState::Running);

    // The boundary is measured from host-a's row: every commit of the fake moves its clock on.
    let row = jobs.read_node(&host("host-a")).unwrap().unwrap();
    let limit_at = row.record.heard_at.saturating_add(LIMIT);
    let pass_at =
        |now: StoreTime, floor: StoreTime| lose_silent(&jobs, (LIMIT, floor), now, &NoMetrics);
    let before = row
        .record
        .heard_at
        .saturating_add(LIMIT - Duration::from_millis(1));
    let zero = StoreTime::ZERO;
    assert_eq!(
        pass_at(before, zero),
        Ok(Vec::new()),
        "a moment before the limit"
    );
    let floor = row.record.heard_at.saturating_add(Duration::from_secs(1));
    assert_eq!(
        pass_at(limit_at, floor),
        Ok(Vec::new()),
        "a floor inside the limit"
    );

    let lost = pass_at(limit_at, zero).unwrap();
    // In job id order.
    assert_eq!(
        lost,
        [
            asked.clone(),
            placed.clone(),
            running.clone(),
            starting.clone()
        ]
    );
    let silent = (
        JobState::Lost,
        Some(LostCause::HostSilent),
        AllocationStep::Releasing,
    );
    for id in &lost {
        assert_eq!(silenced(&jobs, id), silent, "{id}");
        let job = jobs.get_job(id).unwrap().unwrap();
        assert_eq!(job.attempt.number(), 1, "{id}: retried");
    }
    assert!(jobs.get_job(&asked).unwrap().unwrap().cancel_requested);
    let timeout = JobState::FailedToStart(StartFailure::StartTimeout);
    let releasing = AllocationStep::Releasing;
    assert_eq!(silenced(&jobs, &timed_out), (timeout, None, releasing));
    for id in [&rowless, &heard] {
        let booked = (JobState::Running, None, AllocationStep::Booked);
        assert_eq!(silenced(&jobs, id), booked, "{id}");
    }
    assert_eq!(pass_at(limit_at, zero), Ok(Vec::new()), "a second pass");
}

// Catches a pass that goes on ending a host's jobs after a beat of the host landed between its
// read of the host's row and an end: the first end conflicts, and nothing of the host ends.
#[test]
fn a_beat_that_lands_before_an_end_commits_ends_nothing_more_on_the_host() {
    let (clock, store, jobs, other) = silent_tables();
    let epoch = hello(&jobs, "host-a");
    let first = at(&jobs, "job-a", "host-a", JobState::Running);
    let second = at(&jobs, "job-b", "host-a", JobState::Running);
    clock.advance(LIMIT);
    store.before_next(move || {
        let beat = other.beat_node(&host("host-a"), epoch, None, |row| row.state.clone());
        assert!(matches!(beat, Ok(js_jobs::Beat::Applied(_))), "{beat:?}");
    });
    assert_eq!(silent_pass(&jobs), Ok(Vec::new()));
    for id in [&first, &second] {
        let running = (JobState::Running, None, AllocationStep::Booked);
        assert_eq!(silenced(&jobs, id), running, "{id}");
    }
}

// Catches a job another writer ended between the pass's read and its end counted as lost, or
// given the cause, and a pass that stops at it rather than going on with the host's other jobs:
// another writer ends the first two jobs just before the first end commits, so that commit is
// stale and the second end finds its job ended; the third is lost.
#[test]
fn a_job_another_writer_ends_meanwhile_keeps_its_end_and_the_rest_are_lost() {
    let (clock, store, jobs, other) = silent_tables();
    hello(&jobs, "host-a");
    let stale = at(&jobs, "job-a", "host-a", JobState::Running);
    let ended = at(&jobs, "job-b", "host-a", JobState::Running);
    let lost = at(&jobs, "job-c", "host-a", JobState::Running);
    clock.advance(LIMIT);
    let exited = || JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    let (a, b) = (stale.clone(), ended.clone());
    store.before_next(move || {
        other.end_job(&a, exited()).unwrap();
        other.end_job(&b, exited()).unwrap();
    });
    assert_eq!(silent_pass(&jobs), Ok(vec![lost.clone()]));
    for id in [&stale, &ended] {
        let kept = (JobState::Succeeded, None, AllocationStep::Releasing);
        assert_eq!(silenced(&jobs, id), kept, "{id}");
    }
    let silent = (
        JobState::Lost,
        Some(LostCause::HostSilent),
        AllocationStep::Releasing,
    );
    assert_eq!(silenced(&jobs, &lost), silent);
}

// Catches a commit the store failed taken as an end, or swallowed: the pass answers the store's
// error and the job is as it was.
#[test]
fn a_failed_end_is_the_stores_error_and_changes_nothing() {
    let (clock, store, jobs, _) = silent_tables();
    hello(&jobs, "host-a");
    let running = at(&jobs, "job-a", "host-a", JobState::Running);
    clock.advance(LIMIT);
    store.fail_next();
    let failed = JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()));
    assert_eq!(silent_pass(&jobs), Err(failed));
    let booked = (JobState::Running, None, AllocationStep::Booked);
    assert_eq!(silenced(&jobs, &running), booked);
}
