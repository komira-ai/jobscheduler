//! The cell's server: the real lease server in process ([`InProcess`]), its registry, lease
//! table, scaler and placement, as one kernel node.
//!
//! [`ServerNode`] owns the server, a current-thread tokio runtime that runs each request
//! and tick to its end, and the [`ManualClock`] behind the server's [`SharedClock`]. The
//! kernel is the only clock: before every input the node sets that clock to the input's
//! virtual time, counted from [`EPOCH_MS`]. A [`Msg::Request`] is answered by the agent
//! router, run to completion with `block_on`, and its answer queued as a [`Msg::Reply`]
//! to the sender. A kernel timer calls [`InProcess::tick`] every tick of virtual time
//! (`--tick`, 5 s), the first one tick after the node starts.
//!
//! The server runs some of its work on tokio's blocking pool (`spawn_blocking`), on
//! another thread; the node awaits each request and each tick to its end before it takes
//! its next input, so that thread's timing cannot reach a decision.
//!
//! [`ServerNode::client_call`] is how a client reaches the server: a call at an exact
//! virtual time, through the client router, which the bus never drops.
//!
//! The lease file, the pool file and the token files live in a [`TestDir`] the node owns.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{Method, Request, Response};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::in_process::InProcess;
use js_lease_server::{SharedClock, StartupError};
use js_registry::{HostState, HostView, Registry};
use js_sim::{Event, NodeId, NodeInput, Output};
use js_testkit::dir::TestDir;
use js_types::sched::{Effect, FarmTime, StateMachine};
use tokio::runtime::Runtime;
use tower::ServiceExt;

use crate::bus::{Msg, Route};
use crate::observe::{ServerHost, ServerView};

mod spec;

pub use spec::{CLIENT_TOKEN, JOIN_TOKEN, ServerSpec, Shape};

/// The wall time, in milliseconds since the Unix epoch, that virtual time zero stands
/// for: 2027-01-01 00:00 UTC. Every clock of the cell counts from it.
pub const EPOCH_MS: u64 = 1_798_761_600_000;

/// The wall time virtual time `at` stands for.
#[must_use]
pub fn wall_time(at: FarmTime) -> Timestamp {
    Timestamp::from_millis(EPOCH_MS.saturating_add(at.as_millis()))
}

/// A kind of call the server answered: who called, on which route, and the status.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CallKey {
    from: NodeId,
    route: Route,
    status: u16,
}

/// The real lease server as a kernel node.
pub struct ServerNode {
    agent: Router,
    client: Router,
    server: InProcess,
    runtime: Runtime,
    clock: ManualClock,
    tick: Duration,
    out: Vec<Output<Msg>>,
    calls: BTreeMap<CallKey, u64>,
    /// Dropped last: the server's files.
    _dir: TestDir,
}

impl ServerNode {
    /// Starts the server `spec` describes, its files in `dir`, at virtual time zero.
    ///
    /// # Errors
    ///
    /// When a file cannot be written, the runtime cannot be built, or the server refuses
    /// its configuration, naming the flag at fault.
    pub fn start(spec: &ServerSpec, dir: TestDir) -> Result<Self, StartupError> {
        let clock = ManualClock::new(wall_time(FarmTime::default()));
        let config = spec.config(dir.path(), &clock)?;
        let tick = config.tick_period();
        let mut build = tokio::runtime::Builder::new_current_thread();
        let runtime = build.build().map_err(StartupError::because("runtime"))?;
        let server = {
            let _entered = runtime.enter();
            InProcess::start(config, SharedClock::new(clock.clone()))?
        };
        Ok(Self {
            agent: server.agent_router(),
            client: server.client_router(),
            server,
            runtime,
            clock,
            tick,
            out: Vec::new(),
            calls: BTreeMap::new(),
            _dir: dir,
        })
    }

    /// One call through the client router at virtual time `now`, run to its end. The
    /// answer's body is read as text.
    pub fn client_call(&mut self, now: FarmTime, request: Request<Body>) -> Response<String> {
        self.clock.set(wall_time(now));
        self.runtime.block_on(call(self.client.clone(), request))
    }

    /// The server: its registry, scaler and counts.
    #[must_use]
    pub fn server(&self) -> &InProcess {
        &self.server
    }

    /// The agent registry; `None` for a server without registration, which a cell's
    /// server never is.
    #[must_use]
    pub fn registry(&self) -> Option<Registry> {
        self.server.registry()
    }

    /// Every host the registry lists, in id order.
    #[must_use]
    pub fn hosts(&self) -> Vec<HostView> {
        self.registry()
            .map(|registry| registry.hosts())
            .unwrap_or_default()
    }

    /// The registry's listing as the cell observes it.
    #[must_use]
    pub fn view(&self) -> ServerView {
        let hosts = self.hosts().into_iter().map(|host| {
            let listed = ServerHost {
                lost: host.state == HostState::Lost,
                leases: host.leases,
                set_aside: host.mark.is_some(),
            };
            (host.id, listed)
        });
        ServerView {
            hosts: hosts.collect(),
        }
    }

    /// How many calls from `from` on `route` were answered `status`.
    #[must_use]
    pub fn answered(&self, from: &NodeId, route: Route, status: u16) -> u64 {
        let key = CallKey {
            from: from.clone(),
            route,
            status,
        };
        self.calls.get(&key).copied().unwrap_or(0)
    }

    /// Answers an agent's call; anything but a call is ignored.
    fn on_message(&mut self, from: NodeId, msg: Msg) {
        let Msg::Request {
            id,
            route,
            bearer,
            body,
        } = msg
        else {
            return;
        };
        let request = Request::builder()
            .method(Method::POST)
            .uri(route.path())
            .header(AUTHORIZATION, format!("Bearer {bearer}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_default();
        let answer = self.runtime.block_on(call(self.agent.clone(), request));
        let status = answer.status().as_u16();
        let header = answer.headers().get(RETRY_AFTER);
        let seconds = header.and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
        let retry_after = seconds.map(Duration::from_secs);
        let key = CallKey {
            from: from.clone(),
            route,
            status,
        };
        *self.calls.entry(key).or_insert(0) += 1;
        let msg = Msg::Reply {
            id,
            status,
            retry_after,
            body: answer.into_body(),
        };
        self.out.push(Output::Send { to: from, msg });
    }

    /// Arms the next tick.
    fn arm(&mut self) {
        let after = self.tick;
        self.out.push(Output::Timer { after, tag: 0 });
    }
}

impl StateMachine for ServerNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        self.clock.set(wall_time(input.now));
        match input.event {
            Event::Start => self.arm(),
            Event::Timer { .. } => {
                self.runtime.block_on(self.server.tick());
                self.arm();
            }
            Event::Message { from, msg } => self.on_message(from, msg),
        }
        Vec::new()
    }
}

impl ServerNode {
    /// Takes the outputs queued by earlier inputs, oldest first.
    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }
}

/// `request` answered by `router`, called as a service, with the body read as text.
async fn call(router: Router, request: Request<Body>) -> Response<String> {
    let Ok(response) = router.oneshot(request).await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    Response::from_parts(parts, String::from_utf8_lossy(&bytes).into_owned())
}
