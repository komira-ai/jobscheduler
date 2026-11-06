//! The time rules alone, over the job tables on a manual clock.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobRecord, JobState, JobStoreError, NewJob, Retention, StartFailure};
use js_state::{ManualClock, StoreError, StoreTime};

use super::{JobLimits, deadline, due, expire};
use crate::agent_api::job_rig::{BOOKED, spec, tables_on};
use crate::file::PoolSettings;
use crate::job_store::ServerJobs;

const SECOND: Duration = Duration::from_secs(1);

/// The limits of these tests: a queue wait of 30 minutes, a start timeout of 10.
const LIMITS: JobLimits = JobLimits {
    max_queue_wait: Duration::from_secs(1800),
    start_timeout: Duration::from_secs(600),
};

/// Job tables on a clock at an hour past its origin.
fn tables() -> (
    Arc<ManualClock>,
    Arc<js_jobs::interfere::Interfere<js_state::fake::Fake>>,
    Arc<ServerJobs>,
) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(3600));
    let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    (clock, store, Arc::new(jobs))
}

fn queue(jobs: &ServerJobs, text: &str) -> JobId {
    let id = JobId::parse(text).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    id
}

fn record(jobs: &ServerJobs, id: &JobId) -> JobRecord {
    jobs.get_job(id).unwrap().unwrap()
}

fn host() -> js_types::HostId {
    js_types::HostId::new("host-a").unwrap()
}

/// `at` less `by`.
fn before(at: StoreTime, by: Duration) -> StoreTime {
    StoreTime::from_duration(at.as_duration() - by)
}

// Catches a limit counted from the wrong instant (the boot, the pass, the submit of a placed
// job), the wrong limit for a state, a limit applied to a running or ended job, and a floor
// that lowers or replaces a later deadline.
#[test]
fn each_deadline_counts_from_its_stored_instant_and_is_held_to_the_floor() {
    let (clock, _, jobs) = tables();
    let queued = queue(&jobs, "job-q");
    let submitted = record(&jobs, &queued).submitted_at;
    clock.advance(Duration::from_secs(100));
    let placed = queue(&jobs, "job-p");
    jobs.place(&placed, host(), BOOKED).unwrap();
    let starting = queue(&jobs, "job-s");
    jobs.place(&starting, host(), BOOKED).unwrap();
    jobs.transition(&starting, JobState::Placed, JobState::Starting)
        .unwrap();
    let running = queue(&jobs, "job-r");
    jobs.place(&running, host(), BOOKED).unwrap();
    jobs.transition(&running, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&running, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = queue(&jobs, "job-e");
    jobs.end_job(&ended, JobEnd::FailedToStart(StartFailure::NoCapacity))
        .unwrap();

    let queue_end = submitted.saturating_add(LIMITS.max_queue_wait);
    // The store stamps each commit at its own instant: each start counts from its placement.
    let start_end = |id| {
        let placed_at = record(&jobs, id).placed_at.unwrap();
        placed_at.saturating_add(LIMITS.start_timeout)
    };
    let of = |id| deadline(&record(&jobs, id), LIMITS);
    assert_eq!(of(&queued), Some((queue_end, StartFailure::NoCapacity)));
    assert_eq!(
        of(&placed),
        Some((start_end(&placed), StartFailure::StartTimeout))
    );
    assert_eq!(
        of(&starting),
        Some((start_end(&starting), StartFailure::StartTimeout))
    );
    assert_eq!((of(&running), of(&ended)), (None, None));

    let job = record(&jobs, &queued);
    assert_eq!(due(&job, LIMITS, before(queue_end, SECOND)), None);
    assert_eq!(due(&job, LIMITS, queue_end), Some(StartFailure::NoCapacity));
    assert_eq!(
        due(
            &record(&jobs, &running),
            LIMITS,
            StoreTime::from_duration(Duration::MAX)
        ),
        None
    );

    // A boot past both deadlines raises the floor: each deadline moves to it.
    let boot = submitted.saturating_add(Duration::from_secs(3000));
    jobs.raise_at_boot(boot).unwrap();
    let floor = boot.saturating_add(js_jobs::BOOT_GRACE);
    assert_eq!(of(&queued), Some((floor, StartFailure::NoCapacity)));
    assert_eq!(of(&placed), Some((floor, StartFailure::StartTimeout)));
    // A boot long before a deadline: the floor is below it, and the deadline stays.
    let early = StoreTime::from_duration(Duration::from_secs(1));
    let (_, _, other) = tables();
    let late = queue(&other, "job-late");
    other.raise_at_boot(early).unwrap();
    let job = record(&other, &late);
    assert_eq!(
        deadline(&job, LIMITS),
        Some((
            job.submitted_at.saturating_add(LIMITS.max_queue_wait),
            StartFailure::NoCapacity
        )),
        "a floor before the deadline replaced it"
    );
}

// Catches an expiry that ends a job at the wrong limit, frees or leaves booked the allocation
// of a start that timed out, or ends a job that is not out of time.
#[test]
fn expire_ends_the_jobs_out_of_time_and_releases_their_allocations() {
    let (clock, _, jobs) = tables();
    let queued = queue(&jobs, "job-q");
    let placed = queue(&jobs, "job-p");
    jobs.place(&placed, host(), BOOKED).unwrap();
    clock.advance(LIMITS.start_timeout);
    let fresh = queue(&jobs, "job-fresh");
    let now = jobs_now(&clock);
    let ended = expire(&jobs, LIMITS, now).unwrap();
    assert_eq!(ended, [(placed.clone(), StartFailure::StartTimeout)]);
    let allocation = jobs.get_allocation(&placed).unwrap().unwrap();
    assert_eq!(allocation.step, js_jobs::AllocationStep::Releasing);
    clock.advance(LIMITS.max_queue_wait - LIMITS.start_timeout);
    let ended = expire(&jobs, LIMITS, jobs_now(&clock)).unwrap();
    assert_eq!(ended, [(queued.clone(), StartFailure::NoCapacity)]);
    assert_eq!(record(&jobs, &fresh).state, JobState::Queued);
    assert_eq!(expire(&jobs, LIMITS, jobs_now(&clock)).unwrap(), Vec::new());
}

fn jobs_now(clock: &ManualClock) -> StoreTime {
    js_state::Clock::now(clock)
}

// Catches a pass that fails, or ends a job anyway, when a heartbeat moved the job between the
// pass's read and its commit: the job keeps the move, and the next pass reads it again.
#[test]
fn a_job_that_moved_since_the_read_is_left_for_the_next_pass() {
    let (clock, store, jobs) = tables();
    let id = queue(&jobs, "job-q");
    clock.advance(LIMITS.max_queue_wait);
    let racing = Arc::clone(&jobs);
    let raced = id.clone();
    store.before_next(move || {
        racing.place(&raced, host(), BOOKED).unwrap();
    });
    assert_eq!(expire(&jobs, LIMITS, jobs_now(&clock)), Ok(Vec::new()));
    assert_eq!(record(&jobs, &id).state, JobState::Placed);
}

// Catches a failed commit taken for an end, or swallowed: the pass answers the store's error
// and the job is as it was.
#[test]
fn a_failed_commit_ends_the_expiry_with_its_error() {
    let (clock, store, jobs) = tables();
    let id = queue(&jobs, "job-q");
    clock.advance(LIMITS.max_queue_wait);
    store.fail_next();
    let failed = expire(&jobs, LIMITS, jobs_now(&clock));
    assert!(
        matches!(failed, Err(JobStoreError::Store(StoreError::Failed(_)))),
        "{failed:?}"
    );
    assert_eq!(record(&jobs, &id).state, JobState::Queued);
}

// Catches limits read from the wrong setting, or defaults that differ from the pool file's.
#[test]
fn the_limits_are_the_pools_settings() {
    let settings = PoolSettings {
        max_queue_wait: Duration::from_secs(1800),
        start_timeout: Duration::from_secs(90),
        path_grace: Duration::from_secs(5),
    };
    let limits = JobLimits::of(&settings);
    assert_eq!(
        (limits.max_queue_wait, limits.start_timeout),
        (Duration::from_secs(1800), Duration::from_secs(90))
    );
    assert_eq!(JobLimits::default(), JobLimits::DEFAULT);
    assert_eq!(
        JobLimits::DEFAULT,
        JobLimits {
            max_queue_wait: Duration::from_secs(3600),
            start_timeout: Duration::from_secs(900),
        }
    );
}
