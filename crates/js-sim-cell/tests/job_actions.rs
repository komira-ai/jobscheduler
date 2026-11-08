//! The machinery the job scenario family (`tests/jobs.rs`) added, each part shown to do
//! what it says on the real server: the job front's lost-answer switch and the action that
//! throws it, and the action that kills agents mid-job and replaces them.
//!
//! Sizes: one to three hosts beating every 5 s for at most a minute, a few dozen agent
//! requests a test.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use js_sim::Faults;
use js_sim_cell::actions::{kill_agents_mid_job, lose_submit_answer};
use js_sim_cell::{
    Action, AgentNode, CallKind, Cell, Front, JobFront, LostAnswer, ServerSpec, Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

/// How long each job here runs: longer than any test runs the cell.
const RUN: Duration = Duration::from_secs(600);

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A server of `hosts` hosts serving the job service.
fn spec(hosts: usize) -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), hosts).jobs()
}

/// A cell of `hosts` correct agents of [`spec`], its client `front`.
fn cell(name: &str, hosts: usize, front: Box<dyn Front>) -> Cell {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    );
    let mut cell =
        Cell::new(Setup::new(5), spec(hosts), dir, Faults::default()).expect("the cell starts");
    cell.set_front(front);
    for _ in 0..hosts {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell
}

fn run_to(cell: &mut Cell, at: u64) {
    cell.run_until(ms(at)).expect("no violation");
}

/// The statuses of every SubmitJob answer the client was given, in order.
fn submits(cell: &Cell) -> Vec<u16> {
    let answers = cell.answers().iter();
    let queue = answers.filter(|answer| answer.kind == CallKind::Queue);
    queue.map(|answer| answer.status).collect()
}

/// Asserts that a front made by `JobFront::losing_answers` puts a thrown switch down;
/// that `lose_submit_answer` throws it; that the front then reads the 201 its next submit
/// was given as no answer and submits again under the same job id, which the server
/// answers 201 too, after which the switch is down; that the server holds one job; and
/// that a front whose switch is never thrown submits once. Catches a switch that leaks
/// from one seed's front into the next, an action that does not throw it, a lost answer
/// read as a refusal (the item would end failed) or never submitted again, and a switch
/// that stays thrown and loses every answer.
#[test]
fn a_lost_submit_answer_is_submitted_again_under_the_same_job_id() {
    let switch = LostAnswer::new();
    switch.throw();
    let front = JobFront::losing_answers(&switch).make(&spec(1));
    assert!(!switch.is_thrown(), "a new front puts the switch down");
    let mut lost = cell("a_lost_submit_answer", 1, front);
    run_to(&mut lost, 1_000);
    lose_submit_answer(&switch).run(&mut lost);
    assert!(switch.is_thrown());
    lost.submit(&Work::run_for(RUN));
    run_to(&mut lost, 30_000);
    assert!(!switch.is_thrown(), "the lost answer put the switch down");
    assert_eq!(submits(&lost), [201, 201]);
    let reads = lost.observations().job_reads.iter();
    let jobs: BTreeSet<String> = reads.map(|read| read.job.to_string()).collect();
    assert_eq!(jobs, BTreeSet::from(["sim-job-0".to_owned()]));

    let kept = LostAnswer::new();
    let front = JobFront::losing_answers(&kept).make(&spec(1));
    let mut once = cell("a_kept_submit_answer", 1, front);
    run_to(&mut once, 1_000);
    once.submit(&Work::run_for(RUN));
    run_to(&mut once, 30_000);
    assert_eq!(submits(&once), [201]);
}

/// Whether agent `index` has a process.
fn alive(cell: &Cell, index: usize) -> bool {
    cell.agent(index).and_then(AgentNode::truth).is_some()
}

/// Asserts that `kill_agents_mid_job` kills nothing in a cell whose agents run no job;
/// that once one of three agents runs a job its heartbeats listed, a kill of up to two
/// kills that agent only; and that it starts a new process on that host the given time
/// later, and not before. Catches an action that kills idle agents (the kill would not be
/// mid-job), one that kills past its count or fails to replace, and one that replaces at
/// once.
#[test]
fn agents_running_listed_jobs_are_killed_and_replaced_later() {
    let mut cell = cell("kill_agents_mid_job", 3, Box::new(JobFront::new()));
    run_to(&mut cell, 1_000);
    let kill = kill_agents_mid_job(2, Duration::from_secs(10));
    kill.run(&mut cell);
    assert!(cell.observations().kills.is_empty(), "no agent runs a job");
    cell.submit(&Work::run_for(RUN));
    run_to(&mut cell, 30_000);
    let busy: Vec<usize> = (0..3)
        .filter(|index| {
            let truth = cell.agent(*index).and_then(AgentNode::truth);
            truth.is_some_and(|truth| !truth.jobs.is_empty())
        })
        .collect();
    assert_eq!(busy.len(), 1, "one agent runs the job");
    kill.run(&mut cell);
    let kills = cell.observations().kills.iter();
    let killed: Vec<String> = kills.map(|kill| kill.host.to_string()).collect();
    assert_eq!(killed, [format!("h{:03}", busy[0])]);
    assert!(!alive(&cell, busy[0]));
    run_to(&mut cell, 39_000);
    assert!(!alive(&cell, busy[0]), "not replaced before its time");
    run_to(&mut cell, 41_000);
    assert!(alive(&cell, busy[0]), "replaced 10 s after the kill");
    assert!((0..3).all(|index| alive(&cell, index)));
}
