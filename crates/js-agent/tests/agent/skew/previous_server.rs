//! This release's agent against the previous release's server: the agent program runs the
//! conversation, and every post it makes is answered with the frame the previous server
//! answered the previous agent with, after checking that the post says everything the
//! previous agent's post said (a field the previous server reads must still be there).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::{Answer, Connector, Transport};
use js_client::ClientError;
use serde_json::{Value, json};

use super::compare::uncovered;
use super::record::{Act, play, spawn};
use crate::support::{Ran, Scratch, code};

/// The `--server` of an agent that meets a transcript: never dialed, the connector answers.
const UNDIALED: &str = "http://previous-server.invalid";

/// The previous server, as a transcript answers for it.
#[derive(Clone)]
pub struct PreviousServer {
    exchanges: Arc<Mutex<VecDeque<Value>>>,
    problems: Arc<Mutex<Vec<String>>>,
    majors: Option<Value>,
}

impl PreviousServer {
    /// Answers with the exchanges of `flow`, in order; the job API events in it are the
    /// submitter's and are passed over.
    pub fn of(flow: &[Value]) -> Self {
        let exchanges = flow.iter().filter(|step| step.get("agent").is_some());
        Self {
            exchanges: Arc::new(Mutex::new(exchanges.cloned().collect())),
            problems: Arc::default(),
            majors: None,
        }
    }

    /// The same server, met by an agent whose hello says it speaks `majors` instead of its
    /// own, as an agent of a later major would.
    pub fn met_by(self, majors: Value) -> Self {
        Self {
            majors: Some(majors),
            ..self
        }
    }
}

struct Answering(PreviousServer);

impl Connector for PreviousServer {
    fn connect(
        &self,
        _server: &str,
        _token_file: &Path,
        _ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        Ok(Box::new(Answering(self.clone())))
    }
}

impl Transport for Answering {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let mut body = body.clone();
        if let (Some(majors), true) = (&self.0.majors, path == js_agent::HELLO_PATH) {
            body["protocol"] = majors.clone();
        }
        let mut problems = self.0.problems.lock().unwrap();
        let Some(exchange) = self.0.exchanges.lock().unwrap().pop_front() else {
            problems.push(format!("{path}: the previous server has no answer left"));
            return Err("the transcript is used up".to_owned());
        };
        let wanted = &exchange["agent"];
        if wanted["path"] != path {
            problems.push(format!(
                "{path}: the previous agent posted {}",
                wanted["path"]
            ));
        }
        let missing = uncovered(&wanted["body"], &body, None);
        problems.extend(missing.into_iter().map(|what| format!("{path} {what}")));
        let server = &exchange["server"];
        Ok(Answer {
            status: u16::try_from(server["status"].as_u64().unwrap()).unwrap(),
            body: server["body"].clone(),
            retry_after: server["retry_after_s"].as_u64().map(Duration::from_secs),
        })
    }

    fn adopt_credential(&mut self, _secret: &str) -> Result<(), String> {
        Ok(())
    }
}

/// What came of a conversation of this release's agent with the previous server.
pub struct Met {
    /// Every place where a post said less than the previous agent's post.
    pub problems: Vec<String>,
    /// The exchanges of the transcript the agent did not reach.
    pub left: usize,
    /// The waits the agent asked for, the one after its hello first.
    pub waits: Vec<Duration>,
    /// What the container runtime was asked to do with the job of the conversation.
    pub job_calls: Vec<String>,
    /// What the program printed and how it ended.
    pub ran: Ran,
}

/// Runs the conversation of this release's agent against `server`.
pub fn meet(name: &str, server: PreviousServer, job: &str) -> Met {
    let token = Scratch::new(&format!("{name}-token"));
    let token = token.write(
        "join-token",
        "join-token-unused-0123456789-abcdefghij",
        0o600,
    );
    let running = spawn(name, UNDIALED.to_owned(), &token, server.clone());
    let machine = running.machine.clone();
    let (waits, ran) = play(running, |_: Act| {});
    let job_calls = machine
        .world
        .log()
        .into_iter()
        .filter(|call| call.ends_with(job))
        .collect();
    let problems = std::mem::take(&mut *server.problems.lock().unwrap());
    let left = server.exchanges.lock().unwrap().len();
    Met {
        problems,
        left,
        waits,
        job_calls,
        ran,
    }
}

/// The job id of the conversation in `flow`.
pub fn job_of(flow: &[Value]) -> String {
    let submitted = flow.iter().find_map(|step| step["submitted"].as_str());
    submitted.expect("the transcript submits a job").to_owned()
}

/// Asserts that every post of this release's agent says what the previous agent's post
/// said, and that the agent takes every answer of the previous server: the welcome (each
/// wait is the 5 s interval it names), the job start (the container is created), the job
/// stop (it is stopped and removed), the acknowledgement of the killed result (the
/// heartbeat after lists no job, which the transcript checks), and that it ends cleanly
/// with the whole transcript spent. It catches a field this agent no longer sends, or
/// sends under another name or with another meaning, and a field of the previous server's
/// frames this agent no longer reads.
#[test]
fn this_agent_against_the_previous_server() {
    let transcript = super::previous();
    let flow = transcript["flow"].as_array().unwrap();
    let job = job_of(flow);
    let met = meet("skew-agent-new", PreviousServer::of(flow), &job);
    assert_eq!(met.problems, Vec::<String>::new());
    assert_eq!(met.left, 0);
    assert_eq!(met.waits, [Duration::from_secs(5); 7]);
    let calls = ["create", "stop", "remove"].map(|call| format!("{call} {job}"));
    assert_eq!(met.job_calls, calls);
    assert_eq!(met.ran.status(), code(0), "{}", met.ran.err);
}

/// Asserts that a hello the previous server cannot serve (it speaks majors 2 to 3, the
/// previous server major 1) is refused with the previous server's frame, status 400 and
/// `unsupported-protocol` with the majors it speaks, and that this agent then ends with
/// status 2 and says why, without trying again. It catches an agent that reads the
/// refusal of an older server as something to retry, or no longer reads its `supported`.
#[test]
fn the_previous_server_refuses_an_unknown_major_and_this_agent_stops() {
    let transcript = super::previous();
    let refusal = [transcript["refusal"].clone()];
    let server = PreviousServer::of(&refusal).met_by(json!({ "min": 2, "max": 3 }));
    let token = Scratch::new("skew-refused-token");
    let token = token.write(
        "join-token",
        "join-token-unused-0123456789-abcdefghij",
        0o600,
    );
    let running = spawn("skew-refused", UNDIALED.to_owned(), &token, server.clone());
    let ran = running.agent.finish();
    assert_eq!(*server.problems.lock().unwrap(), Vec::<String>::new());
    assert_eq!(server.exchanges.lock().unwrap().len(), 0);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said = "js-agent: the server refused the hello with status 400 (unsupported-protocol; \
                it speaks majors 1 to 1); not retrying";
    assert!(ran.err.lines().any(|line| line == said), "{}", ran.err);
}

/// Asserts that the check of this agent's posts fails, naming the field, when the previous
/// agent's heartbeat held a field this agent does not send (the transcript's first
/// heartbeat carries `phase` under another name, as if this agent had renamed it). It
/// catches a check that would pass an agent whose heartbeat lost a field the previous
/// server reads.
#[test]
fn a_field_this_agent_no_longer_sends_is_named() {
    let mut transcript = super::previous();
    let flow = transcript["flow"].as_array_mut().unwrap();
    let beat = flow
        .iter_mut()
        .find(|step| step["agent"]["path"] == js_agent::HEARTBEAT_PATH)
        .unwrap();
    let fields = beat["agent"]["body"].as_object_mut().unwrap();
    let phase = fields.remove("phase").unwrap();
    fields.insert("host_phase".to_owned(), phase);
    let job = job_of(flow);
    let met = meet("skew-agent-renamed", PreviousServer::of(flow), &job);
    assert_eq!(met.problems, ["/v1/agent/heartbeat $.host_phase: missing"]);
}

/// Asserts that this agent does not take a welcome of the previous server that lacks a
/// field it reads (the transcript's welcome carries `heartbeat_interval_ms` under another
/// name, as if this agent had renamed the field it reads): it does not register and asks
/// again, which the check names. It catches a test that would pass an agent that no
/// longer reads a field the previous server sends.
#[test]
fn a_field_this_agent_no_longer_reads_is_caught() {
    let mut transcript = super::previous();
    let flow = transcript["flow"].as_array_mut().unwrap();
    let fields = flow[0]["server"]["body"].as_object_mut().unwrap();
    let interval = fields.remove("heartbeat_interval_ms").unwrap();
    fields.insert("interval_ms".to_owned(), interval);
    let job = job_of(flow);
    let met = meet("skew-agent-unread", PreviousServer::of(flow), &job);
    let wanted = "/v1/agent/hello: the previous agent posted \"/v1/agent/heartbeat\"";
    assert_eq!(
        met.problems.first().map(String::as_str),
        Some(wanted),
        "{:?}",
        met.problems
    );
    assert_ne!(met.waits[0], Duration::from_secs(5));
    assert_eq!(met.job_calls, Vec::<String>::new());
}
