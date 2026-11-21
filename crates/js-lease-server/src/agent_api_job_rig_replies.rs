//! What a heartbeat's reply tells a [`super::FakeAgent`] to start and to stop.

use js_agent_wire::{Command, HeartbeatReply, JobStartSpec};

/// The job starts of `reply`.
pub(crate) fn starts(reply: &HeartbeatReply) -> Vec<JobStartSpec> {
    let each = reply.commands.iter().filter_map(|command| match command {
        Command::JobStart(spec) => Some(spec.as_ref().clone()),
        _ => None,
    });
    each.collect()
}

/// The job ids `reply` says to stop.
pub(crate) fn stops(reply: &HeartbeatReply) -> Vec<String> {
    let each = reply.commands.iter().filter_map(|command| match command {
        Command::JobStop { job_id } => Some(job_id.clone()),
        _ => None,
    });
    each.collect()
}
