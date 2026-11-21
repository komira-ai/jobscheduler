//! The rig of the update-under-load tests (`live_update_load.rs`): servers over one Postgres
//! store, fake per-job agents that send every request to whichever server the edge picks, the
//! edge itself (a round robin over the servers that take new connections), and the two
//! barriers that stop a server inside a transaction: [`Gate`], an id source that parks a
//! placement's mint of its allocation id while the placement holds its claimed row, and
//! [`HeadLock`], the store's commit head held from a connection of the test's own, so the next
//! claim of a dispatch message waits inside its transaction. Each barrier is met by an event
//! (the mint parked; Postgres listing a waiter on the head), never by time.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;

use js_jobs::{Booked, IdSource, JobEnd, JobId, JobState, JobStore, MintError, NewJob, RandomIds};
use js_state::{Clock, ManualClock, Store, StoreTime};
use js_types::HostId;
use postgres::{Client, NoTls};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, job_hello, job_host_args};
use crate::jobs_support::{JobBody, commands, exited, running};
use crate::store_support::store_file;
use crate::support::{Harness, Reply, Scratch, TOKEN, guard, request};

/// The pool the hosts join: room for [`HOSTS`] machines of 8 cores, so each takes two jobs.
const POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 4

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// How many hosts the fleet has.
pub const HOSTS: usize = 4;

/// How many jobs a host runs at once: 3 of its 8 cores each.
pub const PER_HOST: usize = 2;

/// A server named `name` over the Postgres store at `url`, minting its ids from `ids`.
pub async fn server_minting(url: &str, name: &str, ids: Arc<dyn IdSource>) -> Harness {
    let scratch = Scratch::new(name);
    let mut args: Vec<OsString> = job_host_args(&scratch);
    scratch.write("agent-pool.toml", POOL, 0o644);
    args.push("--store".into());
    args.push(store_file(&scratch, url).into_os_string());
    Harness::with_ids(scratch, args, ids).await
}

/// A server named `name` over the Postgres store at `url`, with a readiness route on a port
/// the system chooses, whose stores read `store_clock`: each call a store makes reads it first,
/// before it takes a connection or a lock.
pub async fn server_reading(url: &str, name: &str, store_clock: Arc<dyn Clock>) -> Harness {
    let scratch = Scratch::new(name);
    let mut args: Vec<OsString> = job_host_args(&scratch);
    scratch.write("agent-pool.toml", POOL, 0o644);
    args.push("--store".into());
    args.push(store_file(&scratch, url).into_os_string());
    args.extend(["--ready-listen", "127.0.0.1:0"].map(OsString::from));
    Harness::with_store_clock(scratch, args, store_clock).await
}

/// A server named `name` over the Postgres store at `url`.
pub async fn server(url: &str, name: &str) -> Harness {
    server_minting(url, name, Arc::new(RandomIds)).await
}

/// Sends a server the termination signal: its drain begins (the agent listener closes,
/// `/readyz` turns 503, it places and claims nothing more) and the hold is not waited out.
/// Returns once the agent listener refuses a connection, so no hello, beat or poll reaches
/// the server from then on: what an edge sees, and retries on the other server.
pub async fn signal(server: &Harness) {
    let hold = server.running.hold();
    tokio::pin!(hold);
    tokio::select! {
        biased;
        () = &mut hold => panic!("the hold is over at once"),
        () = std::future::ready(()) => {}
    }
    let agents = server.addr();
    guard(async {
        // The listener closes on its own task: a connect made before that is let go.
        while tokio::net::TcpStream::connect(agents).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// The job `id` as the job service at `job` reports it.
pub async fn job_at(job: SocketAddr, id: &str) -> Value {
    let read = request(job, "GET", &format!("/v1/jobs/{id}"), Some(TOKEN), None).await;
    assert_eq!(read.status, 200, "{id}: {}", read.body);
    read.json()["job"].clone()
}

/// The state (`JOB_STATE_...`) of the job `id` at the job service `job`.
pub async fn state_at(job: SocketAddr, id: &str) -> String {
    job_at(job, id).await["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Submits the job `id` (3 of a host's 8 cores) to the job service at `job`.
pub async fn submit_at(job: SocketAddr, id: String) -> Reply {
    let body = JobBody::new(&id).needs(3000, 1 << 30).json().to_string();
    request(job, "POST", "/v1/jobs", Some(TOKEN), Some(body.as_bytes())).await
}

/// What the edge in front of the servers does: it sends each new connection to the next
/// server that takes new connections, in turn. A server leaves it at its termination signal
/// (its agent listener closes, so a connection to it would be refused and tried on the
/// next) and joins it when it starts.
#[derive(Default)]
pub struct Edge {
    servers: Vec<SocketAddr>,
    next: usize,
}

impl Edge {
    /// A server that started.
    pub fn add(&mut self, server: &Harness) {
        self.servers.push(server.addr());
    }

    /// A server that was signalled.
    pub fn remove(&mut self, server: &Harness) {
        let addr = server.addr();
        self.servers.retain(|served| *served != addr);
    }

    /// The server the next connection reaches.
    pub fn pick(&mut self) -> SocketAddr {
        assert!(!self.servers.is_empty(), "the edge has a server");
        let addr = self.servers[self.next % self.servers.len()];
        self.next = self.next.wrapping_add(1);
        addr
    }
}

/// A fake `js-agent` that runs per-job containers and counts what it is told. A polling
/// agent (one that reports `dispatch-poll`) takes its starts from `POST /v1/agent/dispatch`
/// and acknowledges them in its next poll; the other takes them from its heartbeat's reply,
/// as an agent of a release before the queue does. Every request it sends must be answered
/// 200: a `409 unknown-host`, a `403 session-mismatch` or a `409 host-live` from a server
/// that did not take its hello fails the test, as does any stop it is told.
pub struct Agent {
    pub host: String,
    polls: bool,
    session: Option<Session>,
    pub hellos: u32,
    /// Every allocation a start named, by job, from either path.
    pub starts: BTreeMap<String, BTreeSet<String>>,
    /// How many times each start (job, allocation) reached it, from either path.
    pub deliveries: BTreeMap<(String, String), u32>,
    /// The jobs it runs, with the allocation each was started under.
    running: BTreeMap<String, String>,
    /// The jobs it ended whose result is not acknowledged yet.
    ended: BTreeMap<String, String>,
    /// The jobs whose result a server acknowledged.
    pub done: BTreeSet<String>,
    /// The acknowledgements of dispatch messages its next poll carries.
    acks: Vec<Value>,
}

impl Agent {
    pub fn new(host: &str, polls: bool) -> Self {
        Self {
            host: host.to_owned(),
            polls,
            session: None,
            hellos: 0,
            starts: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            running: BTreeMap::new(),
            ended: BTreeMap::new(),
            done: BTreeSet::new(),
            acks: Vec::new(),
        }
    }

    pub fn polls(&self) -> bool {
        self.polls
    }

    /// Whether it has said hello.
    pub fn joined(&self) -> bool {
        self.session.is_some()
    }

    /// How many jobs it runs or ended without the end acknowledged.
    pub fn holds(&self) -> usize {
        self.running.len() + self.ended.len()
    }

    /// Says hello to the server at `server`, which must admit it, and must tell a polling
    /// agent to poll dispatch.
    pub async fn hello(&mut self, server: SocketAddr) {
        let mut hello = job_hello(&self.host);
        if self.polls {
            hello["capabilities"]["dispatch-poll"] = json!("true");
        }
        let reply = post_at(server, "/v1/agent/hello", &hello).await;
        assert_eq!(reply.status, 200, "{}: {}", self.host, reply.body);
        let welcome = reply.json();
        assert_eq!(
            welcome["dispatch_poll"].as_bool().unwrap_or(false),
            self.polls,
            "{}: {welcome}",
            self.host
        );
        self.session = Some(Session {
            host: self.host.clone(),
            epoch: welcome["epoch"].as_u64().unwrap(),
            secret: welcome["session"].as_str().unwrap().to_owned(),
        });
        self.hellos += 1;
    }

    fn session(&self) -> &Session {
        self.session.as_ref().expect("the agent said hello")
    }

    /// One heartbeat to `server` with its whole listing. A start in the reply is run (once);
    /// an acknowledged result is let go of.
    pub async fn beat(&mut self, server: SocketAddr) {
        let mut body = self.session().beat();
        let runs = self.running.keys().map(|id| running(id));
        let ends = self.ended.iter().map(|(id, alloc)| exited(id, alloc));
        body["jobs"] = json!(runs.chain(ends).collect::<Vec<_>>());
        let reply = post_at(server, "/v1/agent/heartbeat", &body).await;
        assert_eq!(reply.status, 200, "{}: {}", self.host, reply.body);
        let reply = reply.json();
        let (starts, stops) = commands(&reply);
        assert_eq!(stops, Vec::<String>::new(), "{}: {reply}", self.host);
        for (job, allocation) in starts {
            self.take(job, allocation);
        }
        for ack in reply["job_acks"].as_array().into_iter().flatten() {
            assert_eq!(ack["ack"], "ACCEPTED", "{}: {reply}", self.host);
            let job = ack["job_id"].as_str().unwrap();
            if self.ended.remove(job).is_some() {
                self.done.insert(job.to_owned());
            }
        }
    }

    /// The body of its next dispatch poll: its session, and the acknowledgements it owes
    /// when `acks` (a poll without them writes nothing before its claim).
    pub fn poll_body(&self, acks: bool) -> Value {
        let session = self.session();
        let mut body = json!({
            "host_id": session.host,
            "epoch": session.epoch,
            "session": session.secret,
        });
        if acks && !self.acks.is_empty() {
            body["acks"] = json!(self.acks);
        }
        body
    }

    /// One dispatch poll to `server` carrying what it owes. It is sent only when a start is
    /// due to it (see [`due`]), so the first claim answers it and no poll is held.
    pub async fn poll(&mut self, server: SocketAddr) {
        let reply = post_at(server, "/v1/agent/dispatch", &self.poll_body(true)).await;
        self.acks.clear();
        self.answered(&reply);
    }

    /// Takes the answer of a dispatch poll: each start is run and owed an acknowledgement.
    pub fn answered(&mut self, reply: &Reply) {
        assert_eq!(reply.status, 200, "{}: {}", self.host, reply.body);
        let reply = reply.json();
        for message in reply["messages"].as_array().into_iter().flatten() {
            assert_eq!(message["kind"], "start", "{}: {reply}", self.host);
            let job = message["job_id"].as_str().unwrap().to_owned();
            let allocation = message["allocation_id"].as_str().unwrap().to_owned();
            self.acks
                .push(json!({ "kind": "start", "allocation_id": allocation }));
            self.take(job, allocation);
        }
    }

    /// A start of `job` under `allocation`: counted, and run unless it runs or ran already
    /// (the agent's start is idempotent on the job id).
    fn take(&mut self, job: String, allocation: String) {
        *self
            .deliveries
            .entry((job.clone(), allocation.clone()))
            .or_default() += 1;
        self.starts
            .entry(job.clone())
            .or_default()
            .insert(allocation.clone());
        if !self.ended.contains_key(&job) && !self.done.contains(&job) {
            self.running.entry(job).or_insert(allocation);
        }
    }

    /// Ends the job it has run longest (the lowest id it runs), exit code 0.
    pub fn end_one(&mut self) {
        if let Some((job, allocation)) = self.running.pop_first() {
            self.ended.insert(job, allocation);
        }
    }

    /// Ends every job it runs, exit code 0.
    pub fn end_all(&mut self) {
        self.ended.append(&mut self.running);
    }
}

/// Whether a start is due to `agent`: a job of `jobs` the job service at `job` reports
/// placed on its host that it has not been told to start.
pub async fn due(job: SocketAddr, jobs: &[String], agent: &Agent) -> bool {
    for id in jobs {
        if agent.starts.contains_key(id) {
            continue;
        }
        let read = job_at(job, id).await;
        if read["state"] == "JOB_STATE_PLACED" && read["hostId"] == agent.host.as_str() {
            return true;
        }
    }
    false
}

/// POSTs `body` to `path` on the agent listener at `server` with the join token.
pub async fn post_at(server: SocketAddr, path: &str, body: &Value) -> Reply {
    let bytes = body.to_string().into_bytes();
    request(server, "POST", path, Some(JOIN), Some(&bytes)).await
}

/// An id source that mints as [`RandomIds`] and, once armed, parks one mint until released.
/// A placement mints its allocation id inside its claimed transaction, after the job row is
/// locked and before the commit, so a parked mint holds a server inside a placement it
/// pulled. The mint runs on the pool for blocking calls, so parking it stalls no task.
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    /// The mints to let pass before the one that parks, while armed.
    armed: Option<u32>,
    parked: bool,
    released: bool,
}

impl Gate {
    /// Parks the mint after the next `skip`.
    pub fn arm(&self, skip: u32) {
        let mut state = self.lock();
        *state = GateState {
            armed: Some(skip),
            ..GateState::default()
        };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns once a mint is parked, with what lets it go on. Dropping that lets it go on
    /// too, so a test that fails while a mint is parked ends instead of waiting for the mint
    /// (the runtime waits for its blocking calls when it is dropped).
    pub async fn parked(self: &Arc<Self>) -> Parked {
        let gate = Arc::clone(self);
        off_runtime(move || {
            let mut state = gate.lock();
            while !state.parked {
                state = gate
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        })
        .await;
        Parked(Arc::clone(self))
    }
}

/// A mint a [`Gate`] parked; dropping it lets the mint go on.
pub struct Parked(Arc<Gate>);

impl Parked {
    /// Lets the parked mint go on.
    pub fn release(self) {}
}

impl Drop for Parked {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.released = true;
        self.0.changed.notify_all();
    }
}

impl IdSource for Gate {
    fn mint(&self) -> Result<[u8; 16], MintError> {
        let mut state = self.lock();
        match state.armed {
            Some(0) => {
                state.armed = None;
                state.parked = true;
                self.changed.notify_all();
                while !state.released {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                state.parked = false;
            }
            Some(skip) => state.armed = Some(skip - 1),
            None => {}
        }
        drop(state);
        RandomIds.mint()
    }
}

/// The store's commit head, held by a transaction of the test's own on the database at
/// `url`. Every claim of a queue element (a placement's, a dispatch message's) takes the head
/// first in its transaction, so a claim begun while it is held waits there, inside its
/// transaction, until it is let go. The connection lives on a thread of its own: the
/// blocking client does not run inside the test's runtime.
pub struct HeadLock {
    url: String,
    release: mpsc::Sender<()>,
    holder: Option<thread::JoinHandle<()>>,
    /// Set when the lock is let go or dropped: a wait for a waiter still running ends.
    over: Arc<AtomicBool>,
}

/// The waiters Postgres lists on the head table of the current database.
const WAITERS: &str = "SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation \
     WHERE c.relname = 'head' AND NOT l.granted \
     AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())";

/// The schema of the store's head table.
const SCHEMA: &str = "SELECT n.nspname FROM pg_class c JOIN pg_namespace n \
     ON n.oid = c.relnamespace WHERE c.relname = 'head' AND c.relkind = 'r'";

impl HeadLock {
    /// Takes the head; returns once it is held.
    pub async fn take(url: &str) -> Self {
        let (taken, held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let owned = url.to_owned();
        let holder = thread::spawn(move || {
            let mut client = Client::connect(&owned, NoTls).unwrap();
            let schema: String = client.query_one(SCHEMA, &[]).unwrap().get(0);
            let mut tx = client.transaction().unwrap();
            tx.batch_execute(&format!(
                "LOCK TABLE \"{schema}\".head IN SHARE UPDATE EXCLUSIVE MODE"
            ))
            .unwrap();
            taken.send(()).unwrap();
            // Let go when told, or when the test ends without telling (a failed assertion).
            let _ = released.recv();
            tx.rollback().unwrap();
        });
        off_runtime(move || held.recv().unwrap()).await;
        Self {
            url: url.to_owned(),
            release,
            holder: Some(holder),
            over: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns once a transaction waits on the head, and asserts it is the only one: the test
    /// sends one request at a time while it holds the head, so the waiter is that request's.
    pub async fn waited_on(&self) {
        let (url, over) = (self.url.clone(), Arc::clone(&self.over));
        let waiting = off_runtime(move || {
            let mut client = Client::connect(&url, NoTls).unwrap();
            while !over.load(Ordering::SeqCst) {
                let waiting: i64 = client.query_one(WAITERS, &[]).unwrap().get(0);
                if waiting > 0 {
                    return waiting;
                }
                thread::yield_now();
            }
            0
        })
        .await;
        assert_eq!(waiting, 1, "one transaction waits on the head");
    }

    /// Lets the head go; returns once it is let go.
    pub async fn release(mut self) {
        self.over.store(true, Ordering::SeqCst);
        self.release.send(()).unwrap();
        let holder = self.holder.take().expect("the holder runs");
        off_runtime(move || holder.join().unwrap()).await;
    }
}

impl Drop for HeadLock {
    fn drop(&mut self) {
        // A test that fails while it holds the head lets it go, and ends any wait for a waiter.
        self.over.store(true, Ordering::SeqCst);
        let _ = self.release.send(());
    }
}

/// Runs `wait` (a call that blocks until an event) on a thread of its own, and returns what it
/// returns once it has, under [`guard`]'s bound. The thread is not the runtime's, so a test that
/// fails while the call still blocks ends instead of waiting for it.
async fn off_runtime<T: Send + 'static>(wait: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, answer) = tokio::sync::oneshot::channel();
    thread::spawn(move || {
        let _ = done.send(wait());
    });
    guard(answer).await.expect("the waiting thread answers")
}

/// The serving server's side of a roll, as it writes to the store: its own job store over the
/// shared database, and the jobs it holds that have not ended, with their states. The unit test
/// of the open, `src/job_store_busy_tests.rs`, drives the open with the same kind of writer.
pub struct Serving {
    jobs: JobStore<Arc<dyn Store>, RandomIds>,
    /// The jobs that have not ended, by id.
    live: BTreeMap<JobId, JobState>,
    /// A job's body, which every job it queues in place of an ended one repeats.
    template: NewJob,
    /// How many changes it may still make.
    left: usize,
    /// How many changes it made.
    made: usize,
    /// How many jobs it has queued in place of ended ones.
    queued: usize,
}

impl Serving {
    /// The serving server over `store`, holding the jobs `ids` (which must exist, and which it
    /// may change `steps` times in all).
    pub fn new(store: Arc<dyn Store>, ids: &[String], steps: usize) -> Self {
        let jobs = JobStore::new(Arc::new(store), RandomIds).expect("the serving job store");
        let mut live = BTreeMap::new();
        let mut template = None;
        for id in ids {
            let id = JobId::parse(id).expect("a job id");
            let job = jobs.get_job(&id).expect("reads").expect("present");
            template = Some(NewJob {
                id: None,
                spec: job.spec,
                priority: 0,
                client: None,
            });
            live.insert(id, job.state);
        }
        Self {
            jobs,
            live,
            template: template.expect("a job to repeat"),
            left: steps,
            made: 0,
            queued: 0,
        }
    }

    /// Moves its oldest live job (the lowest id) one step: queued to placed, placed to
    /// starting, starting to running, running to ended; a job that ends is replaced by a new
    /// queued one (its id sorts after the first jobs', so the oldest live job is always one the
    /// open has read), so the farm stays as busy. Each change is one commit, and must hold at its
    /// first try: a server that opens its job store never makes the serving one lose a write.
    fn step(&mut self) {
        let (id, state) = self.live.pop_first().expect("a live job");
        let host = HostId::new("busy-host").expect("a host id");
        let booked = Booked::Shape {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: 0,
            accelerators: 0,
        };
        let (moved, now) = match state {
            JobState::Queued => (
                self.jobs.place(&id, host, booked).map(drop),
                JobState::Placed,
            ),
            JobState::Placed => {
                let to = JobState::Starting;
                (self.jobs.transition(&id, state, to).map(drop), to)
            }
            JobState::Starting => {
                let to = JobState::Running;
                (self.jobs.transition(&id, state, to).map(drop), to)
            }
            _ => (
                self.jobs.end_job(&id, JobEnd::Lost).map(drop),
                JobState::Lost,
            ),
        };
        moved.expect("the serving server's change commits at its first try");
        self.made += 1;
        if now.is_ended() {
            let id = JobId::parse(&format!("zz-busy-{:05}", self.queued)).expect("an id");
            self.queued += 1;
            let new = NewJob {
                id: Some(id.clone()),
                ..self.template.clone()
            };
            self.jobs
                .insert_queued(new)
                .expect("the serving server queues a job");
            self.live.insert(id, JobState::Queued);
        } else {
            self.live.insert(id, now);
        }
    }
}

/// A store clock that makes the serving server change one job at every reading, while it has
/// changes left, and then answers its own clock: a writer that never pauses and is bounded by
/// a count. A store reads its clock once at the start of every call, so between any two calls
/// of a server that opens its job store with this clock, a job has changed.
pub struct Busy {
    clock: ManualClock,
    serving: Mutex<Serving>,
}

impl Busy {
    /// The clock, reading `clock` once the serving server has no change left.
    pub fn new(clock: ManualClock, serving: Serving) -> Self {
        Self {
            clock,
            serving: Mutex::new(serving),
        }
    }

    /// Stops the writer, and returns how many changes it had left and how many it made (each
    /// committed at its first try, or the test has failed). Call it before any read through a
    /// store that reads this clock, which would change a job under the read; and never hold
    /// the writer's lock across a call that does.
    pub fn stop(&self) -> (usize, usize) {
        let mut serving = self.serving.lock().unwrap_or_else(PoisonError::into_inner);
        let left = std::mem::take(&mut serving.left);
        (left, serving.made)
    }
}

impl Clock for Busy {
    fn now(&self) -> StoreTime {
        let mut serving = self.serving.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(left) = serving.left.checked_sub(1) {
            serving.left = left;
            serving.step();
        }
        self.clock.now()
    }
}
