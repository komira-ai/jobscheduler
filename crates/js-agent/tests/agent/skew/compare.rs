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

/// The first step after `index` of `flow` (a transcript's steps, or the steps of one
/// conversation) whose reply starts a job, and that start.
pub fn start_after(flow: &[Value], index: usize) -> Option<(usize, Value)> {
    let later = flow.iter().enumerate().skip(index + 1);
    let mut commands = later.filter_map(|(at, step)| {
        let first = step["server"]["body"]["commands"].as_array()?.first()?;
        Some((at, first.clone()))
    });
    commands.find(|(_, command)| command["type"] == "job_start")
}

/// A job start this release's server may send one heartbeat before the previous server did
/// (its placement poller, which the submit pokes, places the job at once, where the previous
/// server placed it at the next heartbeat): when the previous release's reply `then` held no command and the reply `now`
/// holds exactly one, a `job_start`, takes that command out of `now` and returns it. Any
/// other extra command, a second one, or an extra one where the previous reply held some, is
/// left in `now` for [`uncovered`] to report.
pub fn take_early_start(then: &Value, now: &mut Value) -> Option<Value> {
    let none = then["commands"].as_array().is_some_and(Vec::is_empty);
    let commands = now.get_mut("commands")?.as_array_mut()?;
    match commands.as_slice() {
        [only] if none && only["type"] == "job_start" => Some(commands.remove(0)),
        _ => None,
    }
}

/// The command a heartbeat reply would have carried for a message the dispatch poll handed
/// out: a start is its `start` body (the job start a heartbeat's reply carries) with the
/// command's `type`, a stop is a `job_stop` of the same job. `None` for any other message
/// (an unknown kind, a start without a body).
pub fn as_command(message: &Value) -> Option<Value> {
    match (message["kind"].as_str()?, message.get("start")) {
        ("start", Some(Value::Object(start))) => {
            let mut command = start.clone();
            command.insert("type".to_owned(), Value::from("job_start"));
            Some(Value::Object(command))
        }
        ("stop", None) => Some(serde_json::json!({
            "type": "job_stop",
            "job_id": message["job_id"],
        })),
        _ => None,
    }
}

/// For an agent welcomed with `dispatch_poll`, the server hands a job start or stop out by
/// the dispatch poll and leaves it out of the heartbeat reply, where the previous release
/// sent it. When the previous reply `then` held a command and the reply `now` holds none,
/// takes the next of `handed` (the messages of the poll, in the order handed) and puts it in
/// `now` as the command it stands for ([`as_command`]), so that [`uncovered`] compares it
/// with the previous release's command field by field, and returns the message. A reply that
/// holds a command, a previous reply without one, a poll with nothing left to hand out, and
/// a message that stands for no command, are left as they are: the comparison reports the
/// missing or different command, or [`unhanded`] the message.
pub fn hand_out(
    then: &Value,
    now: &mut Value,
    handed: &mut std::collections::VecDeque<Value>,
) -> Option<Value> {
    let wanted = then["commands"].as_array().is_some_and(|c| !c.is_empty());
    let commands = now.get_mut("commands")?.as_array_mut()?;
    if !wanted || !commands.is_empty() {
        return None;
    }
    let command = as_command(handed.front()?)?;
    commands.push(command);
    handed.pop_front()
}

/// The messages of `handed` that no reply took ([`hand_out`]) and that are no repeat of one
/// that was taken (`taken`), as problems. The poll may hand out a message again (the server
/// hands a stop out again for a job it ended on the host's report of the kill); a repeat is
/// the same message on a route the previous release never had, and says nothing more.
pub fn unhanded(handed: &std::collections::VecDeque<Value>, taken: &[Value]) -> Vec<String> {
    let new = handed.iter().filter(|message| !taken.contains(message));
    new.map(|message| {
        format!("the poll handed out {message}, which no reply of the previous release carried")
    })
    .collect()
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

/// Asserts that [`take_early_start`] takes a lone `job_start` out of a reply the previous
/// release left without commands, and nothing else: not a stop, not two commands, not a start
/// where the previous reply held a command, and not a reply without a command list; and that
/// [`start_after`] finds the next reply that starts a job, skipping the ones before it. It
/// catches an acceptance of an early start that would also pass any other extra command.
#[test]
fn only_a_lone_start_the_previous_reply_lacked_is_taken_early() {
    let start = serde_json::json!({ "type": "job_start", "job_id": "j1" });
    let stop = serde_json::json!({ "type": "job_stop", "job_id": "j1" });
    let none = serde_json::json!({ "commands": [] });
    let mut now = serde_json::json!({ "commands": [start], "result_acks": [] });
    assert_eq!(take_early_start(&none, &mut now), Some(start.clone()));
    assert_eq!(
        now,
        serde_json::json!({ "commands": [], "result_acks": [] })
    );
    for commands in [
        vec![stop.clone()],
        vec![start.clone(), start.clone()],
        vec![],
    ] {
        let mut now = serde_json::json!({ "commands": commands });
        let before = now.clone();
        assert_eq!(take_early_start(&none, &mut now), None);
        assert_eq!(now, before);
    }
    let mut now = serde_json::json!({ "commands": [start] });
    let had = serde_json::json!({ "commands": [stop] });
    assert_eq!(take_early_start(&had, &mut now), None);
    let mut bare = serde_json::json!({});
    assert_eq!(take_early_start(&none, &mut bare), None);

    let step =
        |commands: Value| serde_json::json!({ "server": { "body": { "commands": commands } } });
    let flow = [
        step(serde_json::json!([start])),
        step(serde_json::json!([])),
        step(serde_json::json!([stop])),
        step(serde_json::json!([start])),
    ];
    assert_eq!(start_after(&flow, 0), Some((3, start.clone())));
    assert_eq!(start_after(&flow, 3), None);
    assert_eq!(start_after(&flow[..3], 0), None);
}

/// Asserts that a start or stop the poll handed out stands in for the command the previous
/// release's reply carried only when it is that command: a start with its body (compared field
/// by field afterwards), a stop of the same job; that a reply which still holds a command, a
/// previous reply without one, an empty poll and a message that stands for no command are left
/// alone; and that a message left over is reported unless it repeats one a reply took. It
/// catches a comparison that would pass a reply whose command went missing, or one that would
/// accept any message the poll hands out.
#[test]
fn a_start_or_stop_of_the_poll_stands_in_for_the_command_of_a_reply_only_as_that_command() {
    use std::collections::VecDeque;
    let start = serde_json::json!({ "type": "job_start", "job_id": "j1", "image": "i" });
    let stop = serde_json::json!({ "type": "job_stop", "job_id": "j1" });
    let dispatched_start = serde_json::json!({
        "kind": "start", "job_id": "j1", "allocation_id": "a1",
        "start": { "job_id": "j1", "image": "i" },
    });
    let dispatched_stop =
        serde_json::json!({ "kind": "stop", "job_id": "j1", "allocation_id": "a1" });
    assert_eq!(as_command(&dispatched_start), Some(start.clone()));
    assert_eq!(as_command(&dispatched_stop), Some(stop.clone()));
    for odd in [
        serde_json::json!({ "kind": "start", "job_id": "j1" }),
        serde_json::json!({ "kind": "stop", "job_id": "j1", "start": {} }),
        serde_json::json!({ "kind": "unknown", "job_id": "j1" }),
        serde_json::json!({}),
    ] {
        assert_eq!(as_command(&odd), None, "{odd}");
    }

    let wants = |command: &Value| serde_json::json!({ "commands": [command] });
    let none = serde_json::json!({ "commands": [] });
    let mut handed: VecDeque<Value> = [dispatched_start.clone(), dispatched_stop.clone()].into();
    let mut now = none.clone();
    assert_eq!(
        hand_out(&wants(&start), &mut now, &mut handed),
        Some(dispatched_start.clone())
    );
    assert_eq!(now, wants(&start));
    let mut now = none.clone();
    assert_eq!(
        hand_out(&wants(&stop), &mut now, &mut handed),
        Some(dispatched_stop.clone())
    );
    assert_eq!(now, wants(&stop));
    assert!(handed.is_empty());

    let mut left: VecDeque<Value> = [dispatched_stop.clone()].into();
    let mut held = wants(&start);
    assert_eq!(hand_out(&wants(&stop), &mut held, &mut left), None);
    assert_eq!(held, wants(&start));
    let mut empty = none.clone();
    assert_eq!(hand_out(&none, &mut empty, &mut left), None);
    assert_eq!(empty, none);
    let mut bare = serde_json::json!({});
    assert_eq!(hand_out(&wants(&stop), &mut bare, &mut left), None);
    assert_eq!(
        hand_out(&wants(&stop), &mut none.clone(), &mut VecDeque::new()),
        None
    );
    let mut odd: VecDeque<Value> = [serde_json::json!({ "kind": "unknown" })].into();
    assert_eq!(hand_out(&wants(&stop), &mut none.clone(), &mut odd), None);
    assert_eq!(left.len(), 1);

    assert_eq!(
        unhanded(&left, std::slice::from_ref(&dispatched_stop)),
        Vec::<String>::new()
    );
    assert_eq!(unhanded(&left, &[dispatched_start]).len(), 1);
    assert_eq!(unhanded(&VecDeque::new(), &[]), Vec::<String>::new());
}
