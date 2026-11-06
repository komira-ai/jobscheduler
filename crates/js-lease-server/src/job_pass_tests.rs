//! The job pass of the tick, end to end: a server holding the job tables on one manual clock
//! (the registry's, the server's and the store's), agents that speak the per-job messages,
//! and the job service's read calls. Time moves only when a test advances the clock and runs
//! a tick; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::HeldState;
use js_jobs::{
    AllocationStep, EndReason, JobEnd, JobId, JobSpec, JobState, Needs, NewJob, Output,
    StartFailure,
};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    self, GetJobRequest, GetLogsRequest, GetResultRequest, JobResult, StartFailureCode,
};
use js_types::{HostId, Resources};
use tonic::{Request, Status};

use super::{Passed, run, store_time};
use crate::agent_api::job_rig::{
    FakeAgent, Rig, clocked_rig, clocked_rig_keeping, spec, starts, stops,
};
use crate::job_api::JobApi;
use crate::job_api::read::{Ctx, Silence};
use crate::job_expiry::JobLimits;
use crate::job_loss::{Observe, Seen};

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// The registry's grace: a host not heard for longer is silent.
const GRACE: Duration = Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS);

/// Limits long enough that no job of a test runs out of time.
const NEVER: JobLimits = JobLimits {
    max_queue_wait: Duration::from_secs(365 * 24 * 3600),
    start_timeout: Duration::from_secs(365 * 24 * 3600),
};

/// The job service over the rig's tables and clock, reading host silence from its registry.
fn api(rig: &Rig) -> JobApi {
    let silence: Arc<dyn Silence> = Arc::new(rig.agents.registry.clone());
    let read = Ctx::default().with_silence(silence);
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_read(read)
}

async fn get_job(api: &JobApi, id: &JobId) -> Result<v1::Job, Status> {
    let asked = Request::new(GetJobRequest {
        job_id: id.to_string(),
    });
    let answer = api.get_job(asked).await?.into_inner();
    Ok(answer.job.unwrap())
}

async fn get_result(api: &JobApi, id: &JobId) -> Result<JobResult, Status> {
    let asked = Request::new(GetResultRequest {
        job_id: id.to_string(),
    });
    let answer = api.get_result(asked).await?.into_inner();
    Ok(answer.result.unwrap())
}

async fn get_logs(api: &JobApi, id: &JobId) -> Result<v1::GetLogsResponse, Status> {
    let asked = Request::new(GetLogsRequest {
        job_id: id.to_string(),
    });
    Ok(api.get_logs(asked).await?.into_inner())
}

/// The message of a refused call.
fn refused<T>(answer: Result<T, Status>) -> String {
    answer
        .err()
        .map(|status| status.message().to_owned())
        .unwrap()
}

/// Records the queued job `id` with `spec`.
fn queue(rig: &Rig, id: &str, spec: JobSpec) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// [`spec`] needing `cpu_millis` of a host.
fn needing(cpu_millis: u64) -> JobSpec {
    JobSpec {
        needs: Needs::Shape(Resources::new(cpu_millis, 1 << 30, 0, None).unwrap()),
        ..spec()
    }
}

/// The milliseconds of the rig's clock now.
fn now_ms(rig: &Rig) -> u64 {
    js_lease::Clock::now(&rig.shared).millis()
}

/// A provider whose reading of each host the test sets: present unless set.
#[derive(Default)]
struct Provider(std::sync::Mutex<BTreeMap<String, Result<Seen, String>>>);

impl Provider {
    fn set(&self, host: &str, seen: Result<Seen, String>) {
        self.0.lock().unwrap().insert(host.to_owned(), seen);
    }
}

impl Observe for Provider {
    fn observe(&self, host: &HostId) -> Result<Seen, String> {
        let read = self.0.lock().unwrap().get(host.as_str()).cloned();
        read.unwrap_or(Ok(Seen::Present))
    }
}

/// One pass over the rig's tables at its clock, under `limits`, reading hosts from `provider`.
fn pass(rig: &Rig, limits: JobLimits, provider: &Provider) -> Passed {
    run(&rig.jobs, limits, provider, store_time(now_ms(rig))).unwrap()
}

/// Places the job `id` on the host of `agent` and walks it through the agent's heartbeats to
/// starting (the agent takes the start and lists it), or to running when `run`.
async fn started(rig: &Rig, agent: &mut FakeAgent, id: &str, run: bool) -> JobId {
    let job = rig.place(id, &agent.host);
    agent.take(&rig.beat(agent).await.unwrap());
    if run {
        agent.run(id);
    }
    agent.take(&rig.beat(agent).await.unwrap());
    job
}

// Rows 35 (context) and design section 3.2. Catches an off-by-one at the limit, a rule that
// reads the default wait (1 h) instead of the pool's (30 min), and a queued job that stays
// queued for ever. The job never had an allocation, and the code reaches GetJob; GetResult
// answers its end.
#[tokio::test]
async fn a_queued_job_ends_no_capacity_at_max_queue_wait() {
    let limits = JobLimits {
        max_queue_wait: 30 * MINUTE,
        ..JobLimits::DEFAULT
    };
    let rig = clocked_rig("pass_queue_wait", true, limits);
    let api = api(&rig);
    let id = queue(&rig, "job-q", spec());
    rig.advance(30 * MINUTE - SECOND);
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Queued);
    assert_eq!(refused(get_result(&api, &id).await), "not-finished");
    rig.advance(SECOND);
    rig.tick();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
    assert_eq!(rig.jobs.get_allocation(&id).unwrap(), None);
    let shown = get_job(&api, &id).await.unwrap();
    assert_eq!(shown.start_failure, i32::from(StartFailureCode::NoCapacity));
    let result = get_result(&api, &id).await.unwrap();
    assert_eq!(result.state, i32::from(v1::JobState::FailedToStart));
}

// Row 43. Catches an allocation freed at the timeout without the host's confirmation, a stop
// never sent or sent once only, and the room of a timed-out start offered to the next job
// before the host gives it back. Second host: with its agent silent for the whole time the
// job ends but its allocation stays releasing, however many ticks follow.
#[tokio::test]
async fn a_stuck_start_ends_start_timeout_and_frees_after_confirmation() {
    let mut rig = clocked_rig("pass_start_timeout", true, JobLimits::DEFAULT);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let stuck = started(&rig, &mut agent, "job-stuck", false).await;
    assert_eq!(
        rig.states(&stuck),
        (JobState::Starting, AllocationStep::Booked)
    );

    rig.advance(15 * MINUTE - SECOND);
    rig.tick();
    assert_eq!(rig.job(&stuck).state, JobState::Starting);
    rig.advance(SECOND);
    rig.tick();
    let timed_out = JobState::FailedToStart(StartFailure::StartTimeout);
    assert_eq!(rig.states(&stuck), (timed_out, AllocationStep::Releasing));

    // A job that needs the room the stuck one books waits while the host has not confirmed.
    let next = queue(&rig, "job-next", needing(8000));
    for _ in 0..3 {
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(
            stops(&reply),
            ["job-stuck"],
            "the stop is resent on each reply"
        );
        rig.advance(5 * SECOND);
        rig.tick();
        assert_eq!(rig.states(&stuck), (timed_out, AllocationStep::Releasing));
        assert_eq!(rig.job(&next).state, JobState::Queued);
    }

    // The agent stops it, reports it, and drops it once acknowledged: that listing frees.
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.take(&rig.beat(&agent).await.unwrap());
    assert!(!agent.holds("job-stuck"));
    assert_eq!(rig.allocation(&stuck).step, AllocationStep::Releasing);
    let before = rig.commits();
    rig.beat(&agent).await.unwrap();
    let freed = rig.allocation(&stuck);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::StartTimeout))
    );
    assert_eq!(
        rig.commits(),
        before + 2,
        "the free, then the next job's placement"
    );
    assert_eq!(rig.job(&next).state, JobState::Placed);

    // A silent host: the job ends, its allocation is never freed by a clock.
    rig.register("host-b", true);
    let mut quiet = FakeAgent::new("host-b");
    let silent = rig.place("job-silent", "host-b");
    quiet.take(&rig.beat(&quiet).await.unwrap());
    rig.advance(15 * MINUTE);
    rig.tick();
    assert_eq!(rig.states(&silent), (timed_out, AllocationStep::Releasing));
    for _ in 0..200 {
        rig.advance(15 * MINUTE);
        rig.tick();
    }
    assert_eq!(rig.states(&silent), (timed_out, AllocationStep::Releasing));
}

// Catches a start-timeout rule that ignores the state: a job that reached running at 14
// minutes is still running at 20, with its allocation booked.
#[tokio::test]
async fn a_start_that_reached_running_is_not_timed_out() {
    let mut rig = clocked_rig("pass_reached_running", true, JobLimits::DEFAULT);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-r", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    rig.advance(14 * MINUTE);
    agent.run("job-r");
    agent.take(&rig.beat(&agent).await.unwrap());
    assert_eq!(rig.job(&id).state, JobState::Running);
    for _ in 0..6 {
        rig.advance(MINUTE);
        agent.take(&rig.beat(&agent).await.unwrap());
        rig.tick();
    }
    assert_eq!(rig.states(&id), (JobState::Running, AllocationStep::Booked));
}

/// The output of a job that exited.
fn exited(stdout: &str) -> JobEnd {
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            stdout: stdout.to_owned(),
            ..Output::default()
        },
    }
}

// Catches a sweep that deletes by age alone (a running job older than the window) and one
// that deletes an ended job before its retention end or keeps it after: the job, its result
// and its log tail read one tick before the end, and are unknown one tick after.
#[tokio::test]
async fn a_job_past_retention_is_deleted_and_a_live_one_is_not() {
    let rig = clocked_rig("pass_retention", false, NEVER);
    let api = api(&rig);
    let ended = rig.place("job-ended", "host-a");
    let live = rig.place("job-live", "host-a");
    for id in [&ended, &live] {
        rig.jobs
            .transition(id, JobState::Placed, JobState::Starting)
            .unwrap();
        rig.jobs
            .transition(id, JobState::Starting, JobState::Running)
            .unwrap();
    }
    rig.jobs.end_job(&ended, exited("tail\n")).unwrap();
    let window = rig.jobs.retention().window();
    rig.advance(window - SECOND);
    rig.tick();
    let logs = get_logs(&api, &ended).await.unwrap();
    assert_eq!(logs.stdout_tail, b"tail\n");
    assert!(get_result(&api, &ended).await.is_ok());
    rig.advance(SECOND);
    rig.tick();
    assert_eq!(rig.jobs.get_job(&ended).unwrap(), None);
    assert_eq!(refused(get_job(&api, &ended).await), "unknown-job");
    assert_eq!(refused(get_result(&api, &ended).await), "unknown-job");
    assert_eq!(refused(get_logs(&api, &ended).await), "unknown-job");
    rig.advance(window);
    rig.tick();
    assert_eq!(rig.job(&live).state, JobState::Running);
}

// Rows 28, 35 and 41, and question 25. Catches any clock that ends a silent host's jobs or
// frees their allocations (the rejected options B and C), a host forgotten under its jobs, a
// silence read from the wrong instant, and a silence that never clears. The limits are set
// long here so that only silence could end anything; the start timeout of a silent host's
// starting job is a limit of its own (the test above).
#[tokio::test]
async fn a_lost_host_shows_silence_and_ends_nothing() {
    let mut rig = clocked_rig("pass_silence", true, NEVER);
    let api = api(&rig);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let running = started(&rig, &mut agent, "job-run", true).await;
    let starting = started(&rig, &mut agent, "job-start", false).await;
    agent_beat(&rig, &mut agent).await;
    let heard = now_ms(&rig);
    for id in [&running, &starting] {
        assert_eq!(get_job(&api, id).await.unwrap().host_silent_since_ms, None);
    }

    rig.advance(GRACE + Duration::from_millis(1));
    let missed = Some(heard + js_agent_wire::DEFAULT_GRACE_MS);
    for id in [&running, &starting] {
        assert_eq!(
            get_job(&api, id).await.unwrap().host_silent_since_ms,
            missed
        );
    }
    let waiting = queue(&rig, "job-waits", spec());
    for _ in 0..(30 * 24) {
        rig.advance(HOUR);
        rig.tick();
    }
    assert_eq!(
        rig.states(&running),
        (JobState::Running, AllocationStep::Booked)
    );
    assert_eq!(
        rig.states(&starting),
        (JobState::Starting, AllocationStep::Booked)
    );
    assert_eq!(rig.job(&waiting).state, JobState::Queued);
    let a = HostId::new("host-a").unwrap();
    assert!(rig.agents.registry.hosts().iter().any(|view| view.id == a));
    assert_eq!(
        get_job(&api, &running).await.unwrap().host_silent_since_ms,
        missed
    );

    agent.take(&rig.beat(&agent).await.unwrap());
    for id in [&running, &starting] {
        assert_eq!(get_job(&api, id).await.unwrap().host_silent_since_ms, None);
    }
    assert_eq!(
        get_job(&api, &waiting).await.unwrap().host_silent_since_ms,
        None
    );
}

// Rows 28 and 41. Catches a free on an unknown or failed read, a free in a write of its own
// (each job's end and its free are one commit: three jobs, three commits), and a lost job a
// returning agent revives.
#[tokio::test]
async fn absent_frees_and_the_job_is_lost() {
    let mut rig = clocked_rig("pass_absent", true, NEVER);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let running = started(&rig, &mut agent, "job-run", true).await;
    let starting = started(&rig, &mut agent, "job-start", false).await;
    let placed = rig.place("job-placed", "host-a");
    let provider = Provider::default();

    let before = rig.commits();
    for seen in [
        Ok(Seen::Unknown),
        Err("throttled".to_owned()),
        Ok(Seen::Unknown),
        Ok(Seen::Present),
    ] {
        provider.set("host-a", seen);
        assert_eq!(pass(&rig, NEVER, &provider), Passed::default());
        rig.advance(5 * SECOND);
    }
    assert_eq!(
        rig.commits(),
        before,
        "a read that is not absent wrote something"
    );

    provider.set("host-a", Ok(Seen::Absent));
    let passed = pass(&rig, NEVER, &provider);
    // In job id order: job-placed, job-run, job-start.
    assert_eq!(
        passed.lost,
        [placed.clone(), running.clone(), starting.clone()]
    );
    assert_eq!(
        rig.commits(),
        before + 3,
        "each end and its free are one commit"
    );
    for id in [&running, &starting, &placed] {
        let freed = rig.allocation(id);
        assert_eq!(rig.job(id).state, JobState::Lost);
        assert_eq!(
            (freed.step, freed.end_reason),
            (AllocationStep::Freed, Some(EndReason::HostAbsent))
        );
    }

    let reply = rig.beat(&agent).await.unwrap();
    assert!(stops(&reply).contains(&"job-run".to_owned()), "{reply:?}");
    assert_eq!(rig.job(&running).state, JobState::Lost);
    assert_eq!(pass(&rig, NEVER, &provider), Passed::default());
}

// Section 6.7, step 6. Catches a placed job taken for lost (its start is due again), a
// running job the returning host does not list left running, a listed job changed, and a
// loss on a listing that is not the first after a hello.
#[tokio::test]
async fn a_returning_host_that_does_not_list_a_running_job_loses_it() {
    let mut rig = clocked_rig("pass_returning", true, NEVER);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let gone = started(&rig, &mut agent, "job-gone", true).await;
    let kept = started(&rig, &mut agent, "job-kept", true).await;
    let placed = rig.place("job-placed", "host-a");
    let kept_allocation = rig.allocation(&kept).allocation_id.to_string();

    // The same session omits the job: that is no hello, so nothing is lost.
    let mut forgetful = FakeAgent::new("host-a");
    forgetful.hold("job-kept", &kept_allocation, HeldState::Running);
    rig.beat(&forgetful).await.unwrap();
    assert_eq!(
        rig.states(&gone),
        (JobState::Running, AllocationStep::Booked)
    );

    // The agent restarts: past the grace (a host heard within it cannot say hello again), a
    // new hello, and a listing of the job it kept.
    rig.advance(GRACE + SECOND);
    rig.register("host-a", true);
    let reply = rig.beat(&forgetful).await.unwrap();
    assert_eq!(rig.states(&gone), (JobState::Lost, AllocationStep::Freed));
    assert_eq!(
        rig.states(&kept),
        (JobState::Running, AllocationStep::Booked)
    );
    assert_eq!(
        rig.states(&placed),
        (JobState::Placed, AllocationStep::Booked)
    );
    let started_now: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started_now, ["job-placed"]);
}

// Row 42. Catches a job ended cancelled without a host report, and a lost job whose record
// drops the cancel. Second case: a host merely silent, read unknown, leaves the job running
// with its intent and its allocation booked.
#[tokio::test]
async fn cancel_then_absent_is_lost_with_cancel_requested() {
    let mut rig = clocked_rig("pass_cancel_absent", true, NEVER);
    let api = api(&rig);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut a = FakeAgent::new("host-a");
    let mut b = FakeAgent::new("host-b");
    let doomed = started(&rig, &mut a, "job-a", true).await;
    let waiting = started(&rig, &mut b, "job-b", true).await;
    for id in [&doomed, &waiting] {
        rig.jobs.request_cancel(id).unwrap();
    }
    let provider = Provider::default();
    provider.set("host-a", Ok(Seen::Absent));
    provider.set("host-b", Ok(Seen::Unknown));
    rig.advance(GRACE + SECOND);
    pass(&rig, NEVER, &provider);
    rig.tick();

    assert_eq!(rig.states(&doomed), (JobState::Lost, AllocationStep::Freed));
    let shown = get_job(&api, &doomed).await.unwrap();
    assert!(shown.cancel_requested);
    let result = get_result(&api, &doomed).await.unwrap();
    assert_eq!(result.state, i32::from(v1::JobState::Lost));

    assert_eq!(
        rig.states(&waiting),
        (JobState::Running, AllocationStep::Booked)
    );
    assert!(rig.job(&waiting).cancel_requested);
}

// Row 36. Catches every queued job failed at the first tick after an outage, a result deleted
// before a client could read it, a raise that adds the grace to a later deadline (a job whose
// own deadline is past the raise keeps it), and a raise that stretches on each restart.
#[tokio::test]
async fn an_outage_longer_than_max_queue_wait_fails_no_queued_job() {
    let kept = (JobLimits::DEFAULT, js_jobs::Retention::new(HOUR).unwrap());
    let rig = clocked_rig_keeping("pass_outage", false, kept);
    let api = api(&rig);
    let queued = queue(&rig, "job-queued", spec());
    let ended = queue(&rig, "job-ended", spec());
    rig.jobs.request_cancel(&ended).unwrap();
    rig.advance(150 * MINUTE);
    let later = queue(&rig, "job-later", spec());
    rig.advance(30 * MINUTE);

    let boot = store_time(now_ms(&rig));
    assert_eq!(rig.jobs.raise_at_boot(boot), Ok(3));
    rig.advance(5 * SECOND);
    rig.tick();
    assert_eq!(rig.job(&queued).state, JobState::Queued);
    assert!(get_result(&api, &ended).await.is_ok());
    rig.advance(115 * SECOND - SECOND);
    rig.tick();
    assert_eq!(rig.job(&queued).state, JobState::Queued);
    rig.advance(SECOND);
    rig.tick();
    let no_capacity = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&queued).state, no_capacity);
    assert_eq!(rig.job(&later).state, JobState::Queued);
    assert_eq!(refused(get_result(&api, &ended).await), "unknown-job");
    rig.advance(30 * MINUTE - 120 * SECOND);
    rig.tick();
    assert_eq!(rig.job(&later).state, no_capacity);

    // Two boots a minute apart after another outage: the floor is the later boot plus the
    // grace, never the first floor plus it.
    let again = queue(&rig, "job-again", spec());
    rig.advance(3 * HOUR);
    let first = store_time(now_ms(&rig));
    rig.jobs.raise_at_boot(first).unwrap();
    rig.advance(MINUTE);
    let second = store_time(now_ms(&rig));
    rig.jobs.raise_at_boot(second).unwrap();
    let floor = rig.job(&again).deadline_floor;
    assert_eq!(floor, Some(second.saturating_add(js_jobs::BOOT_GRACE)));
    rig.advance(js_jobs::BOOT_GRACE - SECOND);
    rig.tick();
    assert_eq!(rig.job(&again).state, JobState::Queued);
    rig.advance(SECOND);
    rig.tick();
    assert_eq!(rig.job(&again).state, no_capacity);
}

/// The states of a queued job of a rig whose clock starts `offset` past [`START_MS`], one
/// tick before its queue wait runs out and one tick at it.
fn queue_wait_at(name: &str, offset: Duration) -> (JobState, JobState) {
    let limits = JobLimits {
        max_queue_wait: 30 * MINUTE,
        ..JobLimits::DEFAULT
    };
    let rig = clocked_rig(name, false, limits);
    rig.advance(offset);
    let id = queue(&rig, "job-q", spec());
    rig.advance(30 * MINUTE - SECOND);
    rig.tick();
    let before = rig.job(&id).state;
    rig.advance(SECOND);
    rig.tick();
    (before, rig.job(&id).state)
}

// Catches a stray read of the system's clock in the pass: with the server's clock near the
// epoch and centuries ahead of the system's, a job ends at the same instant of its own.
#[test]
fn a_job_pass_reads_no_wall_clock() {
    let near = queue_wait_at("pass_wall_clock_near", Duration::ZERO);
    let far = queue_wait_at("pass_wall_clock_far", 300 * 365 * DAY);
    let want = (
        JobState::Queued,
        JobState::FailedToStart(StartFailure::NoCapacity),
    );
    assert_eq!((near, far), (want, want));
}

// Catches a host the registry removed (the provider read it gone) not read absent by the
// server's own tick, a removal kept after its allocations are settled, and a host that said
// hello again before the tick still read absent. A host merely silent is read unknown and
// keeps everything.
#[tokio::test]
async fn a_removed_host_is_read_absent_by_the_tick_and_a_returning_one_is_not() {
    let mut rig = clocked_rig("pass_removed", true, NEVER);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut a = FakeAgent::new("host-a");
    let mut b = FakeAgent::new("host-b");
    let on_a = started(&rig, &mut a, "job-a", true).await;
    let on_b = started(&rig, &mut b, "job-b", true).await;
    rig.advance(GRACE + SECOND);
    agent_beat(&rig, &mut b).await;
    rig.tick();
    assert_eq!(
        rig.states(&on_a),
        (JobState::Running, AllocationStep::Booked)
    );

    let placer = rig.agents.placer.clone().unwrap();
    let removed = |host: &HostId| {
        let registry = &rig.agents.registry;
        registry.drain(host).unwrap();
        registry.release_host(host).unwrap();
        registry.remove(host).unwrap();
    };
    let host_a = HostId::new("host-a").unwrap();
    removed(&host_a);
    assert!(placer.gone().contains_key(&host_a));
    rig.tick();
    assert_eq!(rig.states(&on_a), (JobState::Lost, AllocationStep::Freed));
    assert_eq!(
        rig.states(&on_b),
        (JobState::Running, AllocationStep::Booked)
    );
    assert!(placer.gone().is_empty(), "a settled removal is kept");

    // Removed, then back with a hello before any tick: not absent. Its first listing, which
    // omits the job, loses it instead.
    let host_b = HostId::new("host-b").unwrap();
    removed(&host_b);
    rig.register("host-b", true);
    assert!(placer.gone().is_empty());
    rig.tick();
    assert_eq!(
        rig.states(&on_b),
        (JobState::Running, AllocationStep::Booked)
    );
    agent_beat(&rig, &mut FakeAgent::new("host-b")).await;
    assert_eq!(rig.states(&on_b), (JobState::Lost, AllocationStep::Freed));
}

/// A heartbeat of `agent` whose reply it takes.
async fn agent_beat(rig: &Rig, agent: &mut FakeAgent) {
    agent.take(&rig.beat(agent).await.unwrap());
}

// Catches a failed pass that settles the removals it did not act on, or that panics: the
// tick logs it, changes nothing, and the next tick passes.
#[tokio::test]
async fn a_failed_pass_changes_nothing_and_the_next_one_acts() {
    let rig = clocked_rig("pass_failed_pass", false, JobLimits::DEFAULT);
    let id = queue(&rig, "job-q", spec());
    rig.advance(HOUR);
    rig.store.fail_read_after(0);
    rig.agents.job_pass();
    assert_eq!(rig.job(&id).state, JobState::Queued);
    rig.agents.job_pass();
    assert_eq!(
        rig.job(&id).state,
        JobState::FailedToStart(StartFailure::NoCapacity)
    );
    let without = crate::agent_api::job_rig::rig("pass_no_tables", false);
    without.agents.job_pass();
}
