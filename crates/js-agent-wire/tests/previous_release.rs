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
    Ack, BookedResources, Command, Heartbeat, HeartbeatReply, HeldJob, HeldState, Hello, Isolation,
    JobResult, JobResultAck, JobStartSpec, LogGrant, LogGrantState, LogReport, LogSink, Outcome,
    PER_JOB_CAPABILITY, ResultAck, StartKind,
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

    /// The job start of an agent from before the live-log fields, the fields a test here
    /// reads: it has no `log_sink` and ignores what it does not know.
    #[derive(Debug, Deserialize)]
    pub struct JobStartSpec {
        pub job_id: String,
        pub allocation_id: String,
        #[serde(default)]
        pub image: String,
        #[serde(default)]
        pub command: Vec<String>,
        pub wall_ms: u64,
    }

    /// The per-job commands of that agent; any other reads as `Unknown`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum JobCommand {
        JobStart(Box<JobStartSpec>),
        JobStop {
            job_id: String,
        },
        #[serde(other)]
        Unknown,
    }

    /// The reply of that agent: its commands one by one, and the acknowledgements; no
    /// `log_grant`.
    #[derive(Debug, Deserialize)]
    pub struct JobReply {
        #[serde(default)]
        pub commands: Vec<JobCommand>,
        #[serde(default)]
        pub job_acks: Vec<JobAck>,
    }

    #[derive(Debug, Deserialize)]
    pub struct JobAck {
        pub job_id: String,
        pub ack: String,
    }

    /// The heartbeat of that agent as its server read it: the jobs and their results,
    /// with no `log_grant_state` and no `logs`.
    #[derive(Debug, Deserialize)]
    pub struct JobBeat {
        pub host_id: String,
        #[serde(default)]
        pub jobs: Vec<JobListing>,
    }

    #[derive(Debug, Deserialize)]
    pub struct JobListing {
        pub job_id: String,
        pub state: String,
        #[serde(default)]
        pub result: Option<JobOutcome>,
    }

    #[derive(Debug, Deserialize)]
    pub struct JobOutcome {
        pub job_id: String,
        pub outcome: String,
        #[serde(default)]
        pub exit_code: Option<i32>,
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
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
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
                logs: None,
                input_failure: None,
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

/// Asserts that an agent and a server from before the live-log fields read every message
/// that carries them as they read it without: a reply whose job start carries a `log_sink`
/// and which carries a `log_grant` decodes with the start, the stop beside it and the
/// acknowledgement intact, and a heartbeat with a `log_grant_state` whose listing's result
/// carries `logs` decodes with its jobs and result intact. Catches a log field that an
/// older peer cannot skip, which would cost it the job start or the whole message.
#[test]
fn an_old_agent_ignores_log_sink() {
    let Command::JobStart(mut spec) = job_start("build-42") else {
        unreachable!("job_start makes a job start");
    };
    spec.log_sink = Some(LogSink {
        endpoint: "https://store.example".into(),
        region: "us-east-1".into(),
        bucket: "logs".into(),
        prefix: "js/build-42/".into(),
        path_style: false,
    });
    assert_eq!(spec.log_sink_checked().map(|sink| sink.is_some()), Ok(true));
    let reply = HeartbeatReply {
        commands: vec![
            Command::JobStart(spec),
            Command::JobStop {
                job_id: "build-41".into(),
            },
        ],
        job_acks: vec![JobResultAck {
            job_id: "build-40".into(),
            ack: Ack::Accepted,
        }],
        log_grant: Some(LogGrant {
            access_key_id: "testkeyid-0001".into(),
            secret_access_key: "secret".into(),
            session_token: Some("token".into()),
            expires_at_ms: 1_800_000_000_000,
        }),
        ..HeartbeatReply::default()
    };
    assert!(reply.validate().refused.is_empty());
    let wire = serde_json::to_value(&reply).unwrap();
    assert!(wire["commands"][0].get("log_sink").is_some());
    let old: previous::JobReply = serde_json::from_value(wire.clone()).unwrap();
    let previous::JobCommand::JobStart(start) = &old.commands[0] else {
        panic!("the job start is lost: {:?}", old.commands);
    };
    assert_eq!(
        (start.job_id.as_str(), start.allocation_id.as_str()),
        ("build-42", "0123456789abcdef0123456789abcdef")
    );
    assert!(start.image.starts_with("ubuntu:24.04@sha256:"));
    assert_eq!(
        (start.command.clone(), start.wall_ms),
        (vec!["make".to_owned()], 60_000)
    );
    assert!(
        matches!(&old.commands[1], previous::JobCommand::JobStop { job_id } if job_id == "build-41")
    );
    assert_eq!(
        (
            old.job_acks[0].job_id.as_str(),
            old.job_acks[0].ack.as_str()
        ),
        ("build-40", "ACCEPTED")
    );
    let v0: previous::HeartbeatReply = serde_json::from_value(wire).unwrap();
    assert!(matches!(v0.commands[0], previous::Command::Unknown));

    let beat = Heartbeat {
        jobs: vec![HeldJob {
            job_id: "build-42".into(),
            state: HeldState::ExitedUnacked,
            result: Some(JobResult {
                job_id: "build-42".into(),
                allocation_id: "0123456789abcdef0123456789abcdef".into(),
                outcome: Outcome::Failed,
                exit_code: Some(2),
                detail: None,
                log_tail: None,
                logs: Some(LogReport {
                    stdout_bytes: 100,
                    stderr_bytes: 7,
                    stdout_dropped: 0,
                    stderr_dropped: 0,
                }),
                input_failure: None,
            }),
        }],
        log_grant_state: Some(LogGrantState {
            stale: true,
            until_ms: None,
        }),
        ..heartbeat()
    };
    assert_eq!(beat.validate(), Ok(()));
    let wire = serde_json::to_value(&beat).unwrap();
    assert!(wire.get("log_grant_state").is_some());
    let old: previous::JobBeat = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(old.host_id, "mac-01");
    let listing = &old.jobs[0];
    assert_eq!(
        (listing.job_id.as_str(), listing.state.as_str()),
        ("build-42", "exited_unacked")
    );
    let result = listing.result.as_ref().unwrap();
    assert_eq!(
        (
            result.job_id.as_str(),
            result.outcome.as_str(),
            result.exit_code
        ),
        ("build-42", "failed", Some(2))
    );
    let v0: previous::Heartbeat = serde_json::from_value(wire).unwrap();
    assert_eq!(v0.held.len(), 1);
}

/// Asserts that a server from before `input_failure` reads a heartbeat whose job result
/// carries one as it read the result without: the listing, the outcome and the exit status
/// whole, the input failure skipped. Catches an input failure that an older server cannot
/// skip, which would cost it the heartbeat and every result in it.
#[test]
fn an_old_server_ignores_input_failure() {
    let beat = Heartbeat {
        jobs: vec![HeldJob {
            job_id: "build-43".into(),
            state: HeldState::ExitedUnacked,
            result: Some(JobResult {
                job_id: "build-43".into(),
                allocation_id: "0123456789abcdef0123456789abcdef".into(),
                outcome: Outcome::Succeeded,
                exit_code: Some(0),
                detail: None,
                log_tail: None,
                logs: None,
                input_failure: Some(js_agent_wire::refusal::TREE_STALLED.into()),
            }),
        }],
        ..heartbeat()
    };
    assert_eq!(beat.validate(), Ok(()));
    let wire = serde_json::to_value(&beat).unwrap();
    assert_eq!(
        wire["jobs"][0]["result"]["input_failure"],
        json!("tree-stalled")
    );
    let old: previous::JobBeat = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(old.host_id, "mac-01");
    let listing = &old.jobs[0];
    assert_eq!(
        (listing.job_id.as_str(), listing.state.as_str()),
        ("build-43", "exited_unacked")
    );
    let result = listing.result.as_ref().unwrap();
    assert_eq!(
        (
            result.job_id.as_str(),
            result.outcome.as_str(),
            result.exit_code
        ),
        ("build-43", "succeeded", Some(0))
    );
    let v0: previous::Heartbeat = serde_json::from_value(wire).unwrap();
    assert_eq!(v0.held.len(), 1);
}

/// Asserts that an agent from before the `kind` of a job start reads a VM start with the
/// start and the stop beside it intact: the field is skipped, never a decode error. (Such an
/// agent would run it as an action, which is why a server sends `kind: vm` only to a host
/// whose hello reports `vm.slots`.) Catches a VM start that costs an old agent the reply.
#[test]
fn an_old_agent_skips_the_kind_of_a_vm_start() {
    let Command::JobStart(mut spec) = job_start("vm-42") else {
        unreachable!("job_start makes a job start");
    };
    spec.kind = StartKind::Vm;
    let reply = HeartbeatReply {
        commands: vec![
            Command::JobStart(spec),
            Command::JobStop {
                job_id: "vm-41".into(),
            },
        ],
        ..HeartbeatReply::default()
    };
    let wire = serde_json::to_value(&reply).unwrap();
    assert_eq!(wire["commands"][0]["kind"], "vm");
    let old: previous::JobReply = serde_json::from_value(wire).unwrap();
    let previous::JobCommand::JobStart(start) = &old.commands[0] else {
        panic!("the job start is lost: {:?}", old.commands);
    };
    assert_eq!(
        (start.job_id.as_str(), start.command.clone(), start.wall_ms),
        ("vm-42", vec!["make".to_owned()], 60_000)
    );
    assert!(
        matches!(&old.commands[1], previous::JobCommand::JobStop { job_id } if job_id == "vm-41")
    );
}

/// Asserts that an agent from before the `isolation` of a job start reads a host start with
/// the start and the stop beside it intact: the field is skipped, never a decode error. (Such
/// an agent would run it contained, which is why a server sends `isolation: host` only to a
/// host whose hello reports `host-jobs`.) Catches a host start that costs an old agent the
/// reply.
#[test]
fn an_old_agent_skips_the_isolation_of_a_host_start() {
    let Command::JobStart(mut spec) = job_start("host-42") else {
        unreachable!("job_start makes a job start");
    };
    spec.isolation = Isolation::Host;
    let reply = HeartbeatReply {
        commands: vec![
            Command::JobStart(spec),
            Command::JobStop {
                job_id: "host-41".into(),
            },
        ],
        ..HeartbeatReply::default()
    };
    let wire = serde_json::to_value(&reply).unwrap();
    assert_eq!(wire["commands"][0]["isolation"], "host");
    let old: previous::JobReply = serde_json::from_value(wire).unwrap();
    let previous::JobCommand::JobStart(start) = &old.commands[0] else {
        panic!("the job start is lost: {:?}", old.commands);
    };
    assert_eq!(
        (start.job_id.as_str(), start.command.clone(), start.wall_ms),
        ("host-42", vec!["make".to_owned()], 60_000)
    );
    assert!(
        matches!(&old.commands[1], previous::JobCommand::JobStop { job_id } if job_id == "host-41")
    );
}
