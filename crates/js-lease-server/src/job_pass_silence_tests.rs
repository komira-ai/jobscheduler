//! The third step of the job pass, end to end: a host silent past the server's `silence_limit`
//! has its jobs ended lost with the cause `host-silent`, none placed again, their allocations
//! kept releasing until the host, heard again, is told to stop them and confirms them gone; a
//! host heard within the limit keeps its jobs; and the silence counts from no earlier than the
//! server's first pass after its boot or after a pass that failed. One manual clock for the
//! registry, the server and the store; time moves only when a test advances it, nothing sleeps.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{AllocationStep, EndReason, JobId, JobSpec, JobState, LostCause};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{self, GetJobRequest};
use js_types::RetryPolicy;
use tonic::Request;

use crate::agent_api::job_rig::{BOOKED, FakeAgent, Rig, clocked_rig, placed, spec, starts, stops};
use crate::job_api::JobApi;
use crate::job_api::read::Ctx;
use crate::job_api::read::silence::Silence;
use crate::job_api::roles::as_operator;
use crate::job_expiry::{DEFAULT_SILENCE_LIMIT, JobLimits};

/// The margin each boundary is checked by: every commit of the rig's fake store moves its clock
/// on a little, so a check a second from the limit could fall on the wrong side of it.
const MARGIN: Duration = Duration::from_secs(10);
const MINUTE: Duration = Duration::from_secs(60);
const YEAR: Duration = Duration::from_secs(365 * 24 * 3600);

/// The server's limits in these tests: no job runs out of time while it waits, and the silence
/// limit is the default, ten minutes.
const LIMITS: JobLimits = JobLimits {
    max_queue_wait: YEAR,
    start_timeout: YEAR,
    ..JobLimits::DEFAULT
};

/// The job service over the rig's tables and clock.
fn api(rig: &Rig) -> JobApi {
    let silence = Silence::of(rig.agents.registry.clone());
    let read = Ctx::default().with_silence(silence);
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_read(read)
}

/// The job `id` as `GetJob` shows it.
async fn get_job(api: &JobApi, id: &JobId) -> v1::Job {
    let asked = as_operator(Request::new(GetJobRequest {
        job_id: id.to_string(),
    }));
    let answer = api.get_job(asked).await.unwrap().into_inner();
    answer.job.unwrap()
}

/// Places the job `id` with `spec` on the host of `agent` and walks it through the agent's
/// heartbeats to starting, or to running when `run`.
async fn started(rig: &Rig, agent: &mut FakeAgent, id: &str, spec: JobSpec, run: bool) -> JobId {
    let (job, _) = placed(&rig.jobs, id, &agent.host, spec, BOOKED);
    agent.take(&rig.beat(agent).await.unwrap());
    if run {
        agent.run(id);
    }
    agent.take(&rig.beat(agent).await.unwrap());
    job.id
}

// Catches a silent host's job left running for ever (its
// client waiting with no deadline), one ended before the limit, one retried or placed again
// under its retry policy, an allocation freed on silence (a returning host would then run a job
// nobody counts), a job of a host heard meanwhile ended, a cause GetJob does not show, a
// returning host not told to stop a lost job (or sent its start), a lost job revived by the
// host's report, and a host's confirmation that does not free the allocation.
#[tokio::test]
async fn a_host_silent_past_the_limit_has_its_jobs_lost_and_told_to_stop_on_its_return() {
    let mut rig = clocked_rig("silence_lost", true, LIMITS);
    let api = api(&rig);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agent = FakeAgent::new("host-a");
    let mut heard = FakeAgent::new("host-b");
    let retrying = JobSpec {
        retry: Some(RetryPolicy {
            max_attempts: 3,
            initial_backoff: None,
            max_backoff: None,
        }),
        ..spec()
    };
    let running = started(&rig, &mut agent, "job-run", retrying, true).await;
    let starting = started(&rig, &mut agent, "job-start", spec(), false).await;
    let kept = started(&rig, &mut heard, "job-kept", spec(), true).await;
    rig.tick();
    agent.take(&rig.beat(&agent).await.unwrap());
    // Placed after the host's last beat: its start never reached the host.
    let unsent = placed(&rig.jobs, "job-placed", "host-a", spec(), BOOKED)
        .0
        .id;

    // A minute short of the limit, the other host beating each minute.
    for _ in 1..DEFAULT_SILENCE_LIMIT.as_secs() / 60 {
        rig.advance(MINUTE);
        heard.take(&rig.beat(&heard).await.unwrap());
        rig.tick();
    }
    rig.advance(MINUTE - MARGIN);
    rig.tick();
    let booked = [
        (&running, JobState::Running),
        (&starting, JobState::Starting),
        (&unsent, JobState::Placed),
    ];
    for (id, state) in booked {
        let held = (state, AllocationStep::Booked);
        assert_eq!(rig.states(id), held, "{id} just before the limit");
    }

    rig.advance(2 * MARGIN);
    rig.tick();
    for id in [&running, &starting, &unsent] {
        let job = rig.job(id);
        let lost = (JobState::Lost, Some(LostCause::HostSilent));
        assert_eq!((job.state, job.lost_cause), lost, "{id}");
        assert_eq!(job.attempt.number(), 1, "{id} was retried");
        assert_eq!(rig.allocation(id).step, AllocationStep::Releasing, "{id}");
    }
    assert!(
        rig.jobs.queued().unwrap().is_empty(),
        "a lost job was queued"
    );
    let running_kept = (JobState::Running, AllocationStep::Booked);
    assert_eq!(rig.states(&kept), running_kept, "a heard host's job");
    let shown = get_job(&api, &running).await;
    assert_eq!(shown.state, i32::from(v1::JobState::Lost));
    assert_eq!(shown.lost_cause, i32::from(v1::LostCause::HostSilent));
    assert_eq!(shown.host_silent_since_ms, None);
    let shown = get_job(&api, &kept).await;
    assert_eq!(shown.lost_cause, i32::from(v1::LostCause::Unspecified));

    // The host comes back (its route returns): each listed job is told to stop, the one it
    // never had is freed by its listing, and nothing is started.
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-run", "job-start"]);
    assert!(starts(&reply).is_empty(), "{reply:?}");
    let freed = rig.allocation(&unsent);
    let absent = (AllocationStep::Freed, Some(EndReason::HostAbsent));
    assert_eq!((freed.step, freed.end_reason), absent);
    agent.take(&reply);
    // The agent stops both and reports them; the reports are acknowledged and change nothing.
    agent.take(&rig.beat(&agent).await.unwrap());
    assert!(!agent.holds("job-run") & !agent.holds("job-start"));
    for id in [&running, &starting] {
        let releasing = (JobState::Lost, AllocationStep::Releasing);
        assert_eq!(rig.states(id), releasing, "{id}");
    }
    rig.beat(&agent).await.unwrap();
    for id in [&running, &starting] {
        let freed = rig.allocation(id);
        assert_eq!((freed.step, freed.end_reason), absent, "{id}");
        let job = rig.job(id);
        let lost = (JobState::Lost, Some(LostCause::HostSilent));
        assert_eq!((job.state, job.lost_cause), lost, "{id}");
    }
}

// Catches a silence counted across beats (three silences just short
// of the limit add up to half an hour), a host heard within the limit whose job is
// ended, and a limit that never ends anything.
#[tokio::test]
async fn a_host_heard_within_the_limit_keeps_its_jobs_and_each_beat_restarts_the_count() {
    let mut rig = clocked_rig("silence_within", true, LIMITS);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let running = started(&rig, &mut agent, "job-run", spec(), true).await;
    rig.tick();
    agent.take(&rig.beat(&agent).await.unwrap());
    for round in 0..3 {
        rig.advance(DEFAULT_SILENCE_LIMIT - MARGIN);
        rig.tick();
        let booked = (JobState::Running, AllocationStep::Booked);
        assert_eq!(rig.states(&running), booked, "round {round}");
        agent.take(&rig.beat(&agent).await.unwrap());
    }
    rig.advance(DEFAULT_SILENCE_LIMIT + MARGIN);
    rig.tick();
    let lost = (JobState::Lost, AllocationStep::Releasing);
    assert_eq!(rig.states(&running), lost);
}

// Catches a server that counts a silence from before its own first pass (after a boot, or an
// outage of every server, each host's row is old) or across a pass that failed (the store could
// not be read, so no host could be heard): each would end every job of the farm at once. Here
// the first pass comes twice the limit after the host was last heard, and a pass fails just before the
// limit; each time the host gets the whole limit again.
#[tokio::test]
async fn silence_counts_from_the_first_pass_after_a_boot_and_after_a_failed_pass() {
    let mut rig = clocked_rig("silence_floor", true, LIMITS);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let running = started(&rig, &mut agent, "job-run", spec(), true).await;
    let booked = (JobState::Running, AllocationStep::Booked);

    rig.advance(2 * DEFAULT_SILENCE_LIMIT);
    rig.tick();
    assert_eq!(
        rig.states(&running),
        booked,
        "the first pass after the boot"
    );
    rig.advance(DEFAULT_SILENCE_LIMIT - MARGIN);
    rig.tick();
    assert_eq!(rig.states(&running), booked, "just before the limit");

    rig.store.fail_read_after(0);
    rig.agents.job_pass();
    rig.advance(2 * MARGIN);
    rig.tick();
    assert_eq!(
        rig.states(&running),
        booked,
        "the pass after the failed one"
    );
    rig.advance(DEFAULT_SILENCE_LIMIT - MARGIN);
    rig.tick();
    assert_eq!(rig.states(&running), booked, "just before the limit again");
    rig.advance(MARGIN);
    rig.tick();
    let lost = (JobState::Lost, AllocationStep::Releasing);
    assert_eq!(rig.states(&running), lost);
}
