//! The two ways a job becomes lost, alone, over the job tables on a manual clock.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{
    AllocationStep, EndReason, JobEnd, JobId, JobState, NewJob, Output, Retention, StartFailure,
};
use js_state::ManualClock;
use js_types::HostId;

use super::{Observe, Seen, lose_absent, lose_unlisted};
use crate::agent_api::job_rig::{BOOKED, spec, tables_on};
use crate::job_store::ServerJobs;

/// A provider whose reading of each host the test sets: unknown unless set.
struct Provider(BTreeMap<&'static str, Result<Seen, String>>);

impl Observe for Provider {
    fn observe(&self, host: &HostId) -> Result<Seen, String> {
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
        assert_eq!(lose_absent(&jobs, &provider).unwrap(), Vec::<JobId>::new());
        assert_eq!(states(&jobs, &running).1, AllocationStep::Booked);
    }

    let provider = Provider(BTreeMap::from([("host-a", Ok(Seen::Absent))]));
    let lost = lose_absent(&jobs, &provider).unwrap();
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
    assert_eq!(lose_absent(&jobs, &provider).unwrap(), Vec::<JobId>::new());
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
    let lost = lose_unlisted(&jobs, &host("host-a"), &held).unwrap();
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
