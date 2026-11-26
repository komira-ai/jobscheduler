//! Step 5 of `SubmitJob`: the call commits the job as queued, pokes the placement poller and
//! answers `queued`, with no pass of its own; the poller's pass ([`crate::poller::pass`]) places
//! the job, and its start rides on the host's held heartbeat. The tests drive the poller's pass
//! themselves and wait on the server's own progress (each wait of a held reply is counted),
//! never on time.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Read;
use js_jobs::{JobState, JobStore, NewJob, RandomIds};
use js_proto::v1::{self, SubmitJobRequest};
use tonic::Request;

use super::{Ctx, submit_job};
use crate::admit::{Admit, JobAdmission};
use crate::agent_api::job_rig::{FakeAgent, Rig, clocked_rig, spec, starts};
use crate::config::Role;
use crate::job_api::JobApi;
use crate::job_api::roles::Caller;
use crate::job_api::writes_rig::{CEILING, NEVER, id, wire};
use crate::poller;

/// A bound no test reaches: a hold a test does not end returns at it.
const LONG: Duration = Duration::from_secs(60);

/// The job service over `rig`'s tables, admitting over its registry's hosts, its submit poking
/// `rig`'s placement poller when `poking`.
fn api(rig: &Rig, poking: bool) -> JobApi {
    let admission = JobAdmission::new(
        Admit::default(),
        rig.agents.registry.clone(),
        Arc::clone(&rig.jobs),
    );
    let ctx = Ctx::served(CEILING, Arc::new(admission));
    let ctx = match poking {
        true => ctx.poking(Arc::clone(&rig.agents)),
        false => ctx,
    };
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone()).with_submit(ctx)
}

/// The answer to submitting `spec` to `api` as an operator.
async fn submit(api: &JobApi, spec: v1::JobSpec) -> v1::Job {
    let caller = Caller {
        name: js_jobs::ClientName::default_client(),
        role: Role::Operator,
    };
    let request = Request::new(SubmitJobRequest { spec: Some(spec) });
    let answer = submit_job(api, &caller, request).await.unwrap();
    answer.into_inner().job.unwrap()
}

/// A placing server with `host-a` registered, whose agent reports `per-job` alone.
fn placing_rig(name: &str) -> Rig {
    let mut rig = clocked_rig(name, true, NEVER);
    rig.register("host-a", true);
    rig
}

/// The state and host of an answered job.
fn answered(job: &v1::Job) -> (v1::JobState, &str) {
    (job.state(), job.host_id.as_str())
}

/// Asserts the ruling's contract end to end: with an idle host whose heartbeat is held, a
/// submit into an empty queue is answered `queued` with no host, the job is still queued in the
/// store when the answer comes, and the poller's pass the submit poked places it and the held
/// reply carries its start, with no tick and no second heartbeat. It catches a submit that
/// answers `placed` (a pass in the call), and a poke that does not reach the poller.
#[tokio::test]
async fn a_submit_is_answered_queued_and_the_poked_pass_starts_it_on_the_held_heartbeat() {
    let mut rig = clocked_rig("submit_held", true, NEVER).holding_for(LONG);
    rig.register_held("host-a");
    let agent = FakeAgent::new("host-a");
    let api = api(&rig, true);
    let submitted = async {
        let mut waits = rig.agents.wakes.waits();
        waits.wait_for(|waits| *waits >= 1).await.unwrap();
        let job = submit(&api, wire("job-s")).await;
        let stored = rig.job(&id("job-s")).state;
        poller::pass(&rig.agents).await;
        (job, stored)
    };
    let (reply, (job, stored)) = tokio::join!(rig.beat(&agent), submitted);
    assert_eq!(answered(&job), (v1::JobState::Queued, ""));
    assert_eq!(stored, JobState::Queued);
    let reply = reply.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-s"]);
    assert_eq!(rig.job(&id("job-s")).state, JobState::Placed);
}

/// Asserts that a submit into an empty queue, with a host that has room, runs no pass of its
/// own: the answer is `queued`, the store holds the job queued with no allocation, and a poke
/// is waiting for the poller; the poller's pass then places the job on the host. It catches a
/// submit that places in the call (the previous step 5 placed a job with nothing ahead), and
/// one that commits without poking.
#[tokio::test]
async fn a_submit_with_nothing_ahead_places_nothing_itself_and_pokes_the_poller() {
    let rig = placing_rig("submit_pokes");
    let api = api(&rig, true);
    let job = submit(&api, wire("job-p")).await;
    assert_eq!(answered(&job), (v1::JobState::Queued, ""));
    assert_eq!(rig.job(&id("job-p")).state, JobState::Queued);
    assert_eq!(rig.jobs.live_allocations().unwrap(), Vec::new());
    // Taking the poke to check it came, and giving it back for the poller's pass.
    assert!(rig.agents.poller.take_poke());
    rig.agents.poller.poke();
    poller::pass(&rig.agents).await;
    let placed = rig.job(&id("job-p"));
    assert_eq!(placed.state, JobState::Placed);
    assert_eq!(
        placed.host.map(|host| host.to_string()),
        Some("host-a".into())
    );
}

/// Asserts that two submits before the poller runs leave one poke, whose one pass places both
/// jobs: the pokes are kept as one, and the pass reads every commit made before it. It catches
/// a poke lost when another came first, and a pass that reads only the last submit's job.
#[tokio::test]
async fn two_submits_before_the_poller_runs_are_both_placed_by_its_one_pass() {
    let rig = placing_rig("submit_two_pokes");
    let api = api(&rig, true);
    for name in ["job-1", "job-2"] {
        let job = submit(&api, wire(name)).await;
        assert_eq!(answered(&job), (v1::JobState::Queued, ""));
    }
    poller::pass(&rig.agents).await;
    assert!(!rig.agents.poller.take_poke());
    let states = ["job-1", "job-2"].map(|name| rig.job(&id(name)).state);
    assert_eq!(states, [JobState::Placed, JobState::Placed]);
}

/// Asserts that a retry of a job already placed is answered with the job as it stands
/// (`placed` on its host), not `queued`: the answer of step 5 is for a new job, and a retry
/// keeps step 2's answer. It catches a retry answered from a fresh queued record.
#[tokio::test]
async fn a_retry_of_a_placed_job_is_answered_as_the_job_stands() {
    let rig = placing_rig("submit_retry_placed");
    let api = api(&rig, true);
    submit(&api, wire("job-r")).await;
    poller::pass(&rig.agents).await;
    let retried = submit(&api, wire("job-r")).await;
    assert_eq!(answered(&retried), (v1::JobState::Placed, "host-a"));
}

/// Asserts that a service that does not place answers a new job `queued`, leaves it queued and
/// pokes nothing: the tick's pass finds the row. It catches a service built without the
/// poller that still pokes or places.
#[tokio::test]
async fn a_service_that_does_not_place_commits_the_job_and_pokes_nothing() {
    let rig = placing_rig("submit_not_placing");
    let job = submit(&api(&rig, false), wire("job-n")).await;
    assert_eq!(answered(&job), (v1::JobState::Queued, ""));
    assert_eq!(rig.job(&id("job-n")).state, JobState::Queued);
    assert!(!rig.agents.poller.take_poke());
    assert_eq!(rig.jobs.live_allocations().unwrap(), Vec::new());
}

/// The prefix of the one scan of each job table a pass takes: the queued jobs, the live
/// allocations, the marks and the cordons.
const TABLE_SCANS: [&str; 4] = ["idx/job/queued/", "idx/alloc/host/", "mark/", "cordon/"];

/// The scans of the job tables `rig`'s store was asked for since the last call, in order.
fn table_scans(rig: &Rig) -> Vec<String> {
    let reads = rig.store.take_reads().into_iter();
    let scans = reads.filter_map(|read| match read {
        Read::Scan(prefix) => Some(prefix),
        Read::Get(_) => None,
    });
    scans
        .filter(|prefix| TABLE_SCANS.contains(&prefix.as_str()))
        .collect()
}

/// Asserts that a submit after a pass that read the tables and wrote nothing scans no job
/// table in the call (the call reads no queue: it no longer asks whether anything is ahead),
/// and that the poked pass takes the tables it read before with the job added, reading none of
/// the four, and places the job. It catches a submit that reads the queue before its commit,
/// and a pass that reads the tables again for the call's own insert.
#[tokio::test]
async fn a_submit_after_a_quiet_pass_reads_no_table_and_its_pass_reads_none_again() {
    let rig = placing_rig("submit_no_reread");
    let api = api(&rig, true);
    rig.agents.place_jobs();
    let _ = table_scans(&rig);
    submit(&api, wire("job-q")).await;
    assert_eq!(table_scans(&rig), Vec::<String>::new());
    poller::pass(&rig.agents).await;
    assert_eq!(table_scans(&rig), Vec::<String>::new());
    assert_eq!(rig.job(&id("job-q")).state, JobState::Placed);
}

/// Asserts that a commit of another server over the same store, between the pass's read and
/// the submit, makes the poked pass read the four tables again and place the job: the
/// submit's commit is not the only write since. It catches tables kept past another server's
/// write, which leave its job out of the pass.
#[tokio::test]
async fn a_submit_after_another_servers_write_has_its_pass_read_the_tables_again() {
    let rig = placing_rig("submit_other_write");
    let api = api(&rig, true);
    rig.agents.place_jobs();
    let other = JobStore::new(Arc::clone(&rig.store), RandomIds).unwrap();
    let elsewhere = NewJob {
        id: Some(id("job-o")),
        spec: spec(),
        priority: -1,
        client: None,
    };
    other.insert_queued(elsewhere).unwrap();
    let _ = table_scans(&rig);
    submit(&api, wire("job-s")).await;
    poller::pass(&rig.agents).await;
    let read_again = ["idx/alloc/host/", "idx/job/queued/", "mark/", "cordon/"];
    assert_eq!(table_scans(&rig), read_again);
    assert_eq!(rig.job(&id("job-s")).state, JobState::Placed);
}

/// Asserts that a submit made while a pass reads the job tables again (the pass held on a gate
/// the store holds its scan of the queued jobs at) is answered before the gate opens: the
/// call records the job and answers `queued` without waiting for the pass, which holds the
/// tables the call would add the job to. The held pass then reads the queue, with the job, and
/// places it on the host. Before, the call waited for the whole read to tell the placer of its job, so it
/// answered only once the gate opened (here, the test's bound). It catches a submit whose
/// answer waits on a pass, and a job that is not placed once the call skipped telling the
/// placer.
#[tokio::test]
async fn a_submit_during_a_pass_s_read_of_the_tables_answers_before_the_read_ends() {
    let rig = placing_rig("submit_during_read");
    let api = api(&rig, true);
    rig.agents.place_jobs();
    // A write since that read, so the next pass reads the tables again: a job that never
    // fits, behind any other.
    let other = JobStore::new(Arc::clone(&rig.store), RandomIds).unwrap();
    let never = NewJob {
        id: Some(id("job-never")),
        spec: js_jobs::JobSpec {
            needs: js_jobs::Needs::Shape(
                js_types::Resources::new(64_000, 1 << 30, 0, None).unwrap(),
            ),
            ..spec()
        },
        priority: -1,
        client: None,
    };
    other.insert_queued(never).unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (open, opened) = std::sync::mpsc::channel::<()>();
    rig.store.before_next_scan("idx/job/queued/", move || {
        entered_tx.send(()).unwrap();
        // Until the test opens the gate, or drops it.
        let _ = opened.recv();
    });
    let agents = Arc::clone(&rig.agents);
    let pass = std::thread::spawn(move || agents.place_jobs());
    entered.recv().unwrap();
    let waited = tokio::time::timeout(LONG, submit(&api, wire("job-g"))).await;
    drop(open);
    pass.join().unwrap();
    let job = waited.expect("the submit waited for the pass's read");
    assert_eq!(answered(&job), (v1::JobState::Queued, ""));
    assert_eq!(rig.job(&id("job-g")).state, JobState::Placed);
}
