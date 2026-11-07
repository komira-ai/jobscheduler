//! What every case is made of: a node, the server's files, the server, the agent, and the two
//! ways a job is submitted and read (the command line and the gateway) behind one trait, so
//! each case runs the same steps through either.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::Receiver;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::agent::{Agent, HANG_GUARD};
use crate::cli::Js;
use crate::http::Gateway;
use crate::node::{Node, image};
use crate::server::{Files, Server, Store};

/// The most heartbeats a case lets pass while it waits for one change. A change the path
/// makes takes a handful (the start rides on one reply, the result on a later heartbeat); the
/// bound keeps a regression from looping and fails it naming what never happened.
pub const BEATS: usize = 50;

/// The store a case's server keeps its state in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A SQLite file, with `--allow-sqlite-store`.
    Sqlite,
    /// A Postgres database of the case's own.
    Postgres,
}

impl Kind {
    /// The name the case's name starts with.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }

    /// A new store of this kind.
    fn store(self) -> Store {
        match self {
            Self::Sqlite => Store::Sqlite,
            Self::Postgres => Store::postgres(),
        }
    }
}

/// How a case submits and reads its jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The `js` command line, over gRPC.
    Cli,
    /// The HTTP/JSON gateway, `/v1/jobs`.
    Http,
}

impl Via {
    /// The name the case's name carries.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Http => "http",
        }
    }
}

/// A job a case submits: what the stand-in is to do with it.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The job's id, and the stand-in's tag of it.
    pub id: String,
    /// The status the job exits with.
    pub exit: u8,
    /// Whether the stand-in holds it until the case lets it end.
    pub hold: bool,
    /// Whether it books the whole host.
    pub whole_host: bool,
}

impl Spec {
    /// A job `id` that prints its line and exits 0 at once.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            exit: 0,
            hold: false,
            whole_host: false,
        }
    }

    /// The same job, exiting with `exit`.
    pub fn exiting(mut self, exit: u8) -> Self {
        self.exit = exit;
        self
    }

    /// The same job, held by the stand-in until the case releases it.
    pub fn held(mut self) -> Self {
        self.hold = true;
        self
    }

    /// The same job, booking the whole host.
    pub fn whole_host(mut self) -> Self {
        self.whole_host = true;
        self
    }

    /// The line the job prints on its standard output.
    pub fn says(&self) -> String {
        format!("hello from {}", self.id)
    }

    /// The job's arguments: the words the stand-in reads.
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            format!("tag={}", self.id),
            format!("say={}", self.says()),
            format!("exit={}", self.exit),
        ];
        if self.hold {
            args.push("hold=yes".to_owned());
        }
        args
    }
}

/// The job service as a case uses it. Each answer is the service's own proto3 JSON: the job
/// of `GetJob`, the result of `GetResult`, and the whole answer of a tail-only `GetLogs`.
pub trait Jobs {
    /// Submits `spec`; the job as the service made it.
    fn submit(&self, spec: &Spec) -> Value;
    /// The job `id`.
    fn job(&self, id: &str) -> Value;
    /// The result of the job `id`.
    fn result(&self, id: &str) -> Value;
    /// The tails of the job `id`, with its state.
    fn logs(&self, id: &str) -> Value;
    /// Cancels the job `id`; the job as the service left it.
    fn cancel(&self, id: &str) -> Value;
}

/// The bytes a proto3 JSON `bytes` field holds, as text.
pub fn text(field: &Value) -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(field.as_str().unwrap_or_default())
        .expect("a base64 field");
    String::from_utf8(bytes).expect("UTF-8")
}

impl Jobs for Gateway {
    fn submit(&self, spec: &Spec) -> Value {
        let needs = match spec.whole_host {
            true => json!({ "wholeHost": true }),
            false => json!({ "cpuMillis": "1000", "memoryBytes": "268435456" }),
        };
        let body = json!({ "spec": {
            "jobId": spec.id,
            "image": image(),
            "command": "/bin/tool",
            "args": spec.args(),
            "needs": needs,
            "wallLimitSeconds": "600",
        }});
        let made = self.post("/v1/jobs", &body);
        assert_eq!(made.status, 201, "SubmitJob {}: {:?}", spec.id, made.body);
        made.body["job"].clone()
    }

    fn job(&self, id: &str) -> Value {
        Gateway::job(self, id)
    }

    fn result(&self, id: &str) -> Value {
        let got = self.get(&format!("/v1/jobs/{id}/result"));
        assert_eq!(got.status, 200, "GetResult {id}: {:?}", got.body);
        got.body["result"].clone()
    }

    fn logs(&self, id: &str) -> Value {
        let got = self.get(&format!("/v1/jobs/{id}/logs"));
        assert_eq!(got.status, 200, "GetLogs {id}: {:?}", got.body);
        got.body
    }

    fn cancel(&self, id: &str) -> Value {
        let got = self.post(&format!("/v1/jobs/{id}:cancel"), &json!({}));
        assert_eq!(got.status, 200, "CancelJob {id}: {:?}", got.body);
        got.body["job"].clone()
    }
}

impl Jobs for Js {
    fn submit(&self, spec: &Spec) -> Value {
        let image = image();
        let mut words = vec![
            "job",
            "submit",
            "--id",
            &spec.id,
            "--spec.image",
            &image,
            "--spec.command",
            "/bin/tool",
            "--spec.wall-limit-seconds",
            "600",
        ];
        let args = spec.args();
        for arg in &args {
            words.extend(["--spec.args", arg]);
        }
        match spec.whole_host {
            true => words.extend(["--spec.needs.whole-host", "true"]),
            false => words.extend([
                "--spec.needs.cpu-millis",
                "1000",
                "--spec.needs.memory-bytes",
                "268435456",
            ]),
        }
        self.run(&words).json()["job"].clone()
    }

    fn job(&self, id: &str) -> Value {
        self.run(&["job", "get", "--job-id", id]).json()["job"].clone()
    }

    fn result(&self, id: &str) -> Value {
        self.run(&["job", "result", "--job-id", id]).json()["result"].clone()
    }

    fn logs(&self, id: &str) -> Value {
        self.run(&["job", "logs", "--job-id", id]).json()
    }

    fn cancel(&self, id: &str) -> Value {
        self.run(&["job", "cancel", "--job-id", id]).json()["job"].clone()
    }
}

/// A case's server, agent and clients.
pub struct Rig {
    /// The agent's host.
    pub node: Node,
    /// The server's files, which a restart starts over.
    pub files: Files,
    /// The server.
    pub server: Server,
    /// The agent.
    pub agent: Agent,
    /// The gateway on the server's job port.
    pub http: Gateway,
    /// The command line, against the same port.
    pub js: Js,
    /// The FIFO readers of the held jobs submitted and not yet seen up, by id.
    ups: RefCell<BTreeMap<String, Receiver<()>>>,
}

impl Rig {
    /// A server over a new store of `kind` and an agent of a new node, registered as `h1`
    /// with `extra` flags, for the case `name`.
    pub fn new(name: &str, kind: Kind, extra: &[&str]) -> Self {
        Self::with_agent(name, kind, |node, server, token| {
            Agent::start(node, server, token, "h1", extra)
        })
    }

    /// [`Rig::new`] with the agent `start` starts.
    pub fn with_agent(
        name: &str,
        kind: Kind,
        start: impl FnOnce(&Node, &str, &Path) -> Agent,
    ) -> Self {
        let node = Node::new(name);
        let files = Files::new(name, kind.store());
        let server = Server::start(&files, "initialise");
        let agent = start(&node, &server.url(), &files.join_file());
        let http = Gateway::new(server.job);
        let js = Js::new(server.job, &files.token_file());
        Self {
            node,
            files,
            server,
            agent,
            http,
            js,
            ups: RefCell::default(),
        }
    }

    /// Submits `spec` through `via`; the job as the service made it. A held job's FIFO is made
    /// first, so [`Rig::until_up`] can wait on the stand-in's word.
    pub fn submit(&self, via: Via, spec: &Spec) -> Value {
        if spec.hold {
            let up = self.node.expect_up(&spec.id);
            self.ups.borrow_mut().insert(spec.id.clone(), up);
        }
        self.client(via).submit(spec)
    }

    /// The client `via` names.
    pub fn client(&self, via: Via) -> &dyn Jobs {
        match via {
            Via::Cli => &self.js,
            Via::Http => &self.http,
        }
    }

    /// The state of the job `id`, as the gateway says.
    pub fn state(&self, id: &str) -> String {
        self.http.state(id)
    }

    /// Lets the agent beat until `done` holds, at most [`BEATS`] times; fails naming `what`.
    pub fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        for _ in 0..BEATS {
            if done(self) {
                return;
            }
            self.agent.beat();
        }
        assert!(
            done(self),
            "{what}: not after {BEATS} heartbeats\nevents: {:?}\ncalls:\n{}\njobs: {}\nserver:\n{}",
            self.node.events(),
            self.node.calls(),
            self.http.get("/v1/jobs").body,
            self.server.err()
        );
    }

    /// Beats until the job `id` is in the ended state `state`; after each heartbeat, a job the
    /// agent runs is waited for until the agent has seen it end. Only for a job that ends: one
    /// held by the stand-in must have been released, or stopped.
    pub fn until_state(&self, id: &str, state: &str) {
        for _ in 0..BEATS {
            if self.state(id) == state {
                return;
            }
            self.agent.beat();
            self.agent.until_ended(id);
        }
        self.until(&format!("{id} {state}"), |rig| rig.state(id) == state);
    }

    /// Beats until the start of the held job `id` reached the agent, waits for the stand-in to
    /// say it is up, and beats until the server holds it running.
    pub fn until_up(&self, id: &str) {
        self.until(&format!("{id} at the agent"), |rig| rig.agent.holds(id));
        let up = self
            .ups
            .borrow_mut()
            .remove(id)
            .expect("a held job submitted with Rig::submit");
        assert!(
            up.recv_timeout(HANG_GUARD).is_ok(),
            "the stand-in never said {id} is up\nevents: {:?}\ncalls:\n{}",
            self.node.events(),
            self.node.calls()
        );
        self.until(&format!("{id} running"), |rig| {
            rig.state(id) == "JOB_STATE_RUNNING"
        });
    }

    /// Beats until the agent holds no job and has said so in a heartbeat, so the server has
    /// freed every allocation of its jobs.
    pub fn until_freed(&self) {
        self.until("the agent holds no job", |rig| rig.agent.holds_no_job());
        self.agent.beat();
    }

    /// How many times the stand-in started the job `id`.
    pub fn starts(&self, id: &str) -> usize {
        let start = format!("start {id}");
        self.node
            .events()
            .iter()
            .filter(|line| **line == start)
            .count()
    }
}
