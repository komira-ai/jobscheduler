//! What the pass over the queued jobs costs a heartbeat ([`Agents::place_jobs_after_beat`]):
//! the job table reads it makes and the placement rounds it runs, counted per heartbeat. A
//! heartbeat that changes nothing the pass depends on reads no job table and runs no round;
//! one that frees a host reads the queued rows and no other job's, and runs a round only for
//! a job that has a host left to try; and each thing a pass depends on brings the next pass
//! back: a job submitted or cancelled, a host that says hello or goes lost, a write of the
//! last pass that failed, and what pass 2 told. The tick's pass always runs.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Outcome;
use js_jobs::interfere::Read;
use js_jobs::{JobId, JobSpec, JobState, Needs, NewJob};
use js_sched::queue_status::Outcome as Told;
use js_types::{HostId, Resources};

use js_agent_wire::HeartbeatReply;

use crate::agent_api::job_rig::{FakeAgent, Rig, clocked_rig, spec};
use crate::job_expiry::JobLimits;
use crate::sched::place::SchedPlacer;

/// The jobs queued, in queue order (one priority and one submit time, so by id): each asks for
/// every core of a host, so a host runs one at a time.
const QUEUED: [&str; 5] = ["a-first", "b-second", "c-third", "d-fourth", "e-fifth"];

/// A job that asks for all eight cores of a rig's host.
fn whole_cores() -> JobSpec {
    let cores = Resources::new(8000, 1 << 30, 0, None).unwrap();
    JobSpec {
        needs: Needs::Shape(cores),
        ..spec()
    }
}

/// Records the queued job `id`, asking for every core of a host, at `priority`.
fn queue(rig: &Rig, id: &str, priority: i32) -> JobId {
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec: whole_cores(),
        priority,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap().record.id
}

/// A server with two hosts, `host-a` and `host-b`, each running one of [`QUEUED`] (the first
/// two) and the other three queued behind them; every heartbeat since has been answered, so
/// the next one of either host, with an unchanged listing, changes nothing.
struct Settled {
    rig: Rig,
    a: FakeAgent,
    b: FakeAgent,
}

impl Settled {
    async fn new(name: &str) -> Settled {
        let mut rig = clocked_rig(name, true, JobLimits::default());
        rig.register("host-a", true);
        rig.register("host-b", true);
        for id in QUEUED {
            queue(&rig, id, 0);
        }
        rig.tick();
        let (mut a, mut b) = (FakeAgent::new("host-a"), FakeAgent::new("host-b"));
        for agent in [&mut a, &mut b] {
            agent.take(&rig.beat(agent).await.unwrap());
            let held: Vec<String> = agent.listing().into_iter().map(|j| j.job_id).collect();
            assert_eq!(held.len(), 1, "each host was started one job: {held:?}");
            agent.run(&held[0]);
            agent.take(&rig.beat(agent).await.unwrap());
        }
        let settled = Settled { rig, a, b };
        for id in &QUEUED[..2] {
            assert_eq!(settled.state(id), JobState::Running, "{id}");
        }
        for id in &QUEUED[2..] {
            assert_eq!(settled.state(id), JobState::Queued, "{id}");
        }
        // One beat each with nothing new, so the last pass saw what the next beat sees.
        settled.quiet_beats().await;
        settled
    }

    fn placer(&self) -> &Arc<SchedPlacer> {
        self.rig.agents.placer.as_ref().unwrap()
    }

    fn state(&self, id: &str) -> JobState {
        self.rig.job(&JobId::parse(id).unwrap()).state
    }

    /// The job `host`'s agent runs.
    fn running_on(&self, host: &str) -> String {
        let agent = if host == "host-a" { &self.a } else { &self.b };
        let listed = agent.listing();
        listed.into_iter().map(|job| job.job_id).next().unwrap()
    }

    /// A heartbeat of each host with its listing as it was.
    async fn quiet_beats(&self) {
        self.rig.beat(&self.a).await.unwrap();
        self.rig.beat(&self.b).await.unwrap();
    }

    /// What `act` answered, with what it read of the store, the placement rounds it ran and
    /// the passes it skipped.
    async fn counted<T, F: std::future::Future<Output = T>>(&self, act: F) -> (T, Counted) {
        let (rounds, skipped) = (self.placer().job_rounds(), self.placer().passes_skipped());
        let _ = self.rig.store.take_reads();
        let answer = act.await;
        let counted = Counted {
            reads: self.rig.store.take_reads(),
            rounds: self.placer().job_rounds() - rounds,
            skipped: self.placer().passes_skipped() - skipped,
        };
        (answer, counted)
    }

    /// What a heartbeat of `agent`'s host costs, with its reply.
    async fn beat(&self, agent: &FakeAgent) -> (HeartbeatReply, Counted) {
        let (reply, counted) = self.counted(self.rig.beat(agent)).await;
        (reply.unwrap(), counted)
    }
}

/// What one heartbeat or tick cost.
#[derive(Debug)]
struct Counted {
    reads: Vec<Read>,
    rounds: u64,
    skipped: u64,
}

impl Counted {
    /// The scans of a whole job table space: what a pass reads and a heartbeat's own half
    /// (which reads its host's rows alone, beside the rows this build cannot read) never does.
    fn table_scans(&self) -> Vec<&str> {
        const WHOLE: [&str; 4] = [
            "idx/job/active/",
            "idx/job/queued/",
            "idx/alloc/host/",
            "mark/",
        ];
        let scans = self.reads.iter().filter_map(|read| match read {
            Read::Scan(prefix) => WHOLE.contains(&prefix.as_str()).then_some(prefix.as_str()),
            Read::Get(_) => None,
        });
        scans.collect()
    }

    /// Whether the row of the job `id` was read.
    fn read_job(&self, id: &str) -> bool {
        self.reads.contains(&Read::Get(format!("job/{id}")))
    }
}

/// Asserts that a heartbeat with an unchanged listing, from a host whose state the registry
/// holds as it was, runs no pass: it reads no job table space and none of the queued jobs'
/// rows, and runs no placement round. Before, every such heartbeat read every active job and
/// ran a round for every queued job.
#[tokio::test]
async fn a_heartbeat_that_changes_nothing_reads_no_job_table_and_runs_no_round() {
    let settled = Settled::new("cost_quiet").await;
    for agent in [&settled.a, &settled.b] {
        let (_, counted) = settled.beat(agent).await;
        assert_eq!(counted.table_scans(), Vec::<&str>::new(), "{counted:?}");
        for id in &QUEUED[2..] {
            assert!(!counted.read_job(id), "{id} was read: {counted:?}");
        }
        assert_eq!((counted.rounds, counted.skipped), (0, 1), "{counted:?}");
    }
}

/// Asserts what the heartbeats that end and then free a host's job cost. The beat that reports
/// the result ends the job, whose allocation still books the host: the pass reads the queued
/// rows and not the row of the job running on the other host, and runs one round, for the
/// first queued job, which then closes both hosts to the two behind it. The next beat, which
/// no longer lists the job, frees it: the pass places the first queued job on the freed host
/// in one round, and runs a second for the next job, which then closes the hosts to the last.
#[tokio::test]
async fn a_heartbeat_that_frees_a_host_reads_only_the_queued_rows_and_places_the_first_on_it() {
    let mut settled = Settled::new("cost_free").await;
    let ending = settled.running_on("host-a");
    let other = settled.running_on("host-b");
    settled.a.end(&ending, Outcome::Succeeded, Some(0));
    let (reply, ended) = settled.beat(&settled.a).await;
    assert_eq!(settled.state(&ending), JobState::Succeeded);
    assert_eq!(settled.state("c-third"), JobState::Queued);
    assert!(
        !ended.read_job(&other),
        "the other host's job was read: {ended:?}"
    );
    assert_eq!((ended.rounds, ended.skipped), (1, 0), "{ended:?}");
    settled.a.take(&reply);
    let (_, freed) = settled.beat(&settled.a).await;
    assert_eq!(settled.state("c-third"), JobState::Placed);
    let host = settled
        .rig
        .allocation(&JobId::parse("c-third").unwrap())
        .host;
    assert_eq!(host, HostId::new("host-a").unwrap());
    assert!(
        !freed.read_job(&other),
        "the other host's job was read: {freed:?}"
    );
    for id in &QUEUED[2..] {
        assert!(freed.read_job(id), "{id} was not read: {freed:?}");
    }
    assert_eq!((freed.rounds, freed.skipped), (2, 0), "{freed:?}");
    assert_eq!(settled.state("d-fourth"), JobState::Queued);
}

/// Asserts that the tick's pass runs after a heartbeat's was skipped, running the one round
/// the first queued job has a host for; and that the pass reads no job table when no write
/// came since the last pass read them: it uses the rows that pass read. (The tick's job pass,
/// before it, reads the tables for its own ends.)
#[tokio::test]
async fn the_tick_runs_its_pass_and_reads_no_table_that_did_not_change() {
    let settled = Settled::new("cost_tick").await;
    let (_, quiet) = settled.counted(settled.quiet_beats()).await;
    assert_eq!(quiet.skipped, 2, "{quiet:?}");
    let (_, ticked) = settled.counted(async { settled.rig.tick() }).await;
    assert_eq!((ticked.rounds, ticked.skipped), (1, 0), "{ticked:?}");
    let pass = async { settled.rig.agents.place_jobs() };
    let (_, passed) = settled.counted(pass).await;
    assert_eq!((passed.rounds, passed.skipped), (1, 0), "{passed:?}");
    assert_eq!(passed.reads, [], "{passed:?}");
}

/// Asserts that a job submitted, or a queued job cancelled, brings back the next heartbeat's
/// pass: a submit of a job at a higher priority reads the queue again and walks it, the new
/// job first; a cancel of it reads the queue again; then the next beat is quiet again.
#[tokio::test]
async fn a_submit_or_a_cancel_brings_the_next_heartbeats_pass_back() {
    let settled = Settled::new("cost_submit").await;
    let urgent = queue(&settled.rig, "urgent", 5);
    let (_, submitted) = settled.beat(&settled.a).await;
    assert_eq!(submitted.skipped, 0, "{submitted:?}");
    assert!(submitted.table_scans().contains(&"idx/job/queued/"));
    assert!(submitted.read_job("urgent"), "{submitted:?}");
    // The new job is first and waits for both hosts: it alone runs a round.
    assert_eq!(submitted.rounds, 1, "{submitted:?}");
    settled.rig.jobs.request_cancel(&urgent).unwrap();
    let (_, cancelled) = settled.beat(&settled.a).await;
    assert_eq!(
        (cancelled.rounds, cancelled.skipped),
        (1, 0),
        "{cancelled:?}"
    );
    assert!(!cancelled.read_job("urgent"), "{cancelled:?}");
    let (_, quiet) = settled.counted(settled.quiet_beats()).await;
    assert_eq!((quiet.rounds, quiet.skipped), (0, 2), "{quiet:?}");
}

/// Asserts that a host that says hello, and a host the registry holds lost, each bring back
/// the next heartbeat's pass, though no job table changed: the new host takes the first queued
/// job on the next beat of another host, and a beat of `host-a` after `host-b` went silent past
/// its grace runs a pass over the hosts without it.
#[tokio::test]
async fn a_host_that_says_hello_or_goes_lost_brings_the_next_heartbeats_pass_back() {
    let mut settled = Settled::new("cost_hosts").await;
    settled.rig.register("host-c", true);
    let (_, joined) = settled.beat(&settled.a).await;
    assert_eq!(joined.skipped, 0, "{joined:?}");
    let placed = settled
        .rig
        .allocation(&JobId::parse("c-third").unwrap())
        .host;
    assert_eq!(placed, HostId::new("host-c").unwrap());
    settled.rig.advance(Duration::from_secs(60));
    let (_, lost) = settled.beat(&settled.a).await;
    assert_eq!(lost.skipped, 0, "{lost:?}");
}

/// Asserts that a pass whose placement did not commit leaves the next heartbeat's pass to run,
/// with no other change between, and that it places the job then; and that what pass 2 told
/// of a queued job brings the next pass back, which writes it on the job's queue status.
#[tokio::test]
async fn a_failed_placement_or_what_pass_2_told_brings_the_next_heartbeats_pass_back() {
    let mut settled = Settled::new("cost_failed").await;
    let ending = settled.running_on("host-a");
    settled.a.end(&ending, Outcome::Succeeded, Some(0));
    let reply = settled.rig.beat(&settled.a).await.unwrap();
    settled.a.take(&reply);
    // The beat's own write (the free) commits; the placement after it fails.
    settled.rig.store.fail_after(1);
    settled.rig.beat(&settled.a).await.unwrap();
    assert_eq!(settled.state("c-third"), JobState::Queued);
    let (_, again) = settled.beat(&settled.b).await;
    assert_eq!(again.skipped, 0, "{again:?}");
    assert_eq!(settled.state("c-third"), JobState::Placed);
    let (_, quiet) = settled.counted(settled.quiet_beats()).await;
    assert_eq!(
        quiet.skipped, 1,
        "the first beat confirms the placement: {quiet:?}"
    );
    let fourth = JobId::parse("d-fourth").unwrap();
    let before = settled.rig.job(&fourth).queue.events().len();
    let launched = Told::Launched {
        machine: "m-1".to_owned(),
    };
    settled.placer().note_outcomes([(fourth.clone(), launched)]);
    let (_, told) = settled.beat(&settled.a).await;
    assert_eq!(told.skipped, 0, "{told:?}");
    let after = settled.rig.job(&fourth).queue.events().len();
    assert_eq!(after, before + 1, "the launch was not written");
}
