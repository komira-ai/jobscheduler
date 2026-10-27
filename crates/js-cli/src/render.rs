//! What `js` prints: one line of text, or one JSON object, per fact.

use serde_json::{Value, json};

use crate::api::{Entry, Lease};
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

    /// A lease that is held.
    pub(crate) fn held(&self, lease: &Lease) -> String {
        match self.json {
            true => line(&json!({
                "id": lease.id,
                "host": lease.host,
                "expires_at_ms": lease.expires_at_ms,
            })),
            false => format!(
                "lease {} host {} expires {}\n",
                lease.id, lease.host, lease.expires_at_ms
            ),
        }
    }

    /// A lease that was renewed.
    pub(crate) fn renewed(&self, id: &str, expires_at_ms: u64) -> String {
        match self.json {
            true => line(&json!({ "id": id, "expires_at_ms": expires_at_ms })),
            false => format!("lease {id} expires {expires_at_ms}\n"),
        }
    }

    /// A lease that was released: `state` is `freed` or `releasing`.
    pub(crate) fn released(&self, id: &str, state: &str) -> String {
        match self.json {
            true => line(&json!({ "id": id, "state": state })),
            false => format!("lease {id} {state}\n"),
        }
    }

    /// The server's list: its own JSON, or a line per lease.
    pub(crate) fn listing(&self, body: &Value, entries: &[Entry]) -> String {
        let lines = entries.iter().map(|entry| {
            let host = entry.host.as_deref().unwrap_or("-");
            format!(
                "{} {} host {host} expires {}\n",
                entry.id, entry.state, entry.expires_at_ms
            )
        });
        match self.json {
            true => line(body),
            false => lines.collect(),
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
