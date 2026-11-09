//! The wire of release `v0.1.208`, which queued runs and whole-node leases, against this
//! version, which does not: an agent of that release still sends run-keyed hellos and
//! heartbeats, and still reads run-keyed commands.
//!
//! The release's bodies are files under `fixtures/v0.1.208`, written by that tag's code, so
//! they cannot follow a change to the types here. Its reader is a copy of the structs a
//! v0.1.208 agent decodes a reply with, kept below as they were: it must not follow later
//! changes either, because it stands for agents already deployed.

use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldState, Hello, JobResultAck, Outcome,
    PER_JOB_CAPABILITY, ResultAck, START_FENCE_CAPABILITY, limits,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

const HELLO_PER_JOB: &str = include_str!("fixtures/v0.1.208/hello_per_job.json");
const HELLO_NATIVE: &str = include_str!("fixtures/v0.1.208/hello_native.json");
const HEARTBEAT: &str = include_str!("fixtures/v0.1.208/heartbeat.json");
const REPLY: &str = include_str!("fixtures/v0.1.208/reply.json");

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// The structs a v0.1.208 agent reads a heartbeat reply with: every command decoded one by
/// one (a command it cannot decode is `Unknown`), and the acknowledgements. Fields it
/// does not know are ignored, as they were.
mod previous {
    use super::{Deserialize, Deserializer, Value};

    /// `Ack` of that release: an acknowledgement it does not know reads as `Unknown`, which
    /// makes the agent keep the result and send it again.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
    pub enum Ack {
        Accepted,
        Duplicate,
        Stale,
        UnknownLease,
        Mismatch,
        #[serde(other)]
        Unknown,
    }

    /// The run start of that release, the fields a test here reads.
    #[derive(Debug, Deserialize)]
    pub struct RunStart {
        pub run_id: String,
        pub lease_id: String,
    }

    /// The job start of that release, the fields a test here reads.
    #[derive(Debug, Deserialize)]
    pub struct JobStart {
        pub job_id: String,
        pub allocation_id: String,
        pub wall_ms: u64,
    }

    /// `Command` of that release: tagged by `type`, an unknown tag reads as `Unknown`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Command {
        Start(Box<RunStart>),
        Stop {
            run_id: String,
        },
        Cancel {
            run_id: String,
        },
        JobStart(Box<JobStart>),
        JobStop {
            job_id: String,
        },
        #[serde(other)]
        Unknown,
    }

    #[derive(Debug, Deserialize)]
    pub struct ResultAck {
        pub run_id: String,
        pub ack: Ack,
    }

    #[derive(Debug, Deserialize)]
    pub struct JobAck {
        pub job_id: String,
        pub ack: Ack,
    }

    /// `HeartbeatReply` of that release, with its one-by-one decoding of the commands.
    #[derive(Debug, Deserialize)]
    pub struct HeartbeatReply {
        #[serde(default, deserialize_with = "commands_or_unknown")]
        pub commands: Vec<Command>,
        #[serde(default)]
        pub result_acks: Vec<ResultAck>,
        #[serde(default)]
        pub taint_ack: bool,
        #[serde(default)]
        pub exclusive: Option<String>,
        #[serde(default)]
        pub job_acks: Vec<JobAck>,
        #[serde(default)]
        pub retired_acks: Vec<String>,
    }

    fn commands_or_unknown<'de, D>(deserializer: D) -> Result<Vec<Command>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let items = Vec::<Value>::deserialize(deserializer)?;
        Ok(items
            .into_iter()
            .map(|item| Command::deserialize(item).unwrap_or(Command::Unknown))
            .collect())
    }
}

/// Asserts that the hello of a v0.1.208 container agent decodes, passes the hello's checks
/// and encodes back to the same JSON (so every field is kept), and reports what the release
/// reported: `per-job` and `start-fence` set to `true`, and `runtime` and `exec` both
/// `container`. It catches a field or capability the current types drop or refuse, which
/// would lock every deployed container agent out.
#[test]
fn a_v0_1_208_per_job_hello_decodes_with_every_field_kept() {
    let hello: Hello = serde_json::from_value(fixture(HELLO_PER_JOB)).unwrap();
    assert_eq!(hello.validate(), Ok(()));
    assert_eq!(hello.capabilities[PER_JOB_CAPABILITY], "true");
    assert_eq!(hello.capabilities[START_FENCE_CAPABILITY], "true");
    assert_eq!(hello.capabilities["exec"], "container");
    assert_eq!(hello.capabilities["runtime"], "container");
    assert_eq!(
        (hello.host_id.as_str(), hello.pool.as_str()),
        ("builder-01", "builders")
    );
    assert_eq!(hello.shape.disk_bytes, 500 << 30);
    assert!(hello.held.is_empty());
    assert_eq!(
        serde_json::to_value(&hello).unwrap(),
        fixture(HELLO_PER_JOB)
    );
}

/// Asserts that the hello of a v0.1.208 native agent decodes, passes the hello's checks and
/// encodes back to the same JSON, that it reports neither `per-job` nor `start-fence`
/// (`exec` is `sandbox-exec`), and that the run it lists as held is kept. It catches a
/// current check that would refuse a run-keyed agent's restart hello.
#[test]
fn a_v0_1_208_native_hello_decodes_without_per_job_and_keeps_its_held_run() {
    let hello: Hello = serde_json::from_value(fixture(HELLO_NATIVE)).unwrap();
    assert_eq!(hello.validate(), Ok(()));
    assert!(!hello.capabilities.contains_key(PER_JOB_CAPABILITY));
    assert!(!hello.capabilities.contains_key(START_FENCE_CAPABILITY));
    assert_eq!(hello.capabilities["exec"], "sandbox-exec");
    assert_eq!(hello.held.len(), 1);
    assert_eq!(hello.held[0].run_id, "run-1");
    assert_eq!(hello.held[0].state, HeldState::Running);
    assert_eq!(serde_json::to_value(&hello).unwrap(), fixture(HELLO_NATIVE));
}

/// Asserts that a v0.1.208 heartbeat with held runs, run results, a load with lease minutes,
/// listed jobs and a retired job decodes, passes the heartbeat's checks and encodes back to
/// the same JSON, and that the held runs, the results (their lease ids and outcomes) and
/// the jobs read as the release wrote them. It catches a heartbeat field the current types
/// drop or refuse; a refused heartbeat is an agent the server can no longer hear.
#[test]
fn a_v0_1_208_heartbeat_with_held_runs_results_and_jobs_decodes_with_every_field_kept() {
    let beat: Heartbeat = serde_json::from_value(fixture(HEARTBEAT)).unwrap();
    assert_eq!(beat.validate(), Ok(()));
    let held: Vec<(&str, HeldState)> = beat
        .held
        .iter()
        .map(|run| (run.run_id.as_str(), run.state))
        .collect();
    assert_eq!(
        held,
        [
            ("run-1", HeldState::Running),
            ("run-2", HeldState::ExitedUnacked)
        ]
    );
    let results: Vec<(&str, &str, Outcome)> = beat
        .results
        .iter()
        .map(|result| {
            (
                result.run_id.as_str(),
                result.lease_id.as_str(),
                result.outcome,
            )
        })
        .collect();
    assert_eq!(
        results,
        [
            ("run-2", "7-3", Outcome::Succeeded),
            ("run-3", "7-4", Outcome::TimedOut)
        ]
    );
    assert_eq!(beat.results[1].exit_code, Some(137));
    assert_eq!(beat.results[1].detail.as_deref(), Some("wall limit"));
    assert!(beat.load.is_some());
    assert_eq!(beat.disk_gib, Some(400));
    assert_eq!(beat.jobs.len(), 2);
    assert_eq!(beat.jobs[1].result.as_ref().unwrap().job_id, "job-2");
    assert_eq!(beat.retired_jobs, ["job-0"]);
    assert_eq!(serde_json::to_value(&beat).unwrap(), fixture(HEARTBEAT));
}

/// Asserts that a v0.1.208 server's reply, which carries run-keyed and job commands side by
/// side with a result acknowledgement of each kind, decodes, passes the reply's checks (no
/// command refused, acknowledgements well formed) and encodes back to the same JSON. It
/// catches a command or an acknowledgement the current types no longer read, which would
/// break a new agent talking to a server that is not yet updated.
#[test]
fn a_v0_1_208_reply_decodes_with_every_field_kept() {
    let reply: HeartbeatReply = serde_json::from_value(fixture(REPLY)).unwrap();
    let checked = reply.validate();
    assert!(checked.refused.is_empty(), "{:?}", checked.refused);
    assert_eq!(
        (checked.acks, checked.job_acks, checked.retired_acks),
        (Ok(()), Ok(()), Ok(()))
    );
    assert!(matches!(&reply.commands[0], Command::Start(spec) if spec.run_id == "run-1"));
    assert!(matches!(&reply.commands[1], Command::Stop { run_id } if run_id == "run-4"));
    assert!(matches!(&reply.commands[2], Command::Cancel { run_id } if run_id == "run-1"));
    assert!(matches!(&reply.commands[3], Command::JobStart(spec) if spec.job_id == "job-3"));
    assert!(matches!(&reply.commands[4], Command::JobStop { job_id } if job_id == "job-1"));
    assert_eq!(reply.commands.len(), 5);
    assert_eq!(
        reply.result_acks,
        [
            ResultAck {
                run_id: "run-2".into(),
                ack: Ack::Accepted
            },
            ResultAck {
                run_id: "run-3".into(),
                ack: Ack::UnknownLease
            },
        ]
    );
    assert_eq!(
        reply.job_acks,
        [JobResultAck {
            job_id: "job-2".into(),
            ack: Ack::Accepted
        }]
    );
    assert_eq!(reply.retired_acks, ["job-0"]);
    assert_eq!(serde_json::to_value(&reply).unwrap(), fixture(REPLY));
}

/// Builds the reply this version's server gives an agent that reports held runs and
/// results: a cancel for each held run (at most the wire's cap) and `UnknownLease` for
/// each result, and nothing else.
fn reply_for(beat: &Heartbeat) -> HeartbeatReply {
    HeartbeatReply {
        commands: beat
            .held
            .iter()
            .take(limits::MAX_COMMANDS)
            .map(|run| Command::Cancel {
                run_id: run.run_id.clone(),
            })
            .collect(),
        result_acks: beat
            .results
            .iter()
            .map(|result| ResultAck {
                run_id: result.run_id.clone(),
                ack: Ack::UnknownLease,
            })
            .collect(),
        ..HeartbeatReply::default()
    }
}

/// Asserts that the reply this version's server builds for the v0.1.208 heartbeat (a cancel
/// for each held run, an `UnknownLease` acknowledgement for each result, empty commands
/// otherwise) passes the reply's checks and decodes with the copy of that release's
/// structs: the cancels as cancels in the order held, each acknowledgement as `UnknownLease`
/// and not as the `Unknown` that an agent resends a result on, and nothing else set. It
/// catches a reply an old agent cannot read, or reads as an order other than to drop its runs
/// and results.
#[test]
fn the_reply_for_an_old_agent_decodes_with_the_v0_1_208_structs() {
    let beat: Heartbeat = serde_json::from_value(fixture(HEARTBEAT)).unwrap();
    let reply = reply_for(&beat);
    let checked = reply.validate();
    assert!(checked.refused.is_empty(), "{:?}", checked.refused);
    assert_eq!(checked.acks, Ok(()));

    let old: previous::HeartbeatReply =
        serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
    let cancelled: Vec<&str> = old
        .commands
        .iter()
        .map(|command| match command {
            previous::Command::Cancel { run_id } => run_id.as_str(),
            other => panic!("not a cancel: {other:?}"),
        })
        .collect();
    assert_eq!(cancelled, ["run-1", "run-2"]);
    let acks: Vec<(&str, previous::Ack)> = old
        .result_acks
        .iter()
        .map(|ack| (ack.run_id.as_str(), ack.ack))
        .collect();
    assert_eq!(
        acks,
        [
            ("run-2", previous::Ack::UnknownLease),
            ("run-3", previous::Ack::UnknownLease)
        ]
    );
    assert!(!old.taint_ack);
    assert_eq!(old.exclusive, None);
    assert!(old.job_acks.is_empty());
    assert!(old.retired_acks.is_empty());
}

/// Asserts that the same reply with the job half the transport adds (a job start and a job
/// stop beside the cancels, a job and a retired acknowledgement) decodes with the copy of
/// that release's structs, the run-keyed and job commands each as themselves and in order.
/// It catches a job command that costs an old agent the cancels beside it.
#[test]
fn the_cancels_for_an_old_agent_survive_the_job_commands_beside_them() {
    let beat: Heartbeat = serde_json::from_value(fixture(HEARTBEAT)).unwrap();
    let mut reply = reply_for(&beat);
    let sent: HeartbeatReply = serde_json::from_value(fixture(REPLY)).unwrap();
    reply.commands.extend(
        sent.commands
            .into_iter()
            .filter(|command| matches!(command, Command::JobStart(_) | Command::JobStop { .. })),
    );
    reply.job_acks = sent.job_acks;
    reply.retired_acks = sent.retired_acks;
    assert!(reply.validate().refused.is_empty());

    let old: previous::HeartbeatReply =
        serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
    assert_eq!(old.commands.len(), 4);
    assert!(matches!(&old.commands[0], previous::Command::Cancel { run_id } if run_id == "run-1"));
    assert!(matches!(&old.commands[1], previous::Command::Cancel { run_id } if run_id == "run-2"));
    assert!(
        matches!(&old.commands[2], previous::Command::JobStart(spec) if (spec.job_id.as_str(), spec.allocation_id.as_str(), spec.wall_ms) == ("job-3", "fedcba9876543210fedcba9876543210", 600_000))
    );
    assert!(matches!(&old.commands[3], previous::Command::JobStop { job_id } if job_id == "job-1"));
    assert_eq!(old.result_acks.len(), 2);
    assert_eq!(old.job_acks[0].job_id, "job-2");
    assert_eq!(old.job_acks[0].ack, previous::Ack::Accepted);
    assert_eq!(old.retired_acks, ["job-0"]);
}

/// Asserts that the copy of that release's structs reads the release's own reply as that
/// release did: the run start, stop and cancel, the job start and stop, each
/// acknowledgement as its own verdict. It shows the copy is faithful, so the tests above
/// that decode with it prove what a deployed agent would do.
#[test]
fn the_v0_1_208_structs_read_the_v0_1_208_reply() {
    let old: previous::HeartbeatReply = serde_json::from_value(fixture(REPLY)).unwrap();
    assert!(matches!(
        &old.commands[0],
        previous::Command::Start(spec) if (spec.run_id.as_str(), spec.lease_id.as_str()) == ("run-1", "1-1")
    ));
    assert!(matches!(&old.commands[1], previous::Command::Stop { run_id } if run_id == "run-4"));
    assert!(matches!(&old.commands[2], previous::Command::Cancel { run_id } if run_id == "run-1"));
    assert!(
        matches!(&old.commands[3], previous::Command::JobStart(spec) if spec.job_id == "job-3")
    );
    assert!(matches!(&old.commands[4], previous::Command::JobStop { job_id } if job_id == "job-1"));
    assert_eq!(old.commands.len(), 5);
    assert_eq!(old.result_acks[0].ack, previous::Ack::Accepted);
    assert_eq!(old.result_acks[1].ack, previous::Ack::UnknownLease);
    assert_eq!(old.job_acks[0].ack, previous::Ack::Accepted);
}
