//! The small pure helpers of the worker's loop: the reply that stops everything, a reply
//! with its starts refused, and the text of events.

use js_agent_wire::{Command, HeartbeatReply};

use crate::lifecycle::{Event, Lifecycle};

/// A reply that stops every run and every job `life` holds.
pub(super) fn stops(life: &Lifecycle) -> HeartbeatReply {
    let runs = life
        .held()
        .into_iter()
        .map(|run| Command::Stop { run_id: run.run_id });
    let jobs = life
        .held_jobs()
        .into_iter()
        .map(|job| Command::JobStop { job_id: job.job_id });
    let commands = runs.chain(jobs).collect();
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// `reply` with each `Start` and `JobStart` made an unknown command (so the places of the
/// others, which a refusal is logged by, stay), and a line for each.
pub(super) fn without_starts(mut reply: HeartbeatReply) -> (HeartbeatReply, Vec<String>) {
    let mut lines = Vec::new();
    reply.commands = reply
        .commands
        .into_iter()
        .map(|command| match command {
            Command::Start(spec) => {
                lines.push(format!(
                    "draining: the start of {} is not taken",
                    spec.run_id
                ));
                Command::Unknown
            }
            Command::JobStart(spec) => {
                lines.push(format!(
                    "draining: the start of job {} is not taken",
                    spec.job_id
                ));
                Command::Unknown
            }
            other => other,
        })
        .collect();
    (reply, lines)
}

/// One line for each event. The text stays on the host (a finding holds what the engine
/// said), and no event holds a token, a session secret or a job's labels.
pub(super) fn describe(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| format!("job: {event:?}"))
        .collect()
}
