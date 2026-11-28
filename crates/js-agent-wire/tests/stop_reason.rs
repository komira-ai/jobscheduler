//! The reason a job stop carries and the jobs a reply names unknown to the server: both
//! additive (B8), so an agent from before them still stops every job it is told to stop and
//! reads the rest of the reply, and an agent of now reads a server from before them.

use js_agent_wire::{Command, HeartbeatReply, StopReason, limits};
use serde::Deserialize;
use serde_json::json;

/// The job stop and the reply as the release before the fields reads them: a copy of their
/// shapes then, every other field left out (a reader ignores what it does not know).
mod previous {
    use serde::{Deserialize, Deserializer};

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Command {
        JobStop {
            job_id: String,
        },
        #[serde(other)]
        Unknown,
    }

    #[derive(Debug, Deserialize)]
    pub struct HeartbeatReply {
        #[serde(default, deserialize_with = "commands_or_unknown")]
        pub commands: Vec<Command>,
        #[serde(default)]
        pub retired_acks: Vec<String>,
    }

    /// The release's decode of the commands, one by one: one that does not decode is
    /// `Unknown`.
    fn commands_or_unknown<'de, D>(deserializer: D) -> Result<Vec<Command>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
        Ok(values
            .into_iter()
            .map(|value| serde_json::from_value(value).unwrap_or(Command::Unknown))
            .collect())
    }
}

fn stop(job_id: &str, reason: Option<StopReason>) -> Command {
    Command::JobStop {
        job_id: job_id.to_owned(),
        reason,
    }
}

/// Asserts the wire form of each reason, and that a stop without one is the stop of the
/// release before the field, byte for byte. It catches a reason renamed on the wire and a
/// stop that writes `"reason": null`.
#[test]
fn each_reason_has_its_wire_name_and_a_stop_without_one_is_unchanged() {
    let named = [
        (StopReason::Cancelled, "cancelled"),
        (StopReason::Ended, "ended"),
        (StopReason::Replaced, "replaced"),
        (StopReason::OrphanExpired, "orphan_expired"),
        (StopReason::Operator, "operator"),
    ];
    for (reason, name) in named {
        assert_eq!(
            serde_json::to_value(stop("j1", Some(reason))).unwrap(),
            json!({ "type": "job_stop", "job_id": "j1", "reason": name })
        );
    }
    assert_eq!(
        serde_json::to_value(stop("j1", None)).unwrap(),
        json!({ "type": "job_stop", "job_id": "j1" })
    );
}

/// Asserts that a stop with a reason this version does not know is still the stop of its
/// job, with the reason read as unknown, and that a stop from a server from before the field
/// has none. It catches an unknown reason that makes the stop undecodable (the agent would
/// read the whole command as unknown and keep running a job its server stopped).
#[test]
fn a_stop_with_a_reason_this_version_does_not_know_still_stops() {
    let newer: Command = serde_json::from_value(json!({
        "type": "job_stop", "job_id": "j1", "reason": "a-reason-from-later"
    }))
    .unwrap();
    assert_eq!(newer, stop("j1", Some(StopReason::Unknown)));
    let older: Command =
        serde_json::from_value(json!({ "type": "job_stop", "job_id": "j1" })).unwrap();
    assert_eq!(older, stop("j1", None));
    assert!(newer.validate().is_ok());
}

/// Asserts that an agent of the release before the fields reads this server's reply as it
/// always did: each stop, whatever its reason, is the stop of its job, and the unknown jobs
/// are ignored beside the rest. It catches a stop the old agent no longer reads (it would
/// keep a cancelled job running) and a new reply field that fails the old decode.
#[test]
fn an_agent_from_before_the_fields_reads_every_stop_and_ignores_the_unknown_jobs() {
    let reply = HeartbeatReply {
        commands: vec![
            stop("j1", Some(StopReason::Cancelled)),
            stop("j2", Some(StopReason::Replaced)),
            stop("j3", None),
        ],
        retired_acks: vec!["j9".to_owned()],
        unknown_jobs: vec!["j4".to_owned(), "j5".to_owned()],
        ..HeartbeatReply::default()
    };
    let body = serde_json::to_vec(&reply).unwrap();
    let old: previous::HeartbeatReply = serde_json::from_slice(&body).unwrap();
    let stopped: Vec<&str> = old
        .commands
        .iter()
        .map(|command| match command {
            previous::Command::JobStop { job_id } => job_id.as_str(),
            previous::Command::Unknown => "unknown",
        })
        .collect();
    assert_eq!(stopped, ["j1", "j2", "j3"]);
    assert_eq!(old.retired_acks, ["j9"]);
}

/// Asserts that a reply from a server from before the field names no unknown job, and that
/// an empty list is left out of the body. It catches a reply of an old server read as naming
/// jobs, and an empty list written on every reply.
#[test]
fn a_reply_without_unknown_jobs_names_none_and_writes_none() {
    let old: HeartbeatReply = serde_json::from_value(json!({ "commands": [] })).unwrap();
    assert!(old.unknown_jobs.is_empty());
    let body = serde_json::to_value(HeartbeatReply::default()).unwrap();
    assert!(body.get("unknown_jobs").is_none(), "{body}");
    #[derive(Deserialize)]
    struct Named {
        unknown_jobs: Vec<String>,
    }
    let reply = HeartbeatReply {
        unknown_jobs: vec!["j4".to_owned()],
        ..HeartbeatReply::default()
    };
    let named: Named = serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
    assert_eq!(named.unknown_jobs, ["j4"]);
}

/// Asserts the bounds of `unknown_jobs`: a bad id, an id twice and more than
/// [`limits::MAX_HELD`] ids each fail its check alone, and the stop beside it still passes;
/// a good list passes. It catches a bad list that costs the reply its commands, and one that
/// is taken.
#[test]
fn a_bad_unknown_jobs_list_is_refused_alone() {
    let with = |unknown_jobs: Vec<String>| HeartbeatReply {
        commands: vec![stop("j1", Some(StopReason::Ended))],
        unknown_jobs,
        ..HeartbeatReply::default()
    };
    let too_many = (0..=limits::MAX_HELD).map(|i| format!("j{i}")).collect();
    for bad in [
        vec!["-x".to_owned()],
        vec!["j2".to_owned(), "j2".to_owned()],
        too_many,
    ] {
        let reply = with(bad);
        let checked = reply.validate();
        assert!(checked.unknown_jobs.is_err());
        assert_eq!(checked.commands.len(), 1);
        assert!(checked.refused.is_empty());
    }
    let good = with(vec!["j2".to_owned(), "j3".to_owned()]);
    assert!(good.validate().unknown_jobs.is_ok());
}
