//! The lease front: one unit of work as `js run` does it today, on the lease routes and
//! the interim run routes.
//!
//! An item acquires a lease on the pool, waits until the lease is held, queues one run of
//! [`SIM_RUN_IMAGE`] whose first argument is the work's duration in milliseconds, reads
//! the run at every poll until it is terminal, and releases the lease. The lease asks for
//! the work's resources, with the pool's class and the server shape's capabilities, and
//! for a wall limit and a ttl of the work's give-up time in whole seconds (at least one),
//! so the lease outlives the item and is never renewed (the server cuts a ttl above its
//! `--max-ttl`, an hour by default). The run asks for the work's cpu and memory as its
//! limits and the GPU indices `0..gpus`. An item not terminal by its give-up time is
//! given up: the client reports [`HowEnded::GaveUp`] and releases what it holds.
//!
//! This front is transitional: the routes it uses are to be deleted, and a job front that
//! submits jobs takes its place beside it ([`super::FrontFactory`]).
//!
//! # The mapping
//!
//! Every item is looked at in every poll, in item order, every [`POLL_EVERY`] while any
//! item has not released its lease, and at once after a submit or a cancel. Within one
//! poll an item goes as far as its answers let it: a lease granted is given its run, and
//! a terminal answer is followed by the release.
//!
//! | call | answer | event, and what follows |
//! |---|---|---|
//! | `POST /v1/leases` | 201 | none; the lease is held: its run is queued now |
//! | | 202 (`pending`) | none; the lease list is read at the next poll |
//! | | 409 (no capacity, no fit, no capable host) | none; acquired again at the next poll, and no other item asking for the same lease is tried in this poll |
//! | | anything else | `Terminal(Failed)` |
//! | `GET /v1/leases` (pending) | 200, the lease `held` | none; its run is queued now |
//! | | 200, the lease `pending` | none; read again at the next poll |
//! | | 200, the lease gone or in another state | none; acquired again now |
//! | | anything else | none; read again at the next poll |
//! | `POST /v1/leases/{id}/runs` | 202 | `Queued { lease, run }`, then `Placed { host }` |
//! | | 409 | none; queued again at the next poll |
//! | | anything else | `Terminal(Failed)` |
//! | `GET /v1/leases/{id}/runs/{run}` | 200, `starting`, `running` or `exited`, or any state with an outcome | `Started`, the first time only |
//! | | 200, `exited` or `ended` with outcome `succeeded` | `Terminal(Succeeded)` |
//! | | 200, `exited` or `ended` with outcome `killed` | `Terminal(Cancelled)` |
//! | | 200, `exited` or `ended` with any other outcome | `Terminal(Failed)` |
//! | | 200, `ended` with no outcome | `Terminal(Cancelled)` when the client asked to cancel, else `Terminal(Lost)` |
//! | | 200, `never-started` | `Terminal(Cancelled)` when the client asked to cancel, else `Terminal(Failed)` |
//! | | 200, any other state | none; read again at the next poll |
//! | | 410 (`run-forgotten`) | `Terminal(Forgotten)` |
//! | | 404 (the lease or the run is unknown) | `Terminal(Lost)` |
//! | | anything else | none; read again at the next poll |
//! | `DELETE /v1/leases/{id}/runs/{run}` (a cancel, sent once) | 410 | `Terminal(Forgotten)` |
//! | | anything else | none; the run is read as above |
//! | `DELETE /v1/leases/{id}` | before the first send | `ReleaseSent` |
//! | | 200, 202 or 404 | `Released` |
//! | | anything else | none; released again at the next poll |
//!
//! A cancel of an item whose run is not queued yet ends it at once, `Terminal(Cancelled)`,
//! and releases any lease it learned; a cancel of a queued run sends the run's stop once
//! and waits for the run to end. The give-up time is checked at the start of each poll of
//! an item that has no terminal answer. Every lease the item learned is released whatever
//! its terminal, a pending one too.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::{Value, json};

use super::{ClientPort, Front, FrontFactory};
use crate::SIM_RUN_IMAGE;
use crate::agent::lease_id;
use crate::observe::{ClientEvent, HowEnded, ItemId, RunId};
use crate::server::{CLIENT_TOKEN, POOL_CLASS, ServerSpec};
use crate::work::Work;

/// How often the front polls while any item holds or waits for a lease: the server's
/// tick, at which it settles pending leases, as `js` polls a pending lease.
pub const POLL_EVERY: Duration = Duration::from_secs(5);

/// Where an item is.
enum Stage {
    /// No lease: acquire one.
    Asking,
    /// The acquire was answered 202: read the lease list until the lease is held.
    Pending(String),
    /// The lease is held: queue the run.
    Held { lease: String, host: String },
    /// The run is queued: read it until it is terminal.
    Running { lease: String, run: String },
    /// The item is terminal and holds a lease: release it.
    Releasing(String),
    /// Terminal, and nothing held.
    Done,
}

/// One item the front holds.
struct Item {
    work: Work,
    gives_up_at: FarmTime,
    stage: Stage,
    /// The client asked to cancel the item.
    cancel: bool,
    /// The run's stop was sent.
    stop_sent: bool,
    /// `Started` was reported.
    started: bool,
    /// `ReleaseSent` was reported.
    release_sent: bool,
}

/// What one poll shares across items: the time, the server, the lease requests refused
/// for want of capacity in this poll, and the events heard.
struct Poll<'a> {
    now: FarmTime,
    server: &'a mut dyn ClientPort,
    class: String,
    capabilities: Value,
    full: BTreeSet<String>,
    heard: Vec<ClientEvent>,
}

impl Poll<'_> {
    /// One call: the status, and the body as JSON (`Null` when it is not JSON).
    fn call(&mut self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            // A method and a path built from the server's own ids always make a request;
            // were they not to, the empty request is refused and read as such.
            .unwrap_or_default();
        let answer = self.server.call(request);
        let status = answer.status().as_u16();
        (
            status,
            serde_json::from_str(answer.body()).unwrap_or(Value::Null),
        )
    }
}

/// `value` as text, empty when it is not a string.
fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// How a run that ended with `outcome` ended, as the client hears it.
fn outcome(outcome: &str) -> HowEnded {
    match outcome {
        "succeeded" => HowEnded::Succeeded,
        "killed" => HowEnded::Cancelled,
        _ => HowEnded::Failed,
    }
}

impl Item {
    /// One poll of the item: its end when it is due, then each stage it reaches. The
    /// pending lease is read before an acquire, so a lease answered 202 is read at the
    /// next poll, and one found gone is asked for again at once.
    fn poll(&mut self, poll: &mut Poll<'_>) {
        self.close(poll);
        if let Stage::Pending(lease) = &self.stage {
            let lease = lease.clone();
            self.look(lease, poll);
        }
        if let Stage::Asking = self.stage {
            self.acquire(poll);
        }
        if let Stage::Held { lease, host } = &self.stage {
            let (lease, host) = (lease.clone(), host.clone());
            self.queue(lease, host, poll);
        }
        if let Stage::Running { lease, run } = &self.stage {
            let path = format!("/v1/leases/{lease}/runs/{run}");
            self.read(&path, poll);
        }
        if let Stage::Releasing(lease) = &self.stage {
            let path = format!("/v1/leases/{lease}");
            self.release(&path, poll);
        }
    }

    /// Whether the item has its terminal answer.
    fn terminal(&self) -> bool {
        matches!(self.stage, Stage::Releasing(_) | Stage::Done)
    }

    /// Ends an item that is not terminal when the client cancelled it before its run was
    /// queued, or when its give-up time has come.
    fn close(&mut self, poll: &mut Poll<'_>) {
        if self.terminal() {
            return;
        }
        let queued = matches!(self.stage, Stage::Running { .. });
        if self.cancel && !queued {
            self.end(HowEnded::Cancelled, poll);
        } else if poll.now >= self.gives_up_at {
            self.end(HowEnded::GaveUp, poll);
        }
    }

    /// Reports the terminal `how` and moves on to the release of the lease, if any.
    fn end(&mut self, how: HowEnded, poll: &mut Poll<'_>) {
        poll.heard.push(ClientEvent::Terminal { how });
        self.stage = match std::mem::replace(&mut self.stage, Stage::Done) {
            Stage::Pending(lease)
            | Stage::Held { lease, .. }
            | Stage::Running { lease, .. }
            | Stage::Releasing(lease) => Stage::Releasing(lease),
            Stage::Asking | Stage::Done => Stage::Done,
        };
    }

    fn acquire(&mut self, poll: &mut Poll<'_>) {
        let resources = self.work.resources();
        let wall = self.work.gives_up_after().as_secs().max(1);
        let body = json!({
            "class": poll.class,
            "resources": {
                "cpu_millis": resources.cpu_millis(),
                "memory_bytes": resources.memory_bytes(),
                "gpus": resources.gpus(),
                "gpu_class": resources.gpu_class(),
            },
            "capabilities": poll.capabilities,
            "max_wall_seconds": wall,
            "ttl_seconds": wall,
            "labels": { "team": "infra" },
        });
        let key = body.to_string();
        if poll.full.contains(&key) {
            return;
        }
        let (status, answer) = poll.call("POST", "/v1/leases", Some(&body));
        let lease = text(&answer["id"]);
        match status {
            201 => {
                let host = text(&answer["host"]);
                self.stage = Stage::Held { lease, host };
            }
            202 => self.stage = Stage::Pending(lease),
            409 => {
                poll.full.insert(key);
            }
            _ => self.end(HowEnded::Failed, poll),
        }
    }

    /// Reads the lease list for the pending lease `lease`.
    fn look(&mut self, lease: String, poll: &mut Poll<'_>) {
        let (status, answer) = poll.call("GET", "/v1/leases", None);
        if status != 200 {
            return;
        }
        let mut leases = answer["leases"].as_array().into_iter().flatten();
        let entry = leases.find(|entry| entry["id"] == lease.as_str());
        let state = entry.map(|entry| (text(&entry["state"]), text(&entry["host"])));
        let (state, host) = state.unwrap_or_default();
        match state.as_str() {
            "held" => self.stage = Stage::Held { lease, host },
            "pending" => {}
            _ => self.stage = Stage::Asking,
        }
    }

    /// Queues the run on the held lease `lease`, on `host`.
    fn queue(&mut self, lease: String, host: String, poll: &mut Poll<'_>) {
        let resources = self.work.resources();
        let gpus: Vec<u64> = (0..resources.gpus()).collect();
        let body = json!({
            "image": SIM_RUN_IMAGE,
            "args": [self.work.runs_for().as_millis().to_string()],
            "limits": {
                "cpu_millis": resources.cpu_millis(),
                "memory_bytes": resources.memory_bytes(),
            },
            "gpu_indices": gpus,
        });
        let path = format!("/v1/leases/{lease}/runs");
        let (status, answer) = poll.call("POST", &path, Some(&body));
        match status {
            202 => {
                let run = text(&answer["run_id"]);
                let queued = lease_id(&lease).map(|id| ClientEvent::Queued {
                    lease: id,
                    run: RunId::new(run.clone()),
                });
                poll.heard.extend(queued);
                let placed = HostId::new(host.as_str()).ok();
                poll.heard
                    .extend(placed.map(|host| ClientEvent::Placed { host }));
                self.stage = Stage::Running { lease, run };
            }
            409 => {}
            _ => self.end(HowEnded::Failed, poll),
        }
    }

    /// Sends the cancel of the queued run at `path` once, when asked for, and reads the
    /// run.
    fn read(&mut self, path: &str, poll: &mut Poll<'_>) {
        if self.cancel && !self.stop_sent {
            self.stop_sent = true;
            let (status, _) = poll.call("DELETE", path, None);
            if status == 410 {
                return self.end(HowEnded::Forgotten, poll);
            }
        }
        let (status, answer) = poll.call("GET", path, None);
        let how = match status {
            200 => self.heard_run(&answer, poll),
            410 => Some(HowEnded::Forgotten),
            404 => Some(HowEnded::Lost),
            _ => None,
        };
        if let Some(how) = how {
            self.end(how, poll);
        }
    }

    /// What the run's entry says: `Started` the first time it shows the run started, and
    /// the terminal, when it is one.
    fn heard_run(&mut self, entry: &Value, poll: &mut Poll<'_>) -> Option<HowEnded> {
        let state = text(&entry["state"]);
        let ended_with = entry["outcome"].as_str();
        let live = matches!(state.as_str(), "starting" | "running" | "exited");
        if (live || ended_with.is_some()) && !self.started {
            self.started = true;
            poll.heard.push(ClientEvent::Started);
        }
        match (state.as_str(), ended_with) {
            ("exited" | "ended", Some(ended_with)) => Some(outcome(ended_with)),
            ("ended", None) => Some(self.unless_cancelled(HowEnded::Lost)),
            ("never-started", _) => Some(self.unless_cancelled(HowEnded::Failed)),
            _ => None,
        }
    }

    /// `how`, or `Cancelled` when the client asked to cancel the item.
    fn unless_cancelled(&self, how: HowEnded) -> HowEnded {
        if self.cancel {
            HowEnded::Cancelled
        } else {
            how
        }
    }

    /// Releases the lease at `path`.
    fn release(&mut self, path: &str, poll: &mut Poll<'_>) {
        if !self.release_sent {
            self.release_sent = true;
            poll.heard.push(ClientEvent::ReleaseSent);
        }
        let (status, _) = poll.call("DELETE", path, None);
        if matches!(status, 200 | 202 | 404) {
            poll.heard.push(ClientEvent::Released);
            self.stage = Stage::Done;
        }
    }
}

/// The front of `js run`: a lease, one run on it, and the release.
pub struct LeaseFront {
    class: String,
    capabilities: Value,
    items: BTreeMap<ItemId, Item>,
    due: Option<FarmTime>,
}

impl LeaseFront {
    /// A front for the pool `spec` describes: its leases name the pool's class and the
    /// capabilities of its shape.
    #[must_use]
    pub fn new(spec: &ServerSpec) -> Self {
        Self {
            class: POOL_CLASS.to_owned(),
            capabilities: json!(spec.shape().capabilities),
            items: BTreeMap::new(),
            due: None,
        }
    }

    /// The factory of lease fronts: the scenario builder's default.
    #[must_use]
    pub fn factory() -> FrontFactory {
        FrontFactory::new(|spec| Box::new(LeaseFront::new(spec)))
    }

    /// Polls at `now`, or earlier if a poll is already due.
    fn due_at(&mut self, now: FarmTime) {
        self.due = Some(self.due.map_or(now, |due| due.min(now)));
    }
}

impl Front for LeaseFront {
    fn submit(&mut self, item: ItemId, work: &Work, now: FarmTime) {
        let held = Item {
            work: work.clone(),
            gives_up_at: work.ask(now).gives_up_at,
            stage: Stage::Asking,
            cancel: false,
            stop_sent: false,
            started: false,
            release_sent: false,
        };
        self.items.insert(item, held);
        self.due_at(now);
    }

    fn cancel(&mut self, item: ItemId, now: FarmTime) {
        if let Some(held) = self.items.get_mut(&item) {
            held.cancel = true;
            self.due_at(now);
        }
    }

    fn next_poll(&self) -> Option<FarmTime> {
        self.due
    }

    fn poll(&mut self, now: FarmTime, server: &mut dyn ClientPort) -> Vec<(ItemId, ClientEvent)> {
        let mut poll = Poll {
            now,
            server,
            class: self.class.clone(),
            capabilities: self.capabilities.clone(),
            full: BTreeSet::new(),
            heard: Vec::new(),
        };
        let mut heard = Vec::new();
        let open = self.items.iter_mut();
        for (id, item) in open.filter(|(_, item)| !matches!(item.stage, Stage::Done)) {
            item.poll(&mut poll);
            heard.extend(poll.heard.drain(..).map(|event| (*id, event)));
        }
        let waiting = self
            .items
            .values()
            .any(|item| !matches!(item.stage, Stage::Done));
        self.due = waiting.then(|| now.saturating_add(POLL_EVERY));
        heard
    }
}
