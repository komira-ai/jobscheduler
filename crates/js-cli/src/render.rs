//! What `js` prints: one line of text, or one JSON object, per fact.

use prost_reflect::DynamicMessage;
use serde_json::{Value, json};

use crate::failure::Failure;

/// The output style, chosen by `--json`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Render {
    json: bool,
}

impl Render {
    pub(crate) fn new(json: bool) -> Self {
        Self { json }
    }

    /// An answer of a job command: its proto3 JSON on one line, or its text lines.
    pub(crate) fn answer(&self, message: &DynamicMessage) -> String {
        match self.json {
            true => format!("{}\n", serde_json::to_string(message).unwrap_or_default()),
            false => crate::jobs::text_lines(message),
        }
    }

    /// The id `js` drew for a job command's retry key, printed on stderr before the request
    /// is sent: `js: job id <id>`, or `{"id": <id>}`.
    pub(crate) fn drawn(&self, group: &str, id: &str) -> String {
        match self.json {
            true => line(&json!({ "id": id })),
            false => format!("js: {group} id {id}\n"),
        }
    }

    /// How job `id` ended, as `job wait` and `job run` print it: `job <id> <outcome>`, or
    /// `{"id": <id>, "outcome": <outcome>}`.
    pub(crate) fn ended(&self, id: &str, outcome: &str) -> String {
        match self.json {
            true => line(&json!({ "id": id, "outcome": outcome })),
            false => format!("job {id} {outcome}\n"),
        }
    }

    /// Bytes of a job's stream that are not in the log store, for stderr: `js: stdout bytes
    /// 0 to 4 are not in the log store`, or `{"gap": {"stream": .., "offset": .., "length":
    /// ..}}`.
    pub(crate) fn gap(&self, stream: &str, offset: u64, length: u64) -> String {
        match self.json {
            true => line(&json!({
                "gap": { "stream": stream, "offset": offset, "length": length },
            })),
            false => format!(
                "js: {stream} bytes {offset} to {} are not in the log store\n",
                offset.saturating_add(length)
            ),
        }
    }

    /// The line `job run` prints on stderr at the first signal: `js: SIGINT: cancelling job
    /// <id>; ...`, or `{"signal": "SIGINT", "cancelling": <id>}`.
    pub(crate) fn cancelling(&self, signal: &str, job: &str) -> String {
        match self.json {
            true => line(&json!({ "signal": signal, "cancelling": job })),
            false => format!(
                "js: {signal}: cancelling job {job} and waiting for it to end; another signal stops the wait\n"
            ),
        }
    }

    /// A failure, for stderr.
    pub(crate) fn failure(&self, failure: &Failure) -> String {
        match self.json {
            true => line(&json!({
                "error": failure.parts().1,
                "message": failure.to_string(),
            })),
            false => format!("js: {failure}\n"),
        }
    }
}

fn line(value: &Value) -> String {
    format!("{value}\n")
}
