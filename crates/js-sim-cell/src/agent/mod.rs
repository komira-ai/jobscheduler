//! The cell's agent: the conformance suite's in-process agent ([`FakeAgent`]) behind the
//! real wire of `js-agent-wire`, as a [`js_sim::Node`] the kernel can run by the hundred.
//!
//! [`AgentNode`] builds the hello and heartbeat bodies a real agent builds, checks every
//! answer with the wire's own `validate()` before it acts on any field, and turns every
//! command into the [`FakeAgent`] call that models it. The check is by part, as the wire's
//! `ReplyCheck` says. A body that does not decode, and a welcome that fails its check, are
//! ignored whole. A heartbeat reply's refused commands are skipped, and so are its
//! acknowledgements when they fail their check. The valid commands beside them still
//! apply. Each such answer counts once in [`Counter::ReplyInvalid`].
//!
//! | what arrives | the call |
//! |---|---|
//! | `Start` in a reply | [`AgentHarness::start`]`(run, valid_for_ms, waited)` |
//! | `Stop` or `Cancel` in a reply | [`AgentHarness::cancel`]`(run)` |
//! | an acknowledgement of a result | [`AgentHarness::ack`]`(run)` |
//! | the run's own time ran out | [`AgentHarness::complete`]`(run)` |
//! | a heartbeat is sent | [`AgentHarness::heartbeat`] |
//! | a heartbeat had no answer when the next one was due | [`AgentHarness::disconnect`] |
//! | the next answer to any call | [`AgentHarness::reconnect`] |
//! | a heartbeat is answered 200 | [`AgentHarness::contact`] |
//!
//! The [`FakeAgent`] is keyed by the run id: the wire's start, stop, cancel, result and
//! acknowledgement all name the run, so the run id is what the fake calls a lease.
//!
//! It is built with [`FakeAgent::with_window`]`(None)`: the wire carries no self-fence
//! and the real agent has none either (design section 8, row 5). A broken one
//! ([`AgentSpec::defect`]) is [`FakeAgent::broken`]; the node never advances the fake's
//! clock, so its fence window never acts.
//!
//! # The session
//!
//! A process says hello at once and then every [`AgentSpec::hello_interval`] until it is
//! welcomed. A 429 answer is followed by a hello after the `Retry-After` it names (at most
//! [`RETRY_AFTER_CAP`]); any other refusal by the next one an interval later. Once
//! welcomed it sends a heartbeat every interval the welcome names, whether or not the
//! previous one was answered, and acts on an answer whenever it comes. A 409 answer to a
//! heartbeat (`stale-epoch`, `unknown-host`, `host-lost`) or a 403 `session-mismatch`
//! ends the session: the agent says hello again at once when a heartbeat of the session
//! was answered 200, and an interval later otherwise, so a server that welcomes and at
//! once refuses is not asked in a loop (design section 5.1).
//!
//! Every call carries the join token of [`AgentSpec::join_token`] as its bearer. The hello
//! does not ask for a host credential (`accepts_host_credential` is false), so the agent
//! keeps none and a credential in a welcome is ignored.
//!
//! A start names the heartbeat the server had taken when it queued it
//! ([`js_agent_wire::JobSpec::heartbeat_seq`]); the agent numbers its heartbeats of a
//! session from 1 and keeps the time it sent each, with the hello that opened the session
//! as number 0, and `waited` is the time since that send (a number it never sent counts
//! from the hello). The server numbers the heartbeats it took, so the two numberings agree
//! while no heartbeat is lost on the way; after a lost one the agent's number names a later
//! send and `waited` reads short.
//!
//! A hello is sent every interval while none is welcomed, without waiting for the answer to
//! the last. The first welcome closes every open hello: when answers are delayed past an
//! interval, a welcome to a later hello is then ignored, the agent keeps the earlier
//! session, and the server's refusal of its next heartbeat makes it say hello again. The
//! real agent has one call in flight at a time and never meets that case.
//!
//! # Runs
//!
//! A start of the image [`SIM_RUN_IMAGE`] whose first argument is a number of
//! milliseconds ends by itself that long after its launch: the node arms a timer and calls
//! [`AgentHarness::complete`], and the run's result rides in every heartbeat until an
//! answer acknowledges it. Any other start runs until it is stopped. A stopped or
//! cancelled run sends no result. The resources a run holds are its start's `limits` (and
//! one GPU per index it names).
//!
//! # Processes
//!
//! The node is a host. Its agent process holds the [`FakeAgent`], its session and its
//! timers; [`AgentNode::kill`] discards the process and the node then drops every input,
//! and [`AgentNode::replace`] starts a new process with empty state, which says hello
//! again at the node's next tick (the node ticks every [`AgentSpec::hello_interval`]
//! while it has no process). The cell's [`Ledger`] and the node's [`Counters`] are kept
//! outside the process, so neither is reset by a replacement.
//!
//! The node reads no clock and no random source: time arrives in each
//! [`js_sim::NodeInput`].

use std::collections::BTreeMap;
use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::{Event, Node, NodeId, NodeInput, Output};
use js_types::sched::{Effect, FarmTime, StateMachine};
use js_types::{HostId, Resources};

use crate::bus::{self, CallId, Msg, Route};
use crate::counters::{Counter, Counters};
use crate::ledger::Ledger;
use crate::node::Applied;
use crate::observe::AgentTruth;

#[cfg(doc)]
use js_agent_wire::RETRY_AFTER_CAP;
#[cfg(doc)]
use js_conformance::agent::{AgentHarness, FakeAgent};

mod process;

pub(crate) use process::lease_id;
use process::{Answer, Process};

/// The image of a run that ends by itself: its first argument is how long it runs, in
/// milliseconds. Pinned by a fixed digest, as a start must be.
pub const SIM_RUN_IMAGE: &str =
    "sim-run@sha256:5151515151515151515151515151515151515151515151515151515151515151";

/// What an agent node is: its host and what the host offers, the pool it joins with the
/// token it presents, and whether its agent lacks a behaviour.
///
/// It has no `Debug`: it holds the join token.
#[derive(Clone)]
pub struct AgentSpec {
    /// The host's id, sent in every hello and heartbeat.
    pub host: HostId,
    /// The pool the hello asks to join.
    pub pool: String,
    /// The join token: the bearer of every call.
    pub join_token: String,
    /// What the host offers: the hello's shape, and the capacity of its ground truth.
    pub capacity: Resources,
    /// The hello's capabilities.
    pub capabilities: BTreeMap<String, String>,
    /// The hello's labels.
    pub labels: BTreeMap<String, String>,
    /// How long the agent waits between two hellos that were not welcomed, and between
    /// two ticks of a host with no process.
    pub hello_interval: Duration,
    /// The one behaviour the agent lacks, or `None` for a correct agent.
    pub defect: Option<Defect>,
}

/// One host of the cell with its agent process.
pub struct AgentNode {
    spec: AgentSpec,
    process: Option<Process>,
    io: Io,
    /// Where the node writes that it took an input, for the cell's run loop.
    applied: Applied,
}

impl AgentNode {
    /// A host with a fresh agent process, which says hello when the kernel starts the
    /// node, writing what it launches and completes into `ledger`.
    #[must_use]
    pub fn new(spec: AgentSpec, ledger: Ledger) -> Self {
        let io = Io {
            now: FarmTime::default(),
            host: spec.host.clone(),
            out: Vec::new(),
            next_call: 0,
            next_tag: 0,
            tick: None,
            counters: Counters::default(),
            ledger,
        };
        Self {
            process: Some(Process::new(&spec)),
            spec,
            io,
            applied: Applied::default(),
        }
    }

    /// The host.
    #[must_use]
    pub fn host(&self) -> &HostId {
        &self.spec.host
    }

    /// Makes the node write into `applied` each time it takes an input.
    pub(crate) fn report_to(&mut self, applied: Applied) {
        self.applied = applied;
    }

    /// Kills the agent process: everything it held is lost, and the node drops every
    /// input until [`AgentNode::replace`].
    pub fn kill(&mut self) {
        self.process = None;
    }

    /// Starts a new agent process on this host with empty state, in place of any process
    /// it has. It says hello at the node's next tick.
    pub fn replace(&mut self) {
        self.process = Some(Process::new(&self.spec));
    }

    /// The ground truth of the agent process, or `None` while the host has none.
    #[must_use]
    pub fn truth(&self) -> Option<AgentTruth> {
        self.process
            .as_ref()
            .map(|process| process.truth(&self.spec.capacity))
    }

    /// What the node has counted, over every process of the host.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.io.counters
    }

    /// The node's tick: the process says hello or sends a heartbeat, and the next tick is
    /// armed when the process asks (a host with no process ticks every hello interval).
    fn on_tick(&mut self) {
        let next = match self.process.as_mut() {
            Some(process) => process.tick(&self.spec, &mut self.io),
            None => self.spec.hello_interval,
        };
        self.io.arm_tick(next);
    }

    fn on_timer(&mut self, tag: u64) {
        if self.io.tick == Some(tag) {
            self.on_tick();
        } else if let Some(process) = self.process.as_mut() {
            process.timer(tag, &mut self.io);
        }
    }

    fn on_message(&mut self, msg: Msg) {
        let process = self.process.as_mut();
        if let (
            Some(process),
            Msg::Reply {
                id,
                status,
                retry_after,
                body,
            },
        ) = (process, msg)
        {
            let answer = Answer {
                id,
                status,
                retry_after,
                body,
            };
            process.answer(answer, &self.spec, &mut self.io);
        }
    }
}

impl StateMachine for AgentNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        self.applied.agent(&self.spec.host);
        self.io.now = input.now;
        match input.event {
            Event::Start => self.on_tick(),
            Event::Timer { tag } => self.on_timer(tag),
            Event::Message { msg, .. } => self.on_message(msg),
        }
        Vec::new()
    }
}

impl Node for AgentNode {
    type Msg = Msg;

    fn trace_msg(msg: &Msg) -> String {
        bus::trace_msg(msg)
    }

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.io.out)
    }
}

/// What a process uses of its node and leaves behind when it dies: the clock of the input
/// being applied, the outputs, the numbering of calls and timers, the counters and the
/// ledger.
struct Io {
    now: FarmTime,
    host: HostId,
    out: Vec<Output<Msg>>,
    next_call: u64,
    next_tag: u64,
    /// The tag of the one live tick; a tick armed earlier and replaced is ignored when it
    /// fires.
    tick: Option<u64>,
    counters: Counters,
    ledger: Ledger,
}

impl Io {
    /// Sends a call on `route` to the server, and returns its number.
    fn call(&mut self, route: Route, bearer: &str, body: String) -> CallId {
        let id = CallId(self.next_call);
        self.next_call += 1;
        let msg = Msg::Request {
            id,
            route,
            bearer: bearer.to_owned(),
            body,
        };
        self.out.push(Output::Send {
            to: NodeId::new(bus::SERVER),
            msg,
        });
        id
    }

    /// Arms a timer `after` from now, and returns its tag.
    fn timer(&mut self, after: Duration) -> u64 {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.out.push(Output::Timer { after, tag });
        tag
    }

    /// Makes the next tick `after` from now, in place of the one armed.
    fn arm_tick(&mut self, after: Duration) {
        self.tick = Some(self.timer(after));
    }

    fn bump(&mut self, counter: Counter) {
        self.counters.bump(counter);
    }
}
