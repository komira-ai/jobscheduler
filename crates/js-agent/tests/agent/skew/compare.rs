//! Whether a frame says everything a frame of the previous release said.
//!
//! A receiver of the previous release reads the fields it knows and skips the rest, so a
//! frame of this release serves it when every field of the previous release's frame is
//! there, under the same name, with the same value: a field may be added, never removed,
//! renamed or given another meaning. [`uncovered`] lists every place where that fails. A list
//! is compared item by item and must hold as many items: one more command, acknowledgement or
//! held job is a change of what the conversation says, not an added field, and fails too.
//! `agent_version` must be there and a string, but its value is the build's and is not
//! compared ([`UNCOMPARED`]).
//!
//! Some values are drawn afresh in every conversation (the session secret, a job's id, an
//! allocation's id): under a key of [`MINTED`] two conversations differ by design. With a
//! [`Minted`] table, the first such value the previous release sent is paired with the one
//! sent now, and every later frame is compared with the pairs applied.

use serde_json::{Map, Value};

/// The keys whose values must be there, as strings, but are the build's own: the version an
/// agent reports in its hello.
pub const UNCOMPARED: [&str; 1] = ["agent_version"];

/// The keys whose values the server draws afresh in every conversation: the session
/// secret, the ids of a job and of its allocation, and the job id of a transcript's
/// `submitted` and `cancelled` events.
pub const MINTED: [&str; 5] = [
    "session",
    "job_id",
    "allocation_id",
    "submitted",
    "cancelled",
];

/// The fresh values of one conversation, paired with those of the recorded one.
#[derive(Debug, Default)]
pub struct Minted {
    pairs: Vec<(Value, Value)>,
}

impl Minted {
    /// Pairs the recorded `then` with `now`.
    pub fn learn(&mut self, then: Value, now: Value) {
        self.pairs.push((then, now));
    }

    /// The value paired with the recorded `then`, or `then` when it has none.
    pub fn now_of(&self, then: &Value) -> Value {
        let paired = self.pairs.iter().find(|(old, _)| old == then);
        paired.map_or_else(|| then.clone(), |(_, new)| new.clone())
    }

    /// `value` with every value under a key of [`MINTED`] replaced by its pair.
    pub fn apply(&self, value: &Value) -> Value {
        match value {
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.apply(item)).collect())
            }
            Value::Object(fields) => {
                let mapped = fields.iter().map(|(key, item)| {
                    let item = match MINTED.contains(&key.as_str()) {
                        true => self.now_of(item),
                        false => self.apply(item),
                    };
                    (key.clone(), item)
                });
                Value::Object(mapped.collect::<Map<String, Value>>())
            }
            other => other.clone(),
        }
    }

    fn is_now(&self, value: &Value) -> bool {
        self.pairs.iter().any(|(_, new)| new == value)
    }
}

/// Every place where `now` fails to say what `then` said, as `path: what`; empty when it
/// says all of it. An object of `now` may hold more keys than the one of `then`; a list
/// must hold as many items, each compared with its peer; anything else must be equal.
/// With `minted`, a value under a key of [`MINTED`] that is not yet paired is paired
/// instead of compared, when the value sent now is a non-empty string no other recorded
/// value is paired with.
pub fn uncovered(then: &Value, now: &Value, minted: Option<&mut Minted>) -> Vec<String> {
    let mut found = Vec::new();
    walk("$", None, then, now, minted, &mut found);
    found
}

fn walk(
    path: &str,
    key: Option<&str>,
    then: &Value,
    now: &Value,
    mut minted: Option<&mut Minted>,
    found: &mut Vec<String>,
) {
    match (then, now) {
        (Value::Object(old), Value::Object(new)) => {
            for (name, item) in old {
                let at = format!("{path}.{name}");
                match new.get(name) {
                    Some(peer) => walk(&at, Some(name), item, peer, minted.as_deref_mut(), found),
                    None => found.push(format!("{at}: missing")),
                }
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (index, (item, peer)) in old.iter().zip(new).enumerate() {
                let at = format!("{path}[{index}]");
                walk(&at, None, item, peer, minted.as_deref_mut(), found);
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            found.push(format!("{path}: {} items, now {}", old.len(), new.len()));
        }
        _ if then == now => {}
        (Value::String(_), Value::String(_))
            if key.is_some_and(|name| UNCOMPARED.contains(&name)) => {}
        _ => match (minted, key) {
            (Some(table), Some(name))
                if MINTED.contains(&name)
                    && then.is_string()
                    && now.as_str().is_some_and(|text| !text.is_empty())
                    && !table.is_now(then)
                    && !table.is_now(now) =>
            {
                table.learn(then.clone(), now.clone());
            }
            _ => found.push(format!("{path}: was {then}, now {now}")),
        },
    }
}

/// Asserts what [`uncovered`] reports of a frame that lost a field, renamed one, changed a
/// value, or holds another number of items, and that a frame with a field added, or with
/// another `agent_version` string, is covered while a missing or non-string version is
/// not. It catches a comparison that would pass a frame a receiver of the previous
/// release could not read.
#[test]
fn a_removed_renamed_or_changed_field_is_reported_and_an_added_one_is_not() {
    let then = serde_json::json!({
        "epoch": 1,
        "jobs": [{ "job_id": "j1", "state": "running" }],
        "phase": "serving",
    });
    let renamed = serde_json::json!({
        "epoch": 1,
        "jobs": [{ "job_id": "j1", "status": "running" }],
        "phase": "serving",
    });
    assert_eq!(
        uncovered(&then, &renamed, None),
        ["$.jobs[0].state: missing"]
    );
    let removed =
        serde_json::json!({ "epoch": 1, "jobs": [{ "job_id": "j1", "state": "running" }] });
    assert_eq!(uncovered(&then, &removed, None), ["$.phase: missing"]);
    let changed = serde_json::json!({
        "epoch": "1",
        "jobs": [{ "job_id": "j1", "state": "running" }, { "job_id": "j2", "state": "running" }],
        "phase": "serving",
    });
    assert_eq!(
        uncovered(&then, &changed, None),
        ["$.epoch: was 1, now \"1\"", "$.jobs: 1 items, now 2"]
    );
    let added = serde_json::json!({
        "epoch": 1,
        "jobs": [{ "job_id": "j1", "state": "running", "since": 3 }],
        "phase": "serving",
        "load": {},
    });
    assert_eq!(uncovered(&then, &added, None), Vec::<String>::new());
    let built = serde_json::json!({ "agent_version": "0.1.0" });
    let rebuilt = serde_json::json!({ "agent_version": "0.2.0" });
    assert_eq!(uncovered(&built, &rebuilt, None), Vec::<String>::new());
    let unversioned = serde_json::json!({ "agent_version": 2 });
    assert_eq!(
        uncovered(&built, &unversioned, None),
        ["$.agent_version: was \"0.1.0\", now 2"]
    );
    assert_eq!(
        uncovered(&built, &serde_json::json!({}), None),
        ["$.agent_version: missing"]
    );
}

/// Asserts that a minted value is paired the first time it is met and compared after, that
/// two recorded values are never paired with one value sent now, nor with an empty one,
/// that
/// [`Minted::apply`] puts the pair in place under a minted key only, and that without a
/// table a minted value is compared like any other. It catches a comparison that would
/// accept any session or job id, and one that rewrites a value that only looks like one.
#[test]
fn a_minted_value_is_paired_once_and_compared_after() {
    let mut minted = Minted::default();
    let then = serde_json::json!({ "session": "s-then", "pool": "s-then" });
    let now = serde_json::json!({ "session": "s-now", "pool": "s-then" });
    assert_eq!(
        uncovered(&then, &now, Some(&mut minted)),
        Vec::<String>::new()
    );
    assert_eq!(minted.apply(&then), now);
    assert_eq!(
        uncovered(
            &minted.apply(&then),
            &serde_json::json!({ "session": "s-other", "pool": "s-then" }),
            Some(&mut minted)
        ),
        ["$.session: was \"s-now\", now \"s-other\""]
    );
    assert_eq!(
        uncovered(&then, &now, None),
        ["$.session: was \"s-then\", now \"s-now\""]
    );
    let twice = serde_json::json!({ "allocation_id": "a-then", "job_id": "j-then" });
    let same = serde_json::json!({ "allocation_id": "s-now", "job_id": "" });
    assert_eq!(
        uncovered(&twice, &same, Some(&mut minted)),
        [
            "$.allocation_id: was \"a-then\", now \"s-now\"",
            "$.job_id: was \"j-then\", now \"\""
        ]
    );
    let number = serde_json::json!({ "job_id": 1 });
    let other = serde_json::json!({ "job_id": 2 });
    assert_eq!(
        uncovered(&number, &other, Some(&mut minted)),
        ["$.job_id: was 1, now 2"]
    );
}
