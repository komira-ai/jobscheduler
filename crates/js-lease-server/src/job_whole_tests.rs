//! The prepare of a whole-host job alone: what a heartbeat's report and the tick do to a placed
//! whole-host job, over the job tables on a manual clock. Nothing here sleeps.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, Phase, RefusalReason};
use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, EndReason, Exclusion, JobId, JobRecord, JobState,
    JobStoreError, Needs, NewJob, Placement, Retention, StartFailure,
};
use js_registry::WholeNodeConfig;
use js_state::fake::Fake;
use js_state::{ManualClock, Store, StoreError, StoreTime};
use js_types::HostId;

use super::{Beat, expire_prepares, serve};
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables_on};
use crate::job_store::ServerJobs;
use crate::metrics::NoMetrics;

const SECOND: Duration = Duration::from_secs(1);

/// The prepare timeout of the rules of these tests.
const PREPARE: Duration = Duration::from_secs(600);

/// The job tables, the clock they read and the store under them.
struct Fixture {
    clock: Arc<ManualClock>,
    store: Arc<Interfere<Fake>>,
    jobs: Arc<ServerJobs>,
}

impl Fixture {
    fn new() -> Fixture {
        let clock = Arc::new(ManualClock::default());
        clock.advance(Duration::from_secs(3600));
        let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
        Fixture {
            clock,
            store,
            jobs: Arc::new(jobs),
        }
    }

    fn now(&self) -> StoreTime {
        js_state::Clock::now(&*self.clock)
    }

    fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Records the whole-host job `id` and places it on `host-a` with the prepare timeout.
    fn place(&self, id: &str) -> JobId {
        let id = self.queue(id);
        self.place_again(&id);
        id
    }

    /// Records the whole-host job `id`, queued.
    fn queue(&self, id: &str) -> JobId {
        let id = JobId::parse(id).unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: js_jobs::JobSpec {
                needs: Needs::WholeHost,
                ..spec()
            },
            priority: 0,
            client: None,
        };
        self.jobs.insert_queued(new).unwrap();
        id
    }

    /// Places the queued whole-host job `id` on `host-a`.
    fn place_again(&self, id: &JobId) {
        let placement = Placement {
            prepare_timeout: Some(PREPARE),
            wall_limit_secs: None,
        };
        self.jobs
            .place_with(id, host(), Booked::WholeHost, placement)
            .unwrap();
    }

    fn job(&self, id: &JobId) -> JobRecord {
        self.jobs.get_job(id).unwrap().unwrap()
    }

    fn allocation(&self, id: &JobId) -> AllocationRecord {
        self.jobs.get_allocation(id).unwrap().unwrap()
    }

    fn commits(&self) -> u64 {
        self.store.commit_stats().txns
    }

    /// What the heartbeat `beat` does, with the registry's taint recorded in `taints`.
    fn serve(&self, beat: &Beat<'_>, taints: &Mutex<Vec<HostId>>) -> Result<bool, JobStoreError> {
        let taint = |host: &HostId| {
            taints
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(host.clone());
            true
        };
        serve(
            &self.jobs,
            beat,
            (&rules(), self.now()),
            (&NoMetrics, &taint),
        )
    }

    /// The tick at the clock's time.
    fn expire(&self) -> Result<Vec<JobId>, JobStoreError> {
        expire_prepares(&self.jobs, &rules(), self.now(), &NoMetrics)
    }
}

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// A pool that allows two failed prepares, with the default timeout and back-off.
fn rules() -> WholeNodeConfig {
    WholeNodeConfig {
        max_replace: 2,
        prepare_timeout_ms: 600_000,
        ..WholeNodeConfig::dedicated(1)
    }
}

/// A beat of `host-a` that is preparing and reports nothing.
fn quiet(host: &HostId) -> Beat<'_> {
    Beat {
        host,
        phase: Phase::Preparing,
        ready: None,
        refused: None,
        first: false,
        tainted: false,
    }
}

/// The refusal `code` of the allocation `lease`.
fn refusal(lease: &str, code: RefusalReason) -> ExclusiveRefusal {
    ExclusiveRefusal {
        lease_id: lease.to_owned(),
        code,
        detail: String::new(),
    }
}

/// The id of the allocation of `id`, as the agent reports it.
fn lease(fixture: &Fixture, id: &JobId) -> String {
    fixture.allocation(id).allocation_id.to_string()
}

fn no_taint() -> Mutex<Vec<HostId>> {
    Mutex::default()
}

// Catches a ready report that does not start the job, one that starts a job for another
// allocation's report, one from a tainted host that starts it all the same, a start that
// writes more than the job's move, and a ready report acted on twice.
#[test]
fn a_ready_report_starts_a_placed_job_from_an_untainted_host() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let host = host();
    let ready = lease(&fixture, &id);
    let before = fixture.commits();

    let other = Beat {
        ready: Some("another-allocation"),
        ..quiet(&host)
    };
    assert_eq!(fixture.serve(&other, &no_taint()), Ok(false));
    let tainted = Beat {
        ready: Some(&ready),
        tainted: true,
        ..quiet(&host)
    };
    assert_eq!(fixture.serve(&tainted, &no_taint()), Ok(false));
    assert_eq!(fixture.commits(), before);
    assert_eq!(fixture.job(&id).state, JobState::Placed);

    let beat = Beat {
        ready: Some(&ready),
        phase: Phase::Serving,
        ..quiet(&host)
    };
    assert_eq!(fixture.serve(&beat, &no_taint()), Ok(false));
    assert_eq!(fixture.commits(), before + 1);
    assert_eq!(fixture.job(&id).state, JobState::Starting);
    assert_eq!(fixture.allocation(&id).step, AllocationStep::Booked);
    assert_eq!(fixture.serve(&beat, &no_taint()), Ok(false));
    assert_eq!(fixture.commits(), before + 1);
}

// Catches a beat that writes when the agent reports nothing, and one that touches a job that
// shares the host or a whole-host job that is already starting.
#[test]
fn a_beat_with_nothing_to_report_changes_nothing() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let (_, share) = placed(&fixture.jobs, "job-s", "host-b", spec(), BOOKED);
    let (host_a, host_b) = (host(), HostId::new("host-b").unwrap());
    let before = fixture.commits();

    let ready_share = share.allocation_id.to_string();
    let beats = [
        quiet(&host_a),
        Beat {
            first: true,
            ready: Some(&ready_share),
            ..quiet(&host_b)
        },
    ];
    for beat in &beats {
        assert_eq!(fixture.serve(beat, &no_taint()), Ok(false));
    }
    assert_eq!(fixture.commits(), before);
    assert_eq!(fixture.job(&id).state, JobState::Placed);

    fixture
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    let started = fixture.commits();
    let first = Beat {
        first: true,
        ..quiet(&host_a)
    };
    assert_eq!(fixture.serve(&first, &no_taint()), Ok(false));
    assert_eq!(fixture.commits(), started);
    assert_eq!(fixture.job(&id).state, JobState::Starting);
}

// Catches a hello that leaves the prepare clock where the placement set it, one that moves it
// from the wrong instant, a restart that takes a ready report of the old session for the new
// one, and a restart that writes more than the allocation.
#[test]
fn the_first_listing_restarts_the_prepare_clock() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let placed_deadline = fixture.allocation(&id).prepare_deadline;
    let placed_at = fixture.job(&id).placed_at.unwrap();
    assert_eq!(placed_deadline, Some(placed_at.saturating_add(PREPARE)));
    fixture.advance(100 * SECOND);
    let host = host();
    let ready = lease(&fixture, &id);
    let before = fixture.commits();

    let hello = Beat {
        first: true,
        ready: Some(&ready),
        ..quiet(&host)
    };
    let hello_at = fixture.now();
    assert_eq!(fixture.serve(&hello, &no_taint()), Ok(false));
    assert_eq!(fixture.commits(), before + 1);
    let restarted = fixture.allocation(&id);
    assert_eq!(
        restarted.prepare_deadline,
        Some(hello_at.saturating_add(PREPARE))
    );
    assert_ne!(restarted.prepare_deadline, placed_deadline);
    assert_eq!(fixture.job(&id).state, JobState::Placed);
    assert_eq!(restarted.step, AllocationStep::Booked);
}

// Catches a refusal that is not acted on at once, one that frees the allocation without
// requeueing the job (or the other way round), a count or a bar that is not on the job's row,
// a code that bars the wrong way, and a taint that does not reach the registry or reaches it
// for a code that does not taint.
#[test]
fn a_refusal_requeues_the_job_and_bars_the_host_as_its_code_says() {
    let host = host();
    for (code, tainting, bar) in [
        (RefusalReason::NotQuiet, false, true),
        (RefusalReason::Unsupported, false, true),
        (RefusalReason::HostUnsafe, true, false),
    ] {
        let fixture = Fixture::new();
        let id = fixture.place("job-w");
        let leased = lease(&fixture, &id);
        let refused = refusal(&leased, code);
        let beat = Beat {
            refused: Some(&refused),
            ..quiet(&host)
        };
        let tainted = no_taint();
        let before = fixture.commits();
        assert_eq!(fixture.serve(&beat, &tainted), Ok(tainting), "{code:?}");
        assert_eq!(fixture.commits(), before + 1);
        let job = fixture.job(&id);
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.failed_prepares, 1);
        assert_eq!(job.prepare_bars.contains_key(&host), bar, "{code:?}");
        assert_eq!(
            tainted.lock().unwrap().clone(),
            if tainting { vec![host.clone()] } else { vec![] }
        );
        let freed = fixture.allocation(&id);
        assert_eq!(
            (freed.step, freed.end_reason),
            (AllocationStep::Freed, Some(EndReason::FailedPrepare))
        );
        if code == RefusalReason::Unsupported {
            assert_eq!(job.prepare_bars[&host], Exclusion::ForLife);
        }
    }
}

// Catches a limit counted past or short of `max_replace`, a terminal failed prepare that goes
// back to queued, and a queue position that is lost with a requeue.
#[test]
fn the_last_allowed_failed_prepare_ends_the_job_failed_to_start() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let host = host();
    let submitted = fixture.job(&id).submitted_at;
    for attempt in 1..=2u32 {
        let leased = lease(&fixture, &id);
        let refused = refusal(&leased, RefusalReason::NotQuiet);
        let beat = Beat {
            refused: Some(&refused),
            ..quiet(&host)
        };
        assert_eq!(fixture.serve(&beat, &no_taint()), Ok(false));
        let job = fixture.job(&id);
        assert_eq!(job.failed_prepares, attempt);
        assert_eq!(job.submitted_at, submitted);
        if attempt == 1 {
            assert_eq!(job.state, JobState::Queued);
            fixture.place_again(&id);
        } else {
            assert_eq!(
                job.state,
                JobState::FailedToStart(StartFailure::ExclusiveNotReady)
            );
        }
    }
    assert_eq!(fixture.allocation(&id).step, AllocationStep::Freed);
}

// Catches a cancel that is acted on while the agent still prepares, one that waits for ever
// (the agent leaves `preparing`, reports the allocation ready, or refuses it), a cancel that a
// hello turns into a restart, and a release that is not one commit ending the job cancelled and
// freeing the allocation.
#[test]
fn a_cancel_waits_for_the_agent_to_leave_preparing() {
    let host = host();
    for report in 0..4 {
        let fixture = Fixture::new();
        let id = fixture.place("job-w");
        fixture.jobs.request_cancel(&id).unwrap();
        let waiting = fixture.commits();
        assert_eq!(fixture.serve(&quiet(&host), &no_taint()), Ok(false));
        assert_eq!(fixture.commits(), waiting, "report {report}");
        assert_eq!(fixture.job(&id).state, JobState::Placed);

        let leased = lease(&fixture, &id);
        let refused = refusal(&leased, RefusalReason::NotQuiet);
        let beat = match report {
            0 => Beat {
                phase: Phase::Serving,
                ..quiet(&host)
            },
            1 => Beat {
                phase: Phase::Restoring,
                ..quiet(&host)
            },
            2 => Beat {
                ready: Some(&leased),
                ..quiet(&host)
            },
            _ => Beat {
                refused: Some(&refused),
                ..quiet(&host)
            },
        };
        let hello = Beat {
            first: true,
            ..beat
        };
        assert_eq!(fixture.serve(&hello, &no_taint()), Ok(false));
        assert_eq!(fixture.commits(), waiting + 1, "report {report}");
        let job = fixture.job(&id);
        assert_eq!(job.state, JobState::Cancelled);
        assert!(job.cancel_requested);
        let freed = fixture.allocation(&id);
        assert_eq!(
            (freed.step, freed.end_reason),
            (AllocationStep::Freed, Some(EndReason::Cancelled))
        );
    }
}

// Catches a failed commit taken for a change, or swallowed: the beat answers the store's error
// and the job and allocation are as they were.
#[test]
fn a_failed_commit_is_the_beats_error() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let host = host();
    let ready = lease(&fixture, &id);
    let before = fixture.allocation(&id);
    fixture.store.fail_next();
    let beat = Beat {
        ready: Some(&ready),
        ..quiet(&host)
    };
    let failed = fixture.serve(&beat, &no_taint());
    assert!(
        matches!(failed, Err(JobStoreError::Store(StoreError::Failed(_)))),
        "{failed:?}"
    );
    assert_eq!(fixture.job(&id).state, JobState::Placed);
    assert_eq!(fixture.allocation(&id), before);
}

// Catches a deadline checked early or late (it holds at the instant, not before), a timeout
// that frees the allocation without requeueing the job, one that does not count, a started
// job failed for a deadline it met, and an allocation of a job that shares the host failed.
#[test]
fn a_prepare_fails_at_its_deadline_and_not_before() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    let started = fixture.place("job-s");
    fixture
        .jobs
        .transition(&started, JobState::Placed, JobState::Starting)
        .unwrap();
    let (shared, _) = placed(&fixture.jobs, "job-p", "host-b", spec(), BOOKED);
    fixture.advance(PREPARE - SECOND);
    assert_eq!(fixture.expire(), Ok(vec![]));
    fixture.advance(SECOND);
    let before = fixture.commits();
    assert_eq!(fixture.expire(), Ok(vec![id.clone()]));
    assert_eq!(fixture.commits(), before + 1);
    let job = fixture.job(&id);
    assert_eq!((job.state, job.failed_prepares), (JobState::Queued, 1));
    assert!(job.prepare_bars.contains_key(&host()));
    let freed = fixture.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::FailedPrepare))
    );
    assert_eq!(fixture.job(&started).state, JobState::Starting);
    assert_eq!(fixture.job(&shared.id).state, JobState::Placed);
    assert_eq!(fixture.expire(), Ok(vec![]));
}

// Catches a timeout that fails a job the client cancelled (it is cancelled and its host let
// go), and a cancel that waits for a deadline when the deadline has passed.
#[test]
fn a_cancelled_job_at_its_deadline_is_cancelled_and_freed() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    fixture.jobs.request_cancel(&id).unwrap();
    fixture.advance(PREPARE);
    assert_eq!(fixture.expire(), Ok(vec![id.clone()]));
    let job = fixture.job(&id);
    assert_eq!((job.state, job.failed_prepares), (JobState::Cancelled, 0));
    let freed = fixture.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Cancelled))
    );
}

// Catches a limit that is not the pool's `max_replace` for timeouts.
#[test]
fn timeouts_end_the_job_after_max_replace() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    fixture.advance(PREPARE);
    assert_eq!(fixture.expire(), Ok(vec![id.clone()]));
    assert_eq!(fixture.job(&id).state, JobState::Queued);
    fixture.place_again(&id);
    fixture.advance(PREPARE);
    assert_eq!(fixture.expire(), Ok(vec![id.clone()]));
    assert_eq!(
        fixture.job(&id).state,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady)
    );
}

// Catches a pass that fails, or fails the prepare anyway, when a heartbeat moved the job
// between the pass's read and its commit (the report wins), and a pass that swallows a failed
// commit.
#[test]
fn a_job_that_moved_since_the_read_is_left_for_the_next_pass() {
    let fixture = Fixture::new();
    let id = fixture.place("job-w");
    fixture.advance(PREPARE);
    let racing = Arc::clone(&fixture.jobs);
    let raced = id.clone();
    fixture.store.before_next(move || {
        racing
            .transition(&raced, JobState::Placed, JobState::Starting)
            .unwrap();
    });
    assert_eq!(fixture.expire(), Ok(vec![]));
    assert_eq!(fixture.job(&id).state, JobState::Starting);

    let other = fixture.place("job-x");
    fixture.advance(PREPARE);
    fixture.store.fail_next();
    let failed = fixture.expire();
    assert!(
        matches!(failed, Err(JobStoreError::Store(StoreError::Failed(_)))),
        "{failed:?}"
    );
    assert_eq!(fixture.job(&other).state, JobState::Placed);
}
