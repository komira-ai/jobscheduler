//! Each path that makes an event tells the agent side's meter of it: the job pass, the pass
//! over the queued jobs, and a host's heartbeat. The registry, the server and the store read
//! one manual clock; the meter's clock moves 7 ms at each reading, so a timed pass took
//! exactly 7 ms, and nothing here sleeps.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{HeldState, Outcome, RefusalReason};
use js_jobs::{JobId, JobState, NewJob, StartFailure};
use js_registry::WholeNodeConfig;
use js_types::HostId;
use js_types::Monotonic;

use super::*;
use crate::agent_api::job_rig::{FakeAgent, Rig, clocked_rig, spec, whole_rig};
use crate::job_expiry::JobLimits;

/// The meter's clock: each reading is 7 ms after the one before it.
#[derive(Debug, Default)]
struct Stepping(Mutex<Duration>);

impl Monotonic for Stepping {
    fn reading(&self) -> Duration {
        let mut at = self.0.lock().unwrap();
        *at += Duration::from_millis(7);
        *at
    }
}

/// A recorder the agent side of `rig` counts its events on, timed by [`Stepping`].
fn metered(rig: &Rig) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder::default());
    let meter = Meter::new(recorder.clone(), Arc::new(Stepping::default()));
    rig.agents.set_meter(meter);
    recorder
}

/// What `recorder` counted: start failures by code, losses and failed writes.
fn counted(recorder: &Recorder) -> ([u64; 4], u64, u64) {
    let failures = recorder.start_failures.each_ref();
    let failures = failures.map(|count| count.load(Ordering::Relaxed));
    let lost = recorder.lost.load(Ordering::Relaxed);
    (
        failures,
        lost,
        recorder.store_write_failures.load(Ordering::Relaxed),
    )
}

fn queue(rig: &Rig, id: &str) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// Asserts what a job pass counts: each job it fails to start, by its code (a queued job past
/// its queue wait is `no-capacity`, a placed one past its start timeout `start-timeout`), its
/// duration by the meter's clock, and a pass that fails as a failed write, still timed, its
/// start failures committed before the failure counted. It catches a pass whose start failures
/// are not counted or counted under the wrong code, a failed pass left uncounted, and the ends
/// of a failed pass that committed and are never counted.
#[test]
fn a_job_pass_counts_its_start_failures_its_duration_and_its_failure() {
    let limits = JobLimits {
        max_queue_wait: Duration::from_secs(600),
        start_timeout: Duration::from_secs(300),
    };
    let rig = clocked_rig("metrics_pass", false, limits);
    let recorder = metered(&rig);
    queue(&rig, "waits-too-long");
    let placed = queue(&rig, "starts-too-late");
    let booked = crate::agent_api::job_rig::BOOKED;
    rig.jobs
        .place(&placed, HostId::new("host-a").unwrap(), booked)
        .unwrap();
    rig.advance(Duration::from_secs(660));
    rig.agents.job_pass();
    assert_eq!(counted(&recorder), ([1, 1, 0, 0], 0, 0));
    let (below, passes, sum_micros) = recorder.job_pass.read();
    assert_eq!((below[1], passes, sum_micros), (("0.005", 0), 1, 7_000));
    rig.store.fail_read_after(0);
    rig.agents.job_pass();
    assert_eq!(counted(&recorder), ([1, 1, 0, 0], 0, 1));
    assert_eq!(recorder.job_pass.read().1, 2);
    // A pass that ends one job and fails at the next commit still counts the one it ended.
    queue(&rig, "ends-first");
    queue(&rig, "fails-second");
    rig.advance(Duration::from_secs(660));
    rig.store.fail_after(1);
    rig.agents.job_pass();
    assert_eq!(counted(&recorder), ([2, 1, 0, 0], 0, 2));
}

/// Asserts what a pass over the queued jobs counts: a placement, with the time its job waited
/// from its submit (40 s on the manual clock, and 2 ms of the store's own clock for the
/// commit of the submit) in the queue wait histogram; and a placement whose commit fails as a
/// failed write, not as a placement. It catches a wait measured from the wrong instant and a
/// failed placement counted as made.
#[tokio::test]
async fn a_placement_counts_its_wait_and_one_that_does_not_commit_a_failed_write() {
    let mut rig = clocked_rig("metrics_place", true, JobLimits::default());
    rig.register("host-a", true);
    let recorder = metered(&rig);
    queue(&rig, "waits-forty");
    rig.advance(Duration::from_secs(40));
    // The host is heard again first: it has been silent past its grace.
    let mut agent = FakeAgent::new("host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    let (below, placed, sum_micros) = recorder.queue_wait.read();
    assert_eq!((below[2], below[3]), (("30", 0), ("60", 1)));
    assert_eq!((placed, sum_micros), (1, 40_002_000));
    assert_eq!(recorder.placed.load(Ordering::Relaxed), 1);
    queue(&rig, "not-committed");
    rig.store.fail_next();
    rig.agents.place_jobs();
    assert_eq!(
        rig.job(&JobId::parse("not-committed").unwrap()).state,
        JobState::Queued
    );
    assert_eq!(recorder.placed.load(Ordering::Relaxed), 1);
    assert_eq!(counted(&recorder), ([0; 4], 0, 1));
}

/// Takes `host` out of the registry as the scaler does when its provider reads it gone.
fn remove(rig: &Rig, host: &str) {
    let host = HostId::new(host).unwrap();
    let registry = &rig.agents.registry;
    registry.drain(&host).unwrap();
    registry.release_host(&host).unwrap();
    registry.remove(&host).unwrap();
}

/// A running job of `host`'s, as its agent `agent` lists it.
async fn running(rig: &Rig, agent: &mut FakeAgent, id: &str) -> JobId {
    let job = rig.place(id, &agent.host);
    let allocation = rig.allocation(&job).allocation_id.to_string();
    agent.hold(id, &allocation, HeldState::Running);
    rig.beat(agent).await.unwrap();
    assert_eq!(rig.job(&job).state, JobState::Running);
    job
}

/// Asserts what a heartbeat counts, and the losses of a job pass: a start the agent refused,
/// as `agent-refused`; a running job of a host the registry removed, lost by the pass; a
/// running job the first listing after a new hello does not name, lost by the heartbeat; and
/// a listing whose commit fails, as a failed write. It catches a start failure or a loss that
/// one of the two paths makes and does not count.
#[tokio::test]
async fn a_heartbeat_counts_a_refused_start_a_lost_job_and_a_listing_that_does_not_commit() {
    let mut rig = clocked_rig("metrics_beat", true, JobLimits::default());
    rig.register("host-a", true);
    rig.register("host-b", true);
    let recorder = metered(&rig);
    let (mut on_a, mut on_b) = (FakeAgent::new("host-a"), FakeAgent::new("host-b"));
    let refused = rig.place("refused", "host-a");
    let allocation = rig.allocation(&refused).allocation_id.to_string();
    on_a.hold("refused", &allocation, HeldState::ExitedUnacked);
    on_a.end("refused", Outcome::FailedToStart, None);
    rig.beat(&on_a).await.unwrap();
    let ended = JobState::FailedToStart(StartFailure::AgentRefused);
    assert_eq!(rig.job(&refused).state, ended);
    assert_eq!(counted(&recorder), ([0, 0, 0, 1], 0, 0));
    let gone_with_a = running(&rig, &mut on_a, "gone-with-a").await;
    let gone_with_b = running(&rig, &mut on_b, "gone-with-b").await;
    remove(&rig, "host-a");
    rig.tick();
    assert_eq!(rig.job(&gone_with_a).state, JobState::Lost);
    assert_eq!(counted(&recorder), ([0, 0, 0, 1], 1, 0));
    remove(&rig, "host-b");
    rig.register("host-b", true);
    let mut fresh = FakeAgent::new("host-b");
    fresh.take(&rig.beat(&fresh).await.unwrap());
    assert_eq!(rig.job(&gone_with_b).state, JobState::Lost);
    assert_eq!(counted(&recorder), ([0, 0, 0, 1], 2, 0));
    rig.place("unwritten", "host-b");
    fresh.take(&rig.beat(&fresh).await.unwrap());
    rig.store.fail_next();
    assert!(rig.beat(&fresh).await.is_err());
    assert_eq!(counted(&recorder), ([0, 0, 0, 1], 2, 1));
}

/// Asserts that a whole-host job whose prepare fails for the last time the pool allows ends
/// failed to start, and that the heartbeat that refused it counts it once, as
/// `exclusive-not-ready` (the third code). It catches a terminal failed prepare that is not
/// counted and one counted as another code.
#[tokio::test]
async fn a_terminal_failed_prepare_counts_exclusive_not_ready() {
    let rules = WholeNodeConfig {
        max_replace: 1,
        ..WholeNodeConfig::dedicated(2)
    };
    let mut rig = whole_rig("metrics_prepare", rules, None);
    rig.register("host-a", true);
    let recorder = metered(&rig);
    let mut agent = FakeAgent::new("host-a");
    let new = NewJob {
        id: Some(JobId::parse("whole").unwrap()),
        spec: js_jobs::JobSpec {
            needs: js_jobs::Needs::WholeHost,
            ..spec()
        },
        priority: 0,
        client: None,
    };
    let job = rig.jobs.insert_queued(new).unwrap().record.id;
    rig.tick();
    rig.tick();
    agent.take(&rig.beat(&agent).await.unwrap());
    assert_eq!(counted(&recorder), ([0; 4], 0, 0));
    agent.refuse(RefusalReason::NotQuiet);
    agent.take(&rig.beat(&agent).await.unwrap());
    let ended = JobState::FailedToStart(StartFailure::ExclusiveNotReady);
    assert_eq!(rig.job(&job).state, ended);
    assert_eq!(counted(&recorder), ([0, 0, 1, 0], 0, 0));
}

/// Asserts that a meter set once stays: a second one given to the agent side is ignored, so
/// the events go on to the first recorder.
#[test]
fn the_first_meter_of_the_agent_side_stays() {
    let rig = clocked_rig("metrics_once", false, JobLimits::default());
    let first = metered(&rig);
    let second = metered(&rig);
    rig.agents.meter().sink().job_lost();
    assert_eq!(
        (
            first.lost.load(Ordering::Relaxed),
            second.lost.load(Ordering::Relaxed)
        ),
        (1, 0)
    );
}
