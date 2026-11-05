//! A scripted stand-in for the lease server, and the test-only node kind the kernel runs.
//!
//! The stand-in welcomes a hello with a made-up session, answers each heartbeat with the
//! answer the test scripted for its number (counted from 1 over the whole run, whatever
//! the session), and records every call it received after checking it with the wire's
//! own `validate()`. Anything the script does not name gets the plain answer: a welcome,
//! or an empty heartbeat reply.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{Heartbeat, Hello};
use js_sim::{Event, Faults, Node, NodeId, NodeInput, Output, Sim};
use js_sim_cell::bus::{self, SERVER};
use js_sim_cell::{AgentNode, AgentSpec, CallId, Ledger, Msg, Route};
use js_types::sched::{Effect, FarmTime, StateMachine};
use serde_json::{Value, json};

/// How long every message takes on the bus: fixed, so every time in a test is exact.
pub const DELAY: Duration = Duration::from_millis(5);

/// The text every session secret the stand-in issues starts with.
pub const SESSION_PREFIX: &str = "standin-session-";

/// The pool the stand-in admits to.
pub const POOL: &str = "burst";

/// How the stand-in answers one hello.
#[derive(Clone)]
pub enum HelloAnswer {
    /// A welcome.
    Welcome,
    /// No answer: the call is lost.
    Silent,
    /// A refusal with this status and code, and a `Retry-After` when given.
    Refuse {
        status: u16,
        code: &'static str,
        retry_after: Option<Duration>,
    },
    /// A welcome whose interval is zero, which the agent must refuse.
    Invalid,
    /// A welcome, and the same welcome again this much later.
    Twice(Duration),
}

/// How the stand-in answers one heartbeat.
#[derive(Clone)]
pub enum BeatAnswer {
    /// A 200 with this JSON reply.
    Reply(Value),
    /// A 200 with this body, sent as it is.
    Raw(&'static str),
    /// A 200 with this JSON reply, sent this much later than at once.
    Late(Duration, Value),
    /// No answer: the call is lost.
    Silent,
    /// A refusal with this status and code.
    Refuse { status: u16, code: &'static str },
}

/// What the stand-in does, by call number.
#[derive(Clone, Default)]
pub struct Script {
    /// The answer to the hello of each number, from 1.
    pub hellos: BTreeMap<u64, HelloAnswer>,
    /// The answer to the heartbeat of each number, from 1.
    pub beats: BTreeMap<u64, BeatAnswer>,
}

impl Script {
    /// A script that answers everything plainly.
    pub fn plain() -> Self {
        Self::default()
    }

    /// This script, answering hello `n` with `answer`.
    pub fn hello(mut self, n: u64, answer: HelloAnswer) -> Self {
        self.hellos.insert(n, answer);
        self
    }

    /// This script, answering heartbeat `n` with `answer`.
    pub fn beat(mut self, n: u64, answer: BeatAnswer) -> Self {
        self.beats.insert(n, answer);
        self
    }

    /// This script, answering heartbeat `n` with a reply carrying `commands`.
    pub fn commands(self, n: u64, commands: Value) -> Self {
        self.beat(n, BeatAnswer::Reply(json!({ "commands": commands })))
    }
}

/// A call the stand-in received.
pub enum Call {
    /// A hello.
    Hello(Hello),
    /// A heartbeat.
    Beat(Heartbeat),
}

/// One call, when it arrived, and the bearer it carried.
pub struct Received {
    pub at: FarmTime,
    pub bearer: String,
    pub call: Call,
}

/// The scripted server.
pub struct StandIn {
    script: Script,
    interval: Duration,
    epoch: u64,
    hellos: u64,
    beats: u64,
    pub received: Vec<Received>,
    /// The answers held back, by timer tag.
    held_back: BTreeMap<u64, (NodeId, Msg)>,
    next_tag: u64,
    out: Vec<Output<Msg>>,
}

impl StandIn {
    pub fn new(script: Script, interval: Duration) -> Self {
        Self {
            script,
            interval,
            epoch: 0,
            hellos: 0,
            beats: 0,
            received: Vec::new(),
            held_back: BTreeMap::new(),
            next_tag: 0,
            out: Vec::new(),
        }
    }

    /// When each hello arrived.
    pub fn hello_times(&self) -> Vec<u64> {
        let hellos = self.received.iter().filter_map(|r| match &r.call {
            Call::Hello(_) => Some(r.at.as_millis()),
            Call::Beat(_) => None,
        });
        hellos.collect()
    }

    /// Every hello, in order.
    pub fn hellos(&self) -> Vec<&Hello> {
        let hellos = self.received.iter().filter_map(|r| match &r.call {
            Call::Hello(hello) => Some(hello),
            Call::Beat(_) => None,
        });
        hellos.collect()
    }

    /// Every heartbeat, with when it arrived, in order.
    pub fn beats(&self) -> Vec<(u64, &Heartbeat)> {
        let beats = self.received.iter().filter_map(|r| match &r.call {
            Call::Beat(beat) => Some((r.at.as_millis(), beat)),
            Call::Hello(_) => None,
        });
        beats.collect()
    }

    /// When each heartbeat arrived.
    pub fn beat_times(&self) -> Vec<u64> {
        self.beats().into_iter().map(|(at, _)| at).collect()
    }

    /// The run ids of the results each heartbeat carried, in order.
    pub fn results(&self) -> Vec<Vec<String>> {
        let each = |(_, beat): (u64, &Heartbeat)| {
            let runs = beat.results.iter().map(|result| result.run_id.clone());
            runs.collect()
        };
        self.beats().into_iter().map(each).collect()
    }

    fn reply(&mut self, to: NodeId, id: CallId, status: u16, body: String) {
        self.answer(to, id, status, None, body);
    }

    fn answer(
        &mut self,
        to: NodeId,
        id: CallId,
        status: u16,
        retry_after: Option<Duration>,
        body: String,
    ) {
        let msg = Msg::Reply {
            id,
            status,
            retry_after,
            body,
        };
        self.out.push(Output::Send { to, msg });
    }

    fn later(&mut self, after: Duration, to: NodeId, msg: Msg) {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.held_back.insert(tag, (to, msg));
        self.out.push(Output::Timer { after, tag });
    }

    fn welcome(&mut self, interval_ms: u64) -> String {
        self.epoch += 1;
        let welcome = json!({
            "protocol": 1,
            "epoch": self.epoch,
            "heartbeat_interval_ms": interval_ms,
            "grace_ms": interval_ms * 3,
            "pool": POOL,
            "session": format!("{SESSION_PREFIX}{}", self.epoch),
            "host_credential": "standin-credential-the-agent-never-keeps",
        });
        welcome.to_string()
    }

    fn on_hello(&mut self, from: NodeId, id: CallId) {
        self.hellos += 1;
        let answer = self.script.hellos.get(&self.hellos).cloned();
        let interval = u64::try_from(self.interval.as_millis()).expect("interval");
        match answer.unwrap_or(HelloAnswer::Welcome) {
            HelloAnswer::Welcome => {
                let welcome = self.welcome(interval);
                self.reply(from, id, 200, welcome);
            }
            HelloAnswer::Silent => {}
            HelloAnswer::Refuse {
                status,
                code,
                retry_after,
            } => {
                let body = json!({ "error": code }).to_string();
                self.answer(from, id, status, retry_after, body);
            }
            HelloAnswer::Invalid => {
                let welcome = self.welcome(0);
                self.reply(from, id, 200, welcome);
            }
            HelloAnswer::Twice(after) => {
                let welcome = self.welcome(interval);
                self.reply(from.clone(), id, 200, welcome.clone());
                let again = Msg::Reply {
                    id,
                    status: 200,
                    retry_after: None,
                    body: welcome,
                };
                self.later(after, from, again);
            }
        }
    }

    fn on_beat(&mut self, from: NodeId, id: CallId) {
        self.beats += 1;
        let answer = self.script.beats.get(&self.beats).cloned();
        match answer.unwrap_or(BeatAnswer::Reply(json!({}))) {
            BeatAnswer::Reply(reply) => self.reply(from, id, 200, reply.to_string()),
            BeatAnswer::Raw(body) => self.reply(from, id, 200, body.to_owned()),
            BeatAnswer::Late(after, reply) => {
                let msg = Msg::Reply {
                    id,
                    status: 200,
                    retry_after: None,
                    body: reply.to_string(),
                };
                self.later(after, from, msg);
            }
            BeatAnswer::Silent => {}
            BeatAnswer::Refuse { status, code } => {
                let body = json!({ "error": code }).to_string();
                self.reply(from, id, status, body);
            }
        }
    }

    fn on_call(&mut self, now: FarmTime, from: NodeId, msg: Msg) {
        let Msg::Request {
            id,
            route,
            bearer,
            body,
        } = msg
        else {
            panic!("the stand-in was sent an answer");
        };
        let call = match route {
            Route::Hello => {
                let hello: Hello = serde_json::from_str(&body).expect("a hello body");
                hello
                    .validate()
                    .expect("the agent's hello passes validation");
                self.on_hello(from, id);
                Call::Hello(hello)
            }
            Route::Heartbeat => {
                let beat: Heartbeat = serde_json::from_str(&body).expect("a heartbeat body");
                beat.validate()
                    .expect("the agent's heartbeat passes validation");
                self.on_beat(from, id);
                Call::Beat(beat)
            }
        };
        self.received.push(Received {
            at: now,
            bearer,
            call,
        });
    }
}

/// The node kinds of a test cell: the agent under test and the stand-in.
pub enum Cell {
    Agent(Box<AgentNode>),
    Server(StandIn),
}

impl StateMachine for Cell {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            Cell::Agent(agent) => agent.apply(input),
            Cell::Server(server) => {
                match input.event {
                    Event::Message { from, msg } => server.on_call(input.now, from, msg),
                    Event::Timer { tag } => {
                        let (to, msg) = server.held_back.remove(&tag).expect("a held answer");
                        server.out.push(Output::Send { to, msg });
                    }
                    Event::Start => {}
                }
                Vec::new()
            }
        }
    }
}

impl Node for Cell {
    type Msg = Msg;

    fn trace_msg(msg: &Msg) -> String {
        AgentNode::trace_msg(msg)
    }

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Cell::Agent(agent) => agent.take_outputs(),
            Cell::Server(server) => std::mem::take(&mut server.out),
        }
    }
}

/// A cell of one agent, `h000` with `spec`, and the stand-in playing `script`, whose
/// welcome names `interval`; every message takes [`DELAY`]. Returns the cell and the
/// ledger the agent writes.
pub fn cell(spec: AgentSpec, script: Script, interval: Duration) -> (Sim<Cell>, Ledger) {
    let faults = Faults {
        min_delay: DELAY,
        max_delay: DELAY,
        ..Faults::default()
    };
    let mut sim = Sim::new(7, faults).with_trace_lines();
    let ledger = Ledger::new();
    sim.add_node(SERVER, Cell::Server(StandIn::new(script, interval)));
    let agent = AgentNode::new(spec, ledger.clone());
    sim.add_node(bus::agent_name(0), Cell::Agent(Box::new(agent)));
    (sim, ledger)
}

/// The agent of a cell.
pub fn agent(sim: &Sim<Cell>) -> &AgentNode {
    match sim.node(&bus::agent_name(0)) {
        Some(Cell::Agent(agent)) => agent,
        _ => panic!("h000 is the agent"),
    }
}

/// The agent of a cell, to kill or replace between steps.
pub fn agent_mut(sim: &mut Sim<Cell>) -> &mut AgentNode {
    let name = bus::agent_name(0);
    let found = sim.nodes_mut().find(|(id, _)| **id == name);
    match found {
        Some((_, Cell::Agent(agent))) => agent,
        _ => panic!("h000 is the agent"),
    }
}

/// The stand-in of a cell.
pub fn stand_in(sim: &Sim<Cell>) -> &StandIn {
    match sim.node(&NodeId::new(SERVER)) {
        Some(Cell::Server(server)) => server,
        _ => panic!("the server is the stand-in"),
    }
}
