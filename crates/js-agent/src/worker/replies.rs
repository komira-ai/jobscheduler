//! The small pure helpers of the worker's loop: the reply that stops everything, a reply
//! with its starts refused, and the text of events.

use js_agent_wire::{Command, HeartbeatReply, JobUsage, Taint};

use crate::health::one_line;
use crate::lifecycle::{Event, JobBooking, Lifecycle};

/// A reply that stops every job `life` holds.
pub(super) fn stops(life: &Lifecycle) -> HeartbeatReply {
    let commands = life
        .held_jobs()
        .into_iter()
        .map(|job| Command::JobStop { job_id: job.job_id })
        .collect();
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// `reply` with each `JobStart` made an unknown command (so the places of the
/// others, which a refusal is logged by, stay), and a line for each.
pub(super) fn without_starts(mut reply: HeartbeatReply) -> (HeartbeatReply, Vec<String>) {
    let mut lines = Vec::new();
    reply.commands = reply
        .commands
        .into_iter()
        .map(|command| match command {
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
/// taint says what it is in plain words (see [`tainted`]), and the line of a result is one of
/// `key=value` words (see [`result_line`]); every other event is its debug text.
pub(super) fn describe(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Tainted { taint, detail } => tainted(taint, detail.as_deref()),
            Event::ResultReady {
                run_id,
                booked,
                usage,
            } => result_line(run_id, booked, usage.as_ref()),
            other => format!("job: {other:?}"),
        })
        .collect()
}

/// The line of a job's result: what it booked, where the booking came from, what its control
/// group used and how often its CPU was throttled, as `key=value` words in a fixed order, `-`
/// for what is not known:
///
/// `job: result run=<id> cpu_millis=<booked> memory_bytes=<booked> class=<size class>
/// cpu_from=<source> memory_from=<source> cpu_usage_usec=<used> peak_memory_bytes=<peak>
/// cpu_periods=<nr_periods> cpu_throttled_periods=<nr_throttled> throttled_permille=<share>
/// cpu_pressure_some_usec=<the group's cpu.pressure some total>
/// cpu_peak_millicores=<the most CPU over five seconds> peak_swap_bytes=<the most swap held>`
///
/// `peak_memory_bytes` above `memory_bytes` is a job that used more than it booked: the booking
/// is no limit.
///
/// The sources are [`BookedFrom::as_str`](js_agent_wire::BookedFrom::as_str)'s names. The run
/// id is an id of the wire's grammar and the class printable ASCII
/// ([`StartSizing::checked`](js_agent_wire::StartSizing::checked)), so the line is one line;
/// the class is one word as it is ([`class_word`]), or a quoted string when it holds a space.
fn result_line(run_id: &str, booked: &JobBooking, usage: Option<&JobUsage>) -> String {
    let known = |value: Option<String>| value.unwrap_or_else(|| "-".to_owned());
    let sizing = booked.sizing.as_ref();
    let throttle = usage.and_then(|usage| usage.cpu_throttle);
    let words = [
        ("run", run_id.to_owned()),
        ("cpu_millis", booked.cpu_millis.to_string()),
        ("memory_bytes", booked.memory_bytes.to_string()),
        (
            "class",
            known(sizing.and_then(|sizing| sizing.class.as_deref().map(class_word))),
        ),
        ("cpu_from", known(sizing.map(|s| s.cpu.as_str().to_owned()))),
        (
            "memory_from",
            known(sizing.map(|s| s.memory.as_str().to_owned())),
        ),
        (
            "cpu_usage_usec",
            known(usage.map(|u| u.cpu_usage_usec.to_string())),
        ),
        (
            "peak_memory_bytes",
            known(usage.map(|u| u.peak_memory_bytes.to_string())),
        ),
        (
            "cpu_periods",
            known(throttle.map(|t| t.periods.to_string())),
        ),
        (
            "cpu_throttled_periods",
            known(throttle.map(|t| t.throttled_periods.to_string())),
        ),
        (
            "throttled_permille",
            known(throttle.map(|t| t.throttled_permille().to_string())),
        ),
        (
            "cpu_pressure_some_usec",
            known(
                usage
                    .and_then(|u| u.cpu_pressure_some_usec)
                    .map(|p| p.to_string()),
            ),
        ),
        (
            "cpu_peak_millicores",
            known(
                usage
                    .and_then(|u| u.cpu_peak)
                    .map(|p| p.millicores.to_string()),
            ),
        ),
        (
            "peak_swap_bytes",
            known(usage.and_then(|u| u.peak_swap_bytes).map(|p| p.to_string())),
        ),
    ];
    let words: Vec<String> = words
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    format!("job: result {}", words.join(" "))
}

/// The class `class` as one word of a result line: as it is when it is one word (no space,
/// `"` or `\`, and not `-`, which names no class), else in double quotes with each `"` and
/// `\` escaped by a `\`, as a JSON string is, so `class=` still names the whole of it.
fn class_word(class: &str) -> String {
    let word = !class.contains([' ', '"', '\\']) & (class != "-");
    if word {
        return class.to_owned();
    }
    let escaped = class.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
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
