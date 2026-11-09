//! What the process does for the fake agent: it is told each command and acknowledgement
//! of a heartbeat reply one by one, as [`AgentHarness`] calls, and these build the runs it
//! holds and the results it has not had acknowledged.

use std::collections::BTreeMap;

use js_agent_wire::{Command, HeldRun, HeldState, JobSpec, Outcome, ResultAck, RunResult};
use js_conformance::agent::{AgentHarness, FakeAgent};
use js_types::Resources;
use js_types::sched::FarmTime;

use super::{Book, RunInfo, sent_for, verdict};
use crate::agent::Io;
use crate::counters::Counter;
use crate::observe::RunId;

/// The runs the fake holds: those it runs, and those whose result is not yet
/// acknowledged.
pub(super) fn held(agent: &FakeAgent) -> Vec<HeldRun> {
    let observed = agent.observed();
    let unacked = agent.unacked();
    let mut held: BTreeMap<&str, HeldState> = unacked
        .iter()
        .map(|run| (run.as_str(), HeldState::ExitedUnacked))
        .collect();
    held.extend(
        observed
            .running
            .iter()
            .map(|run| (run.as_str(), HeldState::Running)),
    );
    let held = held.into_iter().map(|(run, state)| HeldRun {
        run_id: run.to_owned(),
        state,
    });
    held.collect()
}

/// The results the fake has not had acknowledged, of the runs in `runs`.
pub(super) fn results(agent: &FakeAgent, runs: &BTreeMap<String, RunInfo>) -> Vec<RunResult> {
    let unacked = agent.unacked();
    let results = runs
        .iter()
        .filter(|(run, _)| unacked.contains(*run))
        .map(|(run, info)| RunResult {
            run_id: run.clone(),
            lease_id: info.lease_text.clone(),
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            detail: None,
            log_tail: None,
        });
    results.collect()
}

/// Tells the fake one command of a reply; `beats_sent` dates the heartbeat a start names.
pub(super) fn command(
    agent: &mut FakeAgent,
    book: &mut Book,
    beats_sent: &BTreeMap<u64, FarmTime>,
    command: &Command,
    capacity: &Resources,
    io: &mut Io,
) {
    match command {
        Command::Start(job) => start(agent, book, beats_sent, job, capacity, io),
        Command::Stop { run_id } | Command::Cancel { run_id } => {
            book.stopped.insert(run_id.clone());
            agent.cancel(run_id);
        }
        _ => {}
    }
}

fn start(
    agent: &mut FakeAgent,
    book: &mut Book,
    beats_sent: &BTreeMap<u64, FarmTime>,
    job: &JobSpec,
    capacity: &Resources,
    io: &mut Io,
) {
    let Some(info) = RunInfo::of(job, capacity) else {
        return io.bump(Counter::ReplyInvalid);
    };
    let run = job.run_id.as_str();
    let before = agent.observed();
    if book.stopped.contains(run) {
        io.bump(Counter::StartAfterStop);
    } else if before.launches.contains_key(run) {
        io.bump(Counter::StartRepeated);
    }
    let since = sent_for(beats_sent, job, io.now);
    let waited = io.now.as_millis().saturating_sub(since.as_millis());
    agent.start(run, job.valid_for_ms, waited);
    let after = agent.observed();
    if after.never_started.len() > before.never_started.len() {
        io.bump(Counter::StartDroppedLate);
    }
    if after.launches.get(run) != before.launches.get(run) {
        book.launched(job, info, io);
    }
}

pub(super) fn ack(agent: &mut FakeAgent, book: &mut Book, ack: &ResultAck) {
    let Some(verdict) = verdict(ack.ack) else {
        return;
    };
    if agent.unacked().contains(&ack.run_id) {
        let run = RunId::new(ack.run_id.clone());
        book.acknowledged.insert(run, verdict);
    }
    agent.ack(&ack.run_id);
}

/// The run's own time ran out on the fake: it completes, and `true`, unless it no longer
/// runs.
pub(super) fn run_ends(agent: &mut FakeAgent, run: &str) -> bool {
    let running = agent.observed().running.contains(run);
    if running {
        agent.complete(run);
    }
    running
}
