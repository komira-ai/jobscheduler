//! The small pure helpers of the worker's loop: the reply that stops everything, a reply
//! with its starts refused, and the text of events.

use js_agent_wire::{Command, HeartbeatReply, Taint};

use crate::health::one_line;
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
/// said), and no event holds a token, a session secret or a job's labels. The line of a new
/// taint says what it is in plain words (see [`tainted`]); every other event is its debug text.
pub(super) fn describe(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Tainted { taint, detail } => tainted(taint, detail.as_deref()),
            other => format!("job: {other:?}"),
        })
        .collect()
}

/// The line of a new taint, one line that says what changed:
/// `tainted: <reason> (check <code>, run <id>): <detail>`, with the parts the taint or the
/// finding does not have left out. The detail's line breaks and other control characters are
/// spaces ([`one_line`]), so the taint is one line in the log whatever a process or a mount
/// was called.
fn tainted(taint: &Taint, detail: Option<&str>) -> String {
    let parts: Vec<String> = [
        taint.check.as_ref().map(|check| format!("check {check}")),
        taint.run_id.as_ref().map(|run| format!("run {run}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut line = format!("tainted: {}", taint.reason);
    if !parts.is_empty() {
        line.push_str(&format!(" ({})", parts.join(", ")));
    }
    if let Some(detail) = detail {
        line.push_str(": ");
        line.push_str(&one_line(detail));
    }
    line
}

#[cfg(test)]
#[path = "replies_tests.rs"]
mod tests;
