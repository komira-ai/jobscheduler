//! Both directions of the wire between this version and the previous release (`v0.1.135`),
//! across the per-job messages this version adds.
//!
//! The previous release's bodies are files under `fixtures/v0.1.135`, written from that
//! tag's golden tests, so they cannot follow a change to the types here. Its decoding is a
//! copy of that release's structs, kept below as it was: it must not follow later changes
//! either, because it stands for agents and servers already deployed.

mod common;

use common::{heartbeat, job};
use js_agent_wire::{
    Ack, BookedResources, Command, Heartbeat, HeartbeatReply, HeldJob, HeldState, Hello, JobResult,
    JobResultAck, JobStartSpec, Outcome, PER_JOB_CAPABILITY, ResultAck,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

const HELLO: &str = include_str!("fixtures/v0.1.135/hello.json");
const HEARTBEAT: &str = include_str!("fixtures/v0.1.135/heartbeat.json");
const REPLY: &str = include_str!("fixtures/v0.1.135/reply.json");

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Asserts that every body of the previous release decodes with this version's types and
/// passes their checks, that the new lists read as empty (a heartbeat has no `jobs` and no
/// `retired_jobs`, a reply no `job_acks` and no `retired_acks`), that encoding the decoded
/// body again gives the same JSON (nothing new is sent when it is empty), and that the
/// previous hello reports no `per-job`. It catches a new field without a default, which
/// would make every deployed agent's heartbeat, or every deployed server's reply,
/// unreadable.
#[test]
fn previous_agent_wire_is_still_accepted_by_the_new_wire_types() {
    let hello: Hello = serde_json::from_value(fixture(HELLO)).unwrap();
    assert_eq!(hello.validate(), Ok(()));
    assert!(!hello.capabilities.contains_key(PER_JOB_CAPABILITY));
    assert_eq!(serde_json::to_value(&hello).unwrap(), fixture(HELLO));

    let beat: Heartbeat = serde_json::from_value(fixture(HEARTBEAT)).unwrap();
    assert_eq!(beat.validate(), Ok(()));
    assert!(beat.jobs.is_empty());
    assert!(beat.retired_jobs.is_empty());
    assert_eq!(beat.held[0].run_id, "run-1");
    assert_eq!(beat.results[0].outcome, Outcome::TimedOut);
    assert_eq!(serde_json::to_value(&beat).unwrap(), fixture(HEARTBEAT));

    let reply: HeartbeatReply = serde_json::from_value(fixture(REPLY)).unwrap();
    let checked = reply.validate();
    assert!(checked.refused.is_empty(), "{:?}", checked.refused);
    assert_eq!(checked.commands.len(), 3);
    assert_eq!(
        (checked.acks, checked.job_acks, checked.retired_acks),
        (Ok(()), Ok(()), Ok(()))
    );
    assert!(matches!(&reply.commands[0], Command::Start(spec) if spec.run_id == "run-1"));
    assert!(reply.job_acks.is_empty());
    assert!(reply.retired_acks.is_empty());
    assert_eq!(serde_json::to_value(&reply).unwrap(), fixture(REPLY));
}

/// The structs of the previous release, as far as the fields a receiver of that release
/// read. Unknown fields are ignored, as they were.
mod previous {
    use super::{Deserialize, Deserializer, Value};

    /// `JobSpec` of that release, the fields a test here reads.
    #[derive(Debug, Deserialize)]
    pub struct JobSpec {
        pub run_id: String,
        pub lease_id: String,
        #[serde(default)]
        pub image: String,
    }

    /// `Command` of that release: tagged by `type`, an unknown tag reads as `Unknown`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Command {
        Start(Box<JobSpec>),
        Stop {
            run_id: String,
        },
        Cancel {
            run_id: String,
        },
        #[serde(other)]
        Unknown,
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
    }

    #[derive(Debug, Deserialize)]
    pub struct ResultAck {
        pub run_id: String,
        pub ack: String,
    }

    /// The release's `commands_or_unknown`: a command that does not decode is `Unknown`.
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

    /// `Heartbeat` of that release, as its server read it.
    #[derive(Debug, Deserialize)]
    pub struct Heartbeat {
        pub host_id: String,
        pub epoch: u64,
        #[serde(default)]
        pub session: String,
        pub phase: String,
        #[serde(default)]
        pub held: Vec<HeldRun>,
        #[serde(default)]
        pub results: Vec<RunResult>,
    }

    #[derive(Debug, Deserialize)]
    pub struct HeldRun {
        pub run_id: String,
        pub state: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct RunResult {
        pub run_id: String,
        pub lease_id: String,
        pub outcome: String,
    }
}

fn job_start(id: &str) -> Command {
    Command::JobStart(Box::new(JobStartSpec {
        job_id: id.into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image:
            "ubuntu:24.04@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        command: vec!["make".into()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: Some(1 << 30),
            gpu_indices: vec![1],
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
    }))
}

/// Asserts the other direction: a reply of this version with a job start and a job stop
/// around a start, a stop and a cancel decodes with the previous release's types, the two job
/// commands as `Unknown` and the start and stop beside them intact, its `job_acks` and
/// `retired_acks` ignored; and a heartbeat of this version that lists jobs and retired job
/// ids decodes there with its held runs and results as they are. It catches a new command
/// that breaks the old enum (the whole reply lost, stops included) and a new field that an
/// old server cannot skip.
#[test]
fn previous_agent_wire_reads_the_new_wire_types_without_error() {
    let reply = HeartbeatReply {
        commands: vec![
            job_start("job-1"),
            Command::Start(Box::new(job())),
            Command::JobStop {
                job_id: "job-2".into(),
            },
            Command::Stop {
                run_id: "r1".into(),
            },
            Command::Cancel {
                run_id: "r2".into(),
            },
        ],
        result_acks: vec![ResultAck {
            run_id: "r3".into(),
            ack: Ack::Stale,
        }],
        job_acks: vec![JobResultAck {
            job_id: "job-3".into(),
            ack: Ack::Accepted,
        }],
        retired_acks: vec!["job-2".into()],
        ..HeartbeatReply::default()
    };
    assert!(reply.validate().refused.is_empty());
    let wire = serde_json::to_value(&reply).unwrap();
    let old: previous::HeartbeatReply = serde_json::from_value(wire).unwrap();
    assert_eq!(old.commands.len(), 5);
    assert!(matches!(old.commands[0], previous::Command::Unknown));
    let previous::Command::Start(spec) = &old.commands[1] else {
        panic!("the start beside the job start is lost: {:?}", old.commands);
    };
    assert_eq!(
        (spec.run_id.as_str(), spec.lease_id.as_str()),
        ("run-1", "1-1")
    );
    assert!(spec.image.starts_with("ubuntu:24.04@sha256:"));
    assert!(matches!(old.commands[2], previous::Command::Unknown));
    assert!(matches!(&old.commands[3], previous::Command::Stop { run_id } if run_id == "r1"));
    assert!(matches!(&old.commands[4], previous::Command::Cancel { run_id } if run_id == "r2"));
    assert_eq!(old.result_acks.len(), 1);
    assert_eq!(
        (
            old.result_acks[0].run_id.as_str(),
            old.result_acks[0].ack.as_str()
        ),
        ("r3", "STALE")
    );
    assert!(!old.taint_ack);
    assert_eq!(old.exclusive, None);

    let beat = Heartbeat {
        jobs: vec![HeldJob {
            job_id: "job-1".into(),
            state: HeldState::ExitedUnacked,
            result: Some(JobResult {
                job_id: "job-1".into(),
                allocation_id: "0123456789abcdef0123456789abcdef".into(),
                outcome: Outcome::Succeeded,
                exit_code: Some(0),
                detail: None,
                log_tail: None,
            }),
        }],
        retired_jobs: vec!["job-2".into()],
        ..heartbeat()
    };
    assert_eq!(beat.validate(), Ok(()));
    let wire = serde_json::to_value(&beat).unwrap();
    assert_eq!(wire["jobs"][0]["job_id"], json!("job-1"));
    let old: previous::Heartbeat = serde_json::from_value(wire).unwrap();
    assert_eq!((old.host_id.as_str(), old.epoch), ("mac-01", 3));
    assert_eq!(old.session, "s3cret-0");
    assert_eq!(old.phase, "draining");
    assert_eq!(old.held.len(), 1);
    assert_eq!(
        (old.held[0].run_id.as_str(), old.held[0].state.as_str()),
        ("run-1", "exited_unacked")
    );
    assert_eq!(
        (
            old.results[0].run_id.as_str(),
            old.results[0].lease_id.as_str(),
            old.results[0].outcome.as_str()
        ),
        ("run-1", "1-1", "timed_out")
    );
}
