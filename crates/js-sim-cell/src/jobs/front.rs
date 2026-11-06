//! The job front: one unit of work as a job, through the job service's HTTP/JSON gateway
//! (`/v1/jobs`), which a server of [`crate::ServerSpec::jobs`] serves in process.
//!
//! An item submits one job under an id of its own, `sim-job-<n>` ([`JOB_PREFIX`] and the
//! item's number), so a submit sent again is the same job (the service answers a repeat
//! with the job as it stands). The job runs [`SIM_RUN_IMAGE`] with the work's duration in
//! milliseconds as its one argument, asks for the work's cpu and memory (a work's GPUs are
//! not asked for: the server sends no start for a job that books GPUs, since it records no
//! device indices), or a whole host for [`Work::whole_host`], and has a wall limit of the
//! work's give-up time in whole seconds (at least one). The front reads the job at every
//! poll until it is terminal, then reads its result once. An item not terminal by its
//! give-up time is given up: the client reports [`HowEnded::GaveUp`] and cancels the job.
//!
//! # The mapping
//!
//! Every item is looked at in every poll, in item order, every [`POLL_EVERY`] while any
//! item is not done, and at once after a submit or a cancel. Within one poll an item goes
//! as far as its answers let it: a terminal job's result is read in the same poll.
//!
//! | call | answer | event, and what follows |
//! |---|---|---|
//! | `POST /v1/jobs` (SubmitJob) | 201 | none; the job is read from the next poll |
//! | | 422 (`no-capable-host`: no host has said hello yet), or 5xx | none; submitted again at the next poll |
//! | | anything else (400, 409 `job-id-conflict`, ...) | `Terminal(Failed)` |
//! | `GET /v1/jobs/{id}` (GetJob) | 200 with a `hostId` | `Placed { host }`, the first time only |
//! | | 200, `JOB_STATE_STARTING` or `JOB_STATE_RUNNING`, or a `times.startedAtMs` | `Started`, the first time only |
//! | | 200, `JOB_STATE_SUCCEEDED` | `Terminal(Succeeded)` |
//! | | 200, `JOB_STATE_FAILED` | `Terminal(Failed)` |
//! | | 200, `JOB_STATE_CANCELLED` | `Terminal(Cancelled)` |
//! | | 200, `JOB_STATE_LOST` | `Terminal(Lost)` |
//! | | 200, `JOB_STATE_FAILED_TO_START` | `Terminal(Cancelled)` when the client asked to cancel, else `Terminal(Failed)` |
//! | | 200, any other state | none; read again at the next poll |
//! | | 404 (`unknown-job`) | `Terminal(Lost)` |
//! | | anything else | none; read again at the next poll |
//! | `POST /v1/jobs/{id}:cancel` (CancelJob, sent once) | anything | none; the job is read as above |
//! | `GET /v1/jobs/{id}/result` (GetResult) | 200, 404 or 410 | `Released`: the client is done with the job |
//! | | anything else | none; read again at the next poll |
//!
//! Each event of a GetJob is heard in the order of the table, so `Placed` and `Started`
//! come before a terminal answer heard in the same read. A cancel of an item not yet
//! submitted (no 201 yet) ends it at once, `Terminal(Cancelled)`, with nothing to release.
//! A give-up of a submitted item sends the job's cancel at each poll until it is answered
//! 200 or 404, and then reports `Released`.
//!
//! # Answers
//!
//! Every answer is kept as it came ([`Answer`]), its [`CallKind`] read as the job's call:
//! [`CallKind::Queue`] for SubmitJob, [`CallKind::Read`] for GetJob and GetResult, and
//! [`CallKind::Cancel`] for CancelJob.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::Request;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::{Value, json};

use crate::SIM_RUN_IMAGE;
use crate::front::{Answer, CallKind, ClientPort, Front, FrontFactory, POLL_EVERY};
use crate::observe::{ClientEvent, HowEnded, ItemId};
use crate::server::CLIENT_TOKEN;
use crate::work::Work;

#[cfg(doc)]
use crate::ServerSpec;

/// What the id of an item's job starts with; the item's number follows.
pub const JOB_PREFIX: &str = "sim-job-";

/// Where an item is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Not yet accepted: submit it.
    Submitting,
    /// Accepted: read it until it is terminal.
    Reading,
    /// Terminal: read its result.
    Result,
    /// Given up: cancel it until the cancel is answered.
    Abandoning,
    /// Done with.
    Done,
}

/// One item the front holds.
struct Item {
    /// The job's id.
    id: String,
    /// The SubmitJob body.
    body: Value,
    gives_up_at: FarmTime,
    stage: Stage,
    /// The client asked to cancel the item.
    cancel: bool,
    /// The job's cancel was sent.
    cancel_sent: bool,
    /// `Placed` was reported.
    placed: bool,
    /// `Started` was reported.
    started: bool,
}

/// What one poll shares across items: the time, the server, the item being polled, and
/// the events heard and the answers given.
struct Poll<'a> {
    now: FarmTime,
    server: &'a mut dyn ClientPort,
    item: ItemId,
    heard: Vec<ClientEvent>,
    answers: Vec<Answer>,
}

impl Poll<'_> {
    /// One call for the item being polled, its answer kept: the status, and the body as
    /// JSON (`Null` when it is not JSON).
    fn call(
        &mut self,
        kind: CallKind,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            // A method and a path built from the item's own id always make a request;
            // were they not to, the empty request is refused and read as such.
            .unwrap_or_default();
        let answer = self.server.call(request);
        let status = answer.status().as_u16();
        let raw = answer.into_body();
        let json = serde_json::from_str(&raw).unwrap_or(Value::Null);
        self.answers.push(Answer {
            item: self.item,
            kind,
            at: self.now,
            status,
            body: raw,
        });
        (status, json)
    }
}

/// How a job in the terminal `state` ended, as the client hears it; `None` for a state
/// that is not terminal. `cancelled` is whether the client asked to cancel it.
fn ended(state: &str, cancelled: bool) -> Option<HowEnded> {
    match state {
        "JOB_STATE_SUCCEEDED" => Some(HowEnded::Succeeded),
        "JOB_STATE_FAILED" => Some(HowEnded::Failed),
        "JOB_STATE_CANCELLED" => Some(HowEnded::Cancelled),
        "JOB_STATE_LOST" => Some(HowEnded::Lost),
        "JOB_STATE_FAILED_TO_START" if cancelled => Some(HowEnded::Cancelled),
        "JOB_STATE_FAILED_TO_START" => Some(HowEnded::Failed),
        _ => None,
    }
}

impl Item {
    /// One poll of the item: its end when it is due, then each stage it reaches.
    fn poll(&mut self, poll: &mut Poll<'_>) {
        self.close(poll);
        if self.stage == Stage::Submitting {
            self.submit(poll);
        }
        if self.stage == Stage::Reading {
            self.read(poll);
        }
        if self.stage == Stage::Result {
            self.result(poll);
        }
        if self.stage == Stage::Abandoning {
            self.abandon(poll);
        }
    }

    /// Ends an item that is not terminal when the client cancelled it before its job was
    /// accepted, or when its give-up time has come.
    fn close(&mut self, poll: &mut Poll<'_>) {
        let open = matches!(self.stage, Stage::Submitting | Stage::Reading);
        if !open {
            return;
        }
        let submitted = self.stage == Stage::Reading;
        if self.cancel && !submitted {
            self.end(HowEnded::Cancelled, Stage::Done, poll);
        } else if poll.now >= self.gives_up_at {
            let next = if submitted {
                Stage::Abandoning
            } else {
                Stage::Done
            };
            self.end(HowEnded::GaveUp, next, poll);
        }
    }

    /// Reports the terminal `how`, and moves on to `next`.
    fn end(&mut self, how: HowEnded, next: Stage, poll: &mut Poll<'_>) {
        poll.heard.push(ClientEvent::Terminal { how });
        self.stage = next;
    }

    fn submit(&mut self, poll: &mut Poll<'_>) {
        let body = self.body.clone();
        let (status, _) = poll.call(CallKind::Queue, "POST", "/v1/jobs", Some(&body));
        match status {
            201 => self.stage = Stage::Reading,
            422 | 500..=599 => {}
            _ => self.end(HowEnded::Failed, Stage::Done, poll),
        }
    }

    /// Sends the job's cancel once, when asked for, and reads the job.
    fn read(&mut self, poll: &mut Poll<'_>) {
        if self.cancel && !self.cancel_sent {
            self.cancel_sent = true;
            let path = format!("/v1/jobs/{}:cancel", self.id);
            poll.call(CallKind::Cancel, "POST", &path, Some(&json!({})));
        }
        let path = format!("/v1/jobs/{}", self.id);
        let (status, answer) = poll.call(CallKind::Read, "GET", &path, None);
        let how = match status {
            200 => self.heard_job(&answer["job"], poll),
            404 => Some(HowEnded::Lost),
            _ => None,
        };
        if let Some(how) = how {
            self.end(how, Stage::Result, poll);
        }
    }

    /// What the job says: `Placed` the first time it names a host, `Started` the first time
    /// it shows the job started, and the terminal, when it is one.
    fn heard_job(&mut self, job: &Value, poll: &mut Poll<'_>) -> Option<HowEnded> {
        let host = job["hostId"]
            .as_str()
            .and_then(|host| HostId::new(host).ok());
        if let Some(host) = host.filter(|_| !self.placed) {
            self.placed = true;
            poll.heard.push(ClientEvent::Placed { host });
        }
        let state = job["state"].as_str().unwrap_or_default();
        let live = matches!(state, "JOB_STATE_STARTING" | "JOB_STATE_RUNNING");
        let ran = job["times"]["startedAtMs"].is_string();
        if (live || ran) && !self.started {
            self.started = true;
            poll.heard.push(ClientEvent::Started);
        }
        ended(state, self.cancel)
    }

    /// Reads the terminal job's result.
    fn result(&mut self, poll: &mut Poll<'_>) {
        let path = format!("/v1/jobs/{}/result", self.id);
        let (status, _) = poll.call(CallKind::Read, "GET", &path, None);
        if matches!(status, 200 | 404 | 410) {
            self.release(poll);
        }
    }

    /// Cancels a job the client gave up on.
    fn abandon(&mut self, poll: &mut Poll<'_>) {
        let path = format!("/v1/jobs/{}:cancel", self.id);
        let (status, _) = poll.call(CallKind::Cancel, "POST", &path, Some(&json!({})));
        if matches!(status, 200 | 404) {
            self.release(poll);
        }
    }

    /// The client is done with the job.
    fn release(&mut self, poll: &mut Poll<'_>) {
        poll.heard.push(ClientEvent::Released);
        self.stage = Stage::Done;
    }
}

/// The SubmitJob body of item `id` for `work`.
fn submit_body(id: &str, work: &Work) -> Value {
    let resources = work.resources();
    let needs = if work.wants_whole_host() {
        json!({ "wholeHost": true })
    } else {
        json!({
            "cpuMillis": resources.cpu_millis().to_string(),
            "memoryBytes": resources.memory_bytes().to_string(),
        })
    };
    let wall = work.gives_up_after().as_secs().max(1);
    json!({
        "spec": {
            "jobId": id,
            "image": SIM_RUN_IMAGE,
            "args": [work.runs_for().as_millis().to_string()],
            "needs": needs,
            "wallLimitSeconds": wall.to_string(),
        }
    })
}

/// The front of the job service: one job per unit of work.
#[derive(Default)]
pub struct JobFront {
    items: BTreeMap<ItemId, Item>,
    due: Option<FarmTime>,
    answers: Vec<Answer>,
}

impl JobFront {
    /// A front with no item.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The factory of job fronts, for [`crate::Scenario::front`]: its server must be one of
    /// [`ServerSpec::jobs`] ([`crate::Scenario::over_jobs`] sets both).
    #[must_use]
    pub fn factory() -> FrontFactory {
        FrontFactory::new(|_| Box::new(JobFront::new()))
    }

    /// Polls at `now`, or earlier if a poll is already due.
    fn due_at(&mut self, now: FarmTime) {
        self.due = Some(self.due.map_or(now, |due| due.min(now)));
    }
}

impl Front for JobFront {
    fn submit(&mut self, item: ItemId, work: &Work, now: FarmTime) {
        let id = format!("{JOB_PREFIX}{}", item.0);
        let held = Item {
            body: submit_body(&id, work),
            id,
            gives_up_at: work.ask(now).gives_up_at,
            stage: Stage::Submitting,
            cancel: false,
            cancel_sent: false,
            placed: false,
            started: false,
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
            item: ItemId(0),
            heard: Vec::new(),
            answers: Vec::new(),
        };
        let mut heard = Vec::new();
        let open = self.items.iter_mut();
        for (id, item) in open.filter(|(_, item)| item.stage != Stage::Done) {
            poll.item = *id;
            item.poll(&mut poll);
            heard.extend(poll.heard.drain(..).map(|event| (*id, event)));
        }
        self.answers.append(&mut poll.answers);
        let waiting = self.items.values().any(|item| item.stage != Stage::Done);
        self.due = waiting.then(|| now.saturating_add(POLL_EVERY));
        heard
    }

    fn answers(&self) -> &[Answer] {
        &self.answers
    }
}
