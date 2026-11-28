//! Whole-host jobs through the heartbeat route (design sections 5.3, 5.4 and 6.7, and section
//! 8, rows 15, 26 and 39): a job that needs the whole host is queued as an ordinary job row,
//! the engine places it on a quiet host under the pool's whole-node settings, the agent is told
//! to prepare for it, and its ready report, refusal, silence, cancel and hello move it. The
//! server is a [`whole_rig`] on one manual clock, the agent a [`FakeAgent`] that follows the
//! allocation each reply names; nothing sleeps.

use std::time::Duration;

use js_agent_wire::{HeartbeatReply, Outcome, Phase, RefusalReason, Taint};
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, EndReason, JobId, JobSpec, JobState, Needs, NewJob,
    StartFailure,
};
use js_registry::WholeNodeConfig;
use js_state::{Store, StoreTime};
use js_types::{Clock, JobEventKind};

use super::job_rig::{FakeAgent, Rig, START_MS, spec, starts, stops, whole_over, whole_rig};
use crate::job_pass::store_time;

pub(super) const SECOND: Duration = Duration::from_secs(1);

/// The prepare timeout of [`rules`].
pub(super) const PREPARE: Duration = Duration::from_secs(600);

/// How far the store's clock moves between a beat's arrival and its listing's first read of
/// it: the beat's own row write (`beat_node`) commits first, and each store call moves the
/// test store's clock one millisecond.
const BEAT_ROW: Duration = Duration::from_millis(2);

/// A pool of up to two whole hosts with the default timings, that fails a job after two failed
/// prepares.
pub(super) fn rules() -> WholeNodeConfig {
    WholeNodeConfig {
        max_replace: 2,
        ..WholeNodeConfig::dedicated(2)
    }
}

/// A server for these tests with `host-a` registered, and the agent of that host.
pub(super) fn served(name: &str) -> (Rig, FakeAgent) {
    let mut rig = whole_rig(name, rules(), None);
    rig.register("host-a", true);
    (rig, FakeAgent::new("host-a"))
}

/// The rig's clock as the store reads it.
pub(super) fn now(rig: &Rig) -> StoreTime {
    store_time(rig.shared.now().millis())
}

/// Records the job `id` with `spec`, queued.
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

/// Records the job `id` that needs the whole host and may run `wall_secs`, queued.
pub(super) fn queue_whole(rig: &Rig, id: &str, wall_secs: u64) -> JobId {
    let spec = JobSpec {
        needs: Needs::WholeHost,
        wall_limit_secs: wall_secs,
        ..spec()
    };
    queue(rig, id, spec)
}

/// Records the job `id` that shares the host and may run `wall_secs`, queued.
pub(super) fn queue_share(rig: &Rig, id: &str, wall_secs: u64) -> JobId {
    let spec = JobSpec {
        wall_limit_secs: wall_secs,
        ..spec()
    };
    queue(rig, id, spec)
}

/// One heartbeat of `agent` through the route, and the reply taken: the agent follows what it
/// says.
pub(super) async fn beat(rig: &Rig, agent: &mut FakeAgent) -> HeartbeatReply {
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
    reply
}

/// Moves the clock on by `secs`, 30 seconds at a time, `agent` beating after each step so its
/// host is never lost (the grace is a minute).
pub(super) async fn pass_time(rig: &Rig, agent: &mut FakeAgent, secs: u64) {
    let whole_steps = usize::try_from(secs / 30).unwrap();
    for by in std::iter::repeat_n(30, whole_steps).chain([secs % 30]) {
        rig.advance(Duration::from_secs(by));
        beat(rig, agent).await;
    }
}

/// Passes over the queue until the engine has placed what it will: two ticks, since the first
/// may only queue a job in the engine and the second grant it a host.
pub(super) fn place_all(rig: &Rig) {
    rig.tick();
    rig.tick();
}

/// Records the share job `id` ( `wall_secs` long), has the engine place it on the host of
/// `agent`, and walks it to running there.
pub(super) async fn run_share(rig: &Rig, agent: &mut FakeAgent, id: &str, wall_secs: u64) -> JobId {
    let job = queue_share(rig, id, wall_secs);
    rig.tick();
    beat(rig, agent).await;
    agent.run(id);
    beat(rig, agent).await;
    assert_eq!(rig.job(&job).state, JobState::Running);
    job
}

/// Places the whole-host job `id` on the host of `agent` and has the agent prepare for it: the
/// job is placed with its allocation, and the agent has been told to prepare.
pub(super) async fn prepare(rig: &Rig, agent: &mut FakeAgent, id: &str) -> JobId {
    prepare_timed(rig, agent, id).await.0
}

/// [`prepare`], with the time of the beat the agent began preparing at: the beat is the host's
/// first listing since its hello, which restarts the prepare from that time.
async fn prepare_timed(rig: &Rig, agent: &mut FakeAgent, id: &str) -> (JobId, StoreTime) {
    let job = queue_whole(rig, id, 600);
    place_all(rig);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    // The deadline is read from the store's clock by the beat's listing, after the beat's own
    // row write moved it.
    let at = now(rig).saturating_add(BEAT_ROW);
    beat(rig, agent).await;
    assert_eq!(agent.phase, Phase::Preparing);
    (job, at)
}

/// The allocation id of `job`, as the agent is told it.
fn lease(rig: &Rig, job: &JobId) -> String {
    rig.allocation(job).allocation_id.to_string()
}

/// Asserts that the allocation of `job` was freed for `reason`.
fn assert_freed(rig: &Rig, job: &JobId, reason: EndReason) {
    let freed: AllocationRecord = rig.allocation(job);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(reason))
    );
}

/// A whole-host job waits queued while a running job holds the host, and the host is marked
/// for it, closed to the jobs behind it. Cancelled, the job ends at once, its mark is deleted in
/// the same commit, the running job is untouched, and the host takes the job that waited behind
/// the mark. It catches a cancel that leaves the mark behind (the host stays closed), one that
/// deletes it in a commit of its own, a mark that did not close the host, and a drain that cut
/// the running job.
#[tokio::test]
async fn cancel_while_marked_drops_the_mark_and_the_host_serves() {
    let (rig, mut agent) = served("whole_cancel_marked");
    let busy = run_share(&rig, &mut agent, "job-run", 7200).await;
    let whole = queue_whole(&rig, "job-w", 600);
    place_all(&rig);
    let mark = rig.jobs.get_mark(&whole).unwrap().unwrap();
    assert_eq!(mark.host.as_str(), "host-a");
    assert_eq!(rig.job(&whole).state, JobState::Queued);

    let waiting = queue_share(&rig, "job-q", 600);
    place_all(&rig);
    assert_eq!(rig.job(&waiting).state, JobState::Queued);

    let before = rig.commits();
    rig.jobs.request_cancel(&whole).unwrap();
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(rig.jobs.get_mark(&whole).unwrap(), None);
    assert_eq!(rig.job(&whole).state, JobState::Cancelled);

    place_all(&rig);
    assert_eq!(
        rig.states(&waiting),
        (JobState::Placed, AllocationStep::Booked)
    );
    assert_eq!(rig.allocation(&waiting).host.as_str(), "host-a");
    assert_eq!(rig.job(&busy).state, JobState::Running);
}

/// A whole-host job is cancelled while its host prepares. Nothing is freed while the agent is
/// still `preparing`: no stop is sent (the agent holds no container) and the allocation stays
/// booked, but the reply no longer names it, so the agent leaves `preparing`. At the first
/// heartbeat after that the job is cancelled and the allocation freed in one commit, and the
/// host takes the next job. It catches a stop sent to a preparing agent (which would answer
/// retired and free a host it still holds), a cancel that frees the host while the agent
/// prepares, one that never frees it, and a cancelled job whose host is still named.
#[tokio::test]
async fn cancel_while_preparing_frees_after_preparing_ends() {
    let (rig, mut agent) = served("whole_cancel_preparing");
    let (job, prepared_at) = prepare_timed(&rig, &mut agent, "job-w").await;
    let allocation = rig.allocation(&job);
    assert_eq!(allocation.booked, Booked::WholeHost);
    assert_eq!(
        allocation.prepare_deadline,
        Some(prepared_at.saturating_add(PREPARE))
    );
    assert_eq!(agent.exclusive, Some(lease(&rig, &job)));

    rig.jobs.request_cancel(&job).unwrap();
    let before = rig.commits();
    let reply = beat(&rig, &mut agent).await;
    assert_eq!((stops(&reply), starts(&reply).len()), (vec![], 0));
    assert_eq!(reply.exclusive, None);
    // Nothing of the job moves: only the beat's two row writes (beat_node, settle_node).
    assert_eq!(rig.commits(), before + 2);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    assert!(rig.job(&job).cancel_requested);
    assert_eq!(agent.phase, Phase::Restoring);

    let queued = queue_share(&rig, "job-q", 600);
    let before = rig.commits();
    let reply = beat(&rig, &mut agent).await;
    // The cancel's free, and the pass's queue status of the job it could not place.
    assert_eq!(rig.commits(), before + 4);
    assert_eq!(
        rig.job(&queued).queue.code(),
        js_types::QueueCode::WaitingForCapacity
    );
    assert_eq!(
        rig.states(&job),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_freed(&rig, &job, EndReason::Cancelled);
    assert_eq!((stops(&reply), starts(&reply).len()), (vec![], 0));
    assert_eq!(rig.job(&queued).state, JobState::Queued);

    agent.serve_again();
    beat(&rig, &mut agent).await;
    assert_eq!(
        rig.states(&queued),
        (JobState::Placed, AllocationStep::Booked)
    );
}

/// A host that was never told to prepare (it is serving) frees a cancelled placed whole-host
/// job at its next heartbeat. It catches a cancel that waits for a `preparing` that never
/// comes.
#[tokio::test]
async fn cancel_of_a_placed_job_the_agent_never_prepared_for_frees_at_once() {
    let (rig, agent) = served("whole_cancel_unprepared");
    let job = queue_whole(&rig, "job-w", 600);
    place_all(&rig);
    rig.jobs.request_cancel(&job).unwrap();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.exclusive, None);
    assert_eq!(
        rig.states(&job),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_freed(&rig, &job, EndReason::Cancelled);
}

/// A host refuses the prepare `not-quiet` and the job goes back to queued with its place, its
/// allocation freed and the host barred for it for the back-off, so a pass inside the back-off
/// does not place it again on the only host; after the back-off it is placed again, with a new
/// allocation, and the second refusal, the pool's `max_replace`, ends it failed to start with
/// `exclusive-not-ready`. It catches a refusal that waits for the prepare timeout, a requeue
/// that loses the queue position, a count or a bar that is not kept, a host not barred, and a
/// limit that is not the pool's.
#[tokio::test]
async fn failed_prepare_requeues_and_then_fails_after_max_replace() {
    let (rig, mut agent) = served("whole_failed_prepare");
    let job = prepare(&rig, &mut agent, "job-w").await;
    let first = lease(&rig, &job);
    let submitted = rig.job(&job).submitted_at;
    agent.refuse(RefusalReason::NotQuiet);
    let before = rig.commits();
    let reply = beat(&rig, &mut agent).await;
    // The requeue, and the pass's queue status of the requeued job, which waits for a whole
    // host again, and the beat's two row writes (beat_node, settle_node).
    assert_eq!(rig.commits(), before + 4);
    assert_eq!(reply.exclusive, None);
    let requeued = rig.job(&job);
    assert_eq!(requeued.state, JobState::Queued);
    assert_eq!(
        requeued.queue.code(),
        js_types::QueueCode::WaitingForWholeHost
    );
    let kinds: Vec<js_types::JobEventKind> = requeued
        .queue
        .events()
        .iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            js_types::JobEventKind::Placed,
            js_types::JobEventKind::Requeued
        ]
    );
    assert_eq!(requeued.queue.attempts(), 1);
    assert_eq!(
        (requeued.failed_prepares, requeued.submitted_at),
        (1, submitted)
    );
    assert_eq!(requeued.host, None);
    let barred: Vec<&str> = requeued.prepare_bars.keys().map(|h| h.as_str()).collect();
    assert_eq!(barred, ["host-a"]);
    assert_freed(&rig, &job, EndReason::FailedPrepare);
    assert_eq!(agent.phase, Phase::Restoring);

    // The only host is barred for the back-off: a pass inside it places nothing.
    agent.serve_again();
    pass_time(&rig, &mut agent, 270).await;
    place_all(&rig);
    assert_eq!(rig.job(&job).state, JobState::Queued);
    assert_eq!(rig.allocation(&job).allocation_id.to_string(), first);

    pass_time(&rig, &mut agent, 31).await;
    place_all(&rig);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    let second = lease(&rig, &job);
    assert_ne!(second, first);
    beat(&rig, &mut agent).await;
    assert_eq!(agent.exclusive, Some(second));
    agent.refuse(RefusalReason::NotQuiet);
    beat(&rig, &mut agent).await;
    let ended = rig.job(&job);
    assert_eq!(
        ended.state,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady)
    );
    assert_eq!(ended.failed_prepares, 2);
    assert_freed(&rig, &job, EndReason::FailedPrepare);
}

/// A host that is silent never reports ready: at the tick after the prepare deadline the
/// prepare fails, the job goes back to queued and the allocation is freed, as for a refusal;
/// before the deadline the tick changes nothing. It catches a deadline that is not the
/// placement's time plus `prepare_timeout`, one checked a tick late, and a timeout that does
/// not count toward `max_replace`.
#[tokio::test]
async fn a_silent_agent_fails_the_prepare_at_its_deadline() {
    let (rig, mut agent) = served("whole_prepare_timeout");
    let job = prepare(&rig, &mut agent, "job-w").await;
    pass_time(&rig, &mut agent, 599).await;
    rig.tick();
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    rig.advance(SECOND);
    rig.tick();
    let requeued = rig.job(&job);
    assert_eq!(
        (requeued.state, requeued.failed_prepares),
        (JobState::Queued, 1)
    );
    assert_freed(&rig, &job, EndReason::FailedPrepare);
    assert_eq!(requeued.prepare_bars.len(), 1);
}

/// The agent reports the host ready after the prepare deadline but before the tick that would
/// fail it: the report wins. The job is starting, its start rides on the reply that moved it
/// (the whole host's CPU and memory, the job's wall limit, the allocation's id), and the tick
/// that follows fails nothing. It catches a deadline enforced at the heartbeat, one that undoes
/// a start, and a start built from a share.
#[tokio::test]
async fn a_ready_report_after_the_deadline_but_before_the_tick_wins() {
    let (rig, mut agent) = served("whole_ready_wins");
    let job = prepare(&rig, &mut agent, "job-w").await;
    pass_time(&rig, &mut agent, 601).await;
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    agent.report_ready();
    let reply = beat(&rig, &mut agent).await;
    assert_eq!(
        rig.states(&job),
        (JobState::Starting, AllocationStep::Booked)
    );
    let sent = starts(&reply);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].allocation_id, lease(&rig, &job));
    assert_eq!(
        (sent[0].booked.cpu_millis, sent[0].booked.memory_bytes),
        (8000, 17_179_869_184)
    );
    assert_eq!(sent[0].wall_ms, 600_000);
    assert_eq!(reply.exclusive, Some(lease(&rig, &job)));

    rig.tick();
    assert_eq!(
        rig.states(&job),
        (JobState::Starting, AllocationStep::Booked)
    );
    agent.run("job-w");
    beat(&rig, &mut agent).await;
    assert_eq!(
        rig.states(&job),
        (JobState::Running, AllocationStep::Booked)
    );
    assert_eq!(rig.job(&job).failed_prepares, 0);
}

/// The tick fails the prepare at its deadline and the agent's ready report arrives after it:
/// the report names an allocation that is freed and changes nothing, so the job stays queued
/// (barred from the host) and no start is sent. It catches a late report that revives a freed
/// allocation or starts a requeued job.
#[tokio::test]
async fn a_ready_report_after_the_tick_is_too_late() {
    let (rig, mut agent) = served("whole_ready_late");
    let job = prepare(&rig, &mut agent, "job-w").await;
    pass_time(&rig, &mut agent, 600).await;
    rig.tick();
    assert_eq!(rig.job(&job).state, JobState::Queued);
    agent.report_ready();
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(reply.exclusive, None);
    assert_eq!(rig.job(&job).state, JobState::Queued);
    assert_freed(&rig, &job, EndReason::FailedPrepare);
}

/// The agent refuses the host `host-unsafe`: the job goes back to queued with no bar of its
/// own, the host is tainted in the registry, and no pass places the job on it. It catches a
/// taint that stops at the job (the host would be offered again) and a refusal that bars the
/// host as a back-off.
#[tokio::test]
async fn a_host_unsafe_refusal_taints_the_host() {
    let (rig, mut agent) = served("whole_host_unsafe");
    let job = prepare(&rig, &mut agent, "job-w").await;
    agent.refuse(RefusalReason::HostUnsafe);
    beat(&rig, &mut agent).await;
    let requeued = rig.job(&job);
    assert_eq!(
        (requeued.state, requeued.failed_prepares),
        (JobState::Queued, 1)
    );
    assert!(requeued.prepare_bars.is_empty());
    let host = js_types::HostId::new("host-a").unwrap();
    let view = rig.agents.registry.own_host(&host).unwrap();
    assert!(view.host.taint.is_some());
    agent.serve_again();
    pass_time(&rig, &mut agent, 600).await;
    place_all(&rig);
    assert_eq!(rig.job(&job).state, JobState::Queued);
}

/// Two servers over one store. `host-a` said hello to the second, which placed the whole-host
/// job `job-w` on it, and the agent prepares. A beat of the agent (one that sends no `tainted` of
/// its own) reaches the first server and reports a taint, which the first stores in the host's
/// binding; the next beat reaches the second and reports the host ready. The second server's
/// registry holds no taint for the host (it read the binding at the hello, before the taint),
/// but the start reads the host's rows: the job stays placed, no start is sent, and the reply
/// still names the allocation. It catches a whole-host start judged on one server's registry.
#[tokio::test]
async fn a_ready_report_on_a_host_another_server_tainted_starts_nothing() {
    let mut first = whole_rig("whole_taint_first", rules(), None);
    let second = (rules(), None);
    let mut second = whole_over("whole_taint_second", second, Some(&first), START_MS);
    second.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let job = prepare(&second, &mut agent, "job-w").await;

    let session = second.sessions["host-a"].clone();
    first.sessions.insert("host-a".to_owned(), session);
    let taint = Taint {
        reason: "residue".to_owned(),
        run_id: Some("r1".to_owned()),
        check: None,
    };
    let _ = first.beat_reporting(&agent, Some(taint)).await;
    let binding = first.store.get("hosts/binding/host-a").unwrap().unwrap();
    let binding: serde_json::Value = serde_json::from_slice(&binding.value).unwrap();
    assert_eq!(
        binding["taint_reason"], "residue",
        "the first server stored the taint"
    );
    let host = js_types::HostId::new("host-a").unwrap();
    let view = second.agents.registry.own_host(&host).unwrap();
    assert_eq!(
        view.host.taint, None,
        "the second server's registry holds no taint"
    );

    agent.report_ready();
    let reply = beat(&second, &mut agent).await;
    assert_eq!(
        second.states(&job),
        (JobState::Placed, AllocationStep::Booked)
    );
    assert!(starts(&reply).is_empty());
    assert_eq!(reply.exclusive, Some(lease(&second, &job)));
}

/// The host `name` as the registry holds it is tainted, and the taint's reason.
fn taint_reason(rig: &Rig, name: &str) -> Option<String> {
    let host = js_types::HostId::new(name).unwrap();
    let view = rig.agents.registry.own_host(&host).unwrap();
    view.host.taint.map(|taint| taint.reason)
}

/// Two hosts; `host-a` refuses its prepare `host-unsafe`: the job goes back to the queue with
/// no bar of its own and `host-a` is tainted, and the pass after the refusing heartbeat places
/// it on `host-b`, which is idle, where it is prepared and starts; `host-a` is named no
/// allocation again. It catches komira-ai/jobscheduler#834, a requeued job left on the
/// tainted host's grant that stays queued while `host-b` is idle.
#[tokio::test]
async fn a_host_unsafe_refusal_places_the_job_on_the_other_host() {
    let (mut rig, mut a) = served("whole_host_unsafe_other");
    rig.register("host-b", true);
    let mut b = FakeAgent::new("host-b");
    beat(&rig, &mut b).await;
    let job = prepare(&rig, &mut a, "job-w").await;
    assert_eq!(rig.allocation(&job).host.as_str(), "host-a");
    a.refuse(RefusalReason::HostUnsafe);
    beat(&rig, &mut a).await;
    let requeued = rig.job(&job);
    assert_eq!(
        (requeued.state, requeued.failed_prepares),
        (JobState::Placed, 1)
    );
    assert!(requeued.prepare_bars.is_empty());
    assert_eq!(rig.allocation(&job).host.as_str(), "host-b");
    a.serve_again();
    place_all(&rig);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    let reply = beat(&rig, &mut a).await;
    assert_eq!(reply.exclusive, None);
    let reply = beat(&rig, &mut b).await;
    assert_eq!(reply.exclusive, Some(lease(&rig, &job)));
    assert_eq!(b.phase, Phase::Preparing);
    b.report_ready();
    beat(&rig, &mut b).await;
    assert_eq!(rig.job(&job).state, JobState::Starting);
    assert!(taint_reason(&rig, "host-a").is_some());
    assert_eq!(taint_reason(&rig, "host-b"), None);
}

/// Every host refuses its prepare `host-unsafe`, one after the other, under a pool that ends a
/// job only after three failed prepares: the first refusal sends the job to the other host,
/// and after the second both hosts are tainted and the job stays queued. The hosts' taints say
/// `host-unsafe`, and the job's trail holds each placement and each requeue, naming its host.
/// The passes that follow over ten minutes commit nothing (only the heartbeats' own row writes
/// do), name no allocation to either host, and leave the job's row as it was. It catches a job
/// ended because every host is tainted, one whose refusals leave no trace, and one that is
/// marked, granted or rewritten on every pass.
#[tokio::test]
async fn a_job_every_host_refused_host_unsafe_stays_queued_and_writes_nothing() {
    let rules = WholeNodeConfig {
        max_replace: 3,
        ..WholeNodeConfig::dedicated(2)
    };
    let mut rig = whole_rig("whole_host_unsafe_every", rules, None);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agents = [FakeAgent::new("host-a"), FakeAgent::new("host-b")];
    let job = queue_whole(&rig, "job-w", 600);
    place_all(&rig);
    let mut refused = Vec::new();
    for (round, after) in [(1, JobState::Placed), (2, JobState::Queued)] {
        let on = rig.allocation(&job).host.to_string();
        let agent = agents.iter_mut().find(|agent| agent.host == on).unwrap();
        beat(&rig, agent).await;
        assert_eq!(agent.phase, Phase::Preparing);
        agent.refuse(RefusalReason::HostUnsafe);
        beat(&rig, agent).await;
        agent.serve_again();
        refused.push(on);
        let requeued = rig.job(&job);
        assert_eq!((requeued.state, requeued.failed_prepares), (after, round));
    }
    let mut both = refused.clone();
    both.sort();
    assert_eq!(both, ["host-a", "host-b"]);
    let unsafe_host = Some("host-unsafe".to_owned());
    assert_eq!(taint_reason(&rig, "host-a"), unsafe_host);
    assert_eq!(taint_reason(&rig, "host-b"), unsafe_host);
    let noted = rig.job(&job).queue;
    assert_eq!(noted.code(), js_types::QueueCode::WaitingForWholeHost);
    let trail: Vec<(JobEventKind, &str, Option<&str>)> = noted
        .events()
        .iter()
        .map(|event| {
            let host = event.host.as_ref().map(js_types::HostId::as_str);
            (event.kind, event.detail.as_str(), host)
        })
        .collect();
    let requeued = |host| {
        let detail = match host {
            "host-a" => "the whole-host prepare on host host-a failed; the job is queued again",
            _ => "the whole-host prepare on host host-b failed; the job is queued again",
        };
        (JobEventKind::Requeued, detail, Some(host))
    };
    let placed = |host| {
        let detail = match host {
            "host-a" => "placed on host host-a",
            _ => "placed on host host-b",
        };
        (JobEventKind::Placed, detail, Some(host))
    };
    let (first, second) = (refused[0].as_str(), refused[1].as_str());
    assert_eq!(
        trail,
        [
            placed(first),
            requeued(first),
            placed(second),
            requeued(second)
        ]
    );

    for _ in 0..20 {
        rig.advance(30 * SECOND);
        for agent in &mut agents {
            let reply = beat(&rig, agent).await;
            assert_eq!(reply.exclusive, None);
        }
        let before = rig.commits();
        place_all(&rig);
        assert_eq!(rig.commits(), before);
    }
    let still = rig.job(&job);
    assert_eq!((still.state, still.failed_prepares), (JobState::Queued, 2));
    assert_eq!(still.queue, noted);
}

/// A hello restarts the prepare of a placed whole-host job: the first listing after it moves
/// the allocation's prepare deadline to the hello's time plus `prepare_timeout`, the allocation
/// and job staying as they are. The first deadline then passes without failing the prepare, and
/// the new one fails it. It catches a hello that leaves the clock where the placement set it,
/// one that restarts it from another instant, and a hello that requeues the job.
#[tokio::test]
async fn a_hello_restarts_the_prepare_of_a_placed_whole_host_job() {
    let (mut rig, mut agent) = served("whole_hello_placed");
    let (job, prepared_at) = prepare_timed(&rig, &mut agent, "job-w").await;
    let placed = rig.allocation(&job);
    let first_deadline = prepared_at.saturating_add(PREPARE);
    assert_eq!(placed.prepare_deadline, Some(first_deadline));

    // The agent restarts: the host is silent past the grace, then says hello.
    rig.advance(120 * SECOND);
    rig.register("host-a", true);
    // The beat's listing reads the clock after the beat's own row write moved it.
    let hello_at = now(&rig).saturating_add(BEAT_ROW);
    rig.beat(&agent).await.unwrap();
    let restarted = rig.allocation(&job);
    assert_eq!(
        restarted.prepare_deadline,
        Some(hello_at.saturating_add(PREPARE))
    );
    assert_eq!(restarted.allocation_id, placed.allocation_id);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));

    rig.advance(PREPARE - 120 * SECOND);
    rig.tick();
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    rig.advance(120 * SECOND);
    rig.tick();
    assert_eq!(rig.job(&job).state, JobState::Queued);
    assert_freed(&rig, &job, EndReason::FailedPrepare);
}

/// A hello cannot reset a job that is already running. With the container still listed the
/// job keeps its state and its allocation, whose prepare deadline is not moved, and the host
/// goes on being named; when the agent restarted and lists nothing, the job is lost and its
/// allocation freed, and it is not requeued. It catches a hello that restarts the prepare of a
/// running job, one that requeues it or frees its host, and a loss of a job the host lists.
#[tokio::test]
async fn a_hello_does_not_reset_a_running_whole_host_job() {
    let (mut rig, mut agent) = served("whole_hello_running");
    let job = prepare(&rig, &mut agent, "job-w").await;
    agent.report_ready();
    beat(&rig, &mut agent).await;
    assert_eq!(rig.job(&job).state, JobState::Starting);
    agent.run("job-w");
    beat(&rig, &mut agent).await;
    assert_eq!(
        rig.states(&job),
        (JobState::Running, AllocationStep::Booked)
    );
    let running = rig.allocation(&job);

    rig.advance(120 * SECOND);
    rig.register("host-a", true);
    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    // Nothing of the job moves: only the beat's two row writes (beat_node, settle_node).
    assert_eq!(rig.commits(), before + 2);
    assert_eq!(reply.exclusive, Some(lease(&rig, &job)));
    assert_eq!(
        rig.states(&job),
        (JobState::Running, AllocationStep::Booked)
    );
    assert_eq!(rig.allocation(&job), running);

    // The agent restarted and lists nothing: the container went with it.
    rig.advance(120 * SECOND);
    rig.register("host-a", true);
    let restarted = FakeAgent::new("host-a");
    let reply = rig.beat(&restarted).await.unwrap();
    assert_eq!(reply.exclusive, None);
    assert_eq!(rig.states(&job), (JobState::Lost, AllocationStep::Freed));
    assert_eq!(rig.job(&job).failed_prepares, 0);
}

/// A whole-host job runs to its end like any job: the agent reports its exit, the job ends and
/// is acknowledged, the allocation is freed once the host no longer lists it, the reply stops
/// naming the host, and the host takes the next job. It catches a whole-host job whose end
/// leaves the host booked or named.
#[tokio::test]
async fn a_whole_host_job_that_ran_frees_the_host_when_it_ends() {
    let (rig, mut agent) = served("whole_ends");
    let job = prepare(&rig, &mut agent, "job-w").await;
    agent.report_ready();
    beat(&rig, &mut agent).await;
    agent.run("job-w");
    beat(&rig, &mut agent).await;
    agent.end("job-w", Outcome::Succeeded, Some(0));
    beat(&rig, &mut agent).await;
    assert_eq!(rig.job(&job).state, JobState::Succeeded);
    let reply = beat(&rig, &mut agent).await;
    assert_eq!(
        rig.states(&job),
        (JobState::Succeeded, AllocationStep::Freed)
    );
    assert_eq!(reply.exclusive, None);
    assert_freed(&rig, &job, EndReason::Exited);
}
