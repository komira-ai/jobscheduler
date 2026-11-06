//! The live-log round trip against the S3-compatible test store with STS (`js_testkit::s3`;
//! CI's `test` job runs it in a service container): the server in process with an
//! object-store sink at the test store, configured from a file as an operator would, whose
//! STS mints the host's credential and whose read key the job service reads with; an agent
//! played by the test over the agent routes (hello, heartbeats with the job listing, the
//! credential state and the result) that uploads with js-agent's own uploader; and `GetLogs`
//! through the gateway of the job port, its bytes compared with what the job wrote.
//!
//! Every test is `#[ignore]`d, with the reason, so the default `cargo test` never reaches for
//! the store; the `test` job runs them with `--ignored`. The server and the uploader read the
//! real time (the store refuses a signing time more than 15 minutes from its own); the
//! uploader's clock moves only when the test moves it, and its wake is manual. The runtime is
//! a current-thread one on paused time with one thread for blocking calls: a mint the server
//! queued is over once a blocking call queued after it has run, and the one second a follow
//! waits between two reads passes only once nothing else is to be done, never while a store
//! request runs. No test waits for real time.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use js_agent::logs::{HostGrant, Limits, LiveLogs, ManualWake, Settled, Sink, Uploader, Wake};
use js_agent_wire::{
    Command, Heartbeat, HeartbeatReply, HeldJob, HeldState, JobResult, JobStartSpec, LogReport,
    Outcome, Phase,
};
use js_lease_server::in_process::InProcess;
use js_lease_server::{Config, Kinds, OwnerOnlyFiles, SharedClock};
use js_logstore::{Clock as _, ManualClock, UreqTransport};
use js_proto::v1::GetLogsResponse;
use js_testkit::s3::{TestStore, unique};
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config_support::{PoolText, file, quote, quoted, server};
use crate::support::{Scratch, TOKEN};

/// The join token the agent says hello with.
const JOIN: &str = "join-token-L-0123456789-abcdefghijklmnop";

/// The pool the agent joins.
const POOL: &str = "burst";

/// The image of the job: pinned by digest, as the service demands.
const IMAGE: &str =
    "team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The role every mint asks for; the session policy is what narrows the credential.
const ROLE: &str = "arn:aws:iam::000000000000:role/js-live-logs";

/// The chunk size of the uploader, small so that a few kilobytes make several chunks.
const CHUNK: usize = 4096;

/// The largest page `GetLogs` gives.
const PAGE: u32 = 1 << 20;

/// What the job writes to stdout: 10,000 bytes of every value (chunks at 0, 4096 and 8192)
/// and a line.
fn stdout_bytes() -> Vec<u8> {
    let head: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    [head.as_slice(), b"the last line\n"].concat()
}

/// A runtime on paused time, with one thread for blocking calls.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()
        .expect("a runtime")
}

/// Waits until every blocking call queued before it (a mint the server queued) has run: the
/// one thread for blocking calls runs them in order. It waits for no time.
async fn settle() {
    tokio::task::spawn_blocking(|| ())
        .await
        .expect("a blocking call");
}

/// What a request got back: the status and the body.
struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// One request answered by `router`, called as a service, with the bearer `token`.
async fn routed(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> Reply {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.map(Value::to_string).unwrap_or_default()))
        .expect("a request");
    let response = router.clone().oneshot(request).await.expect("an answer");
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body")
        .to_vec();
    Reply { status, body }
}

/// A page of `GetLogs`, decoded from the gateway's JSON by the proto3 JSON mapping.
fn page_of(reply: &Reply) -> GetLogsResponse {
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let pool = DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).expect("the set decodes");
    let descriptor = pool
        .get_message_by_name("jobscheduler.v1.GetLogsResponse")
        .expect("the message is defined");
    let json: Value = serde_json::from_slice(&reply.body).expect("JSON");
    DynamicMessage::deserialize(descriptor, json)
        .expect("the body decodes")
        .transcode_to::<GetLogsResponse>()
        .expect("the message converts")
}

/// The server in process with its sink at the test store, and the agent of one host the test
/// plays: its session, the credential cell its uploader reads, the uploader's clock and wakes,
/// and the job it was started.
struct Cell {
    server: InProcess,
    host: String,
    session: (u64, String),
    clock: Arc<ManualClock>,
    grant: Arc<HostGrant>,
    wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
    /// The access key id of every credential a heartbeat reply brought, in order.
    delivered: Mutex<Vec<String>>,
    scratch: Scratch,
}

/// The job the agent runs: its start and its capture files.
struct Job {
    start: JobStartSpec,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Cell {
    /// The server with the job service on and an object-store sink at the test store, minting
    /// with the store's root key for the shortest lifetime the server takes (15 minutes, the
    /// shortest the store's STS issues too) and reading with it, and an agent that has said
    /// hello with the `per-job` and `log-upload` capabilities and holds the credential the
    /// first heartbeat's reply brought.
    async fn start(name: &str) -> Self {
        let test = TestStore::from_env();
        let scratch = Scratch::new(name);
        let key = format!(
            "{{\"access_key_id\": {}, \"secret_access_key\": {}}}",
            quote(&test.access_key_id),
            quote(&test.secret_access_key)
        );
        let read_key = scratch.write("read-key", &key, 0o600);
        let mint_key = scratch.write("mint-key", &key, 0o600);
        let join = scratch.write("join-token", &format!("{JOIN}\n"), 0o600);
        let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\
                         job_listen = \"127.0.0.1:0\"\nplacement = \"js-sched\"";
        let pool = PoolText::joined(POOL)
            .set("capabilities", "{ arch = \"x86_64\" }")
            .set("join_token_file", &quoted(&join));
        let logs = format!(
            "[logs]\nsink = \"object_store\"\nurl = {}\nregion = {}\nendpoint = {}\n\
             path_style = true\nread_key_file = {}\nrole_arn = {}\nmint_key_file = {}\n\
             credential_ttl = \"15m\"\n",
            quote(&format!("s3://{}/{}", test.bucket, unique("server"))),
            quote(&test.region),
            quote(&test.base_url),
            quoted(&read_key),
            quote(ROLE),
            quoted(&mint_key),
        );
        let text = format!("{}\n{logs}", file(&server(&scratch, listeners), &[&pool]));
        let path = scratch.write("js.toml", &text, 0o644);
        let clock = SharedClock::new(js_lease::SystemClock);
        let config =
            Config::from_config_file(&path, &Kinds::standard(), &OwnerOnlyFiles, clock.clone())
                .expect("the file loads");
        let server = InProcess::start(config, clock).expect("the server starts");
        let agent_clock = Arc::new(ManualClock::new(SystemTime::now()));
        let mut cell = Self {
            server,
            host: unique("host"),
            session: (0, String::new()),
            grant: Arc::new(HostGrant::new(agent_clock.clone())),
            clock: agent_clock,
            wakes: Arc::default(),
            delivered: Mutex::default(),
            scratch,
        };
        cell.hello().await;
        settle().await;
        cell.beat(Vec::new()).await;
        assert_eq!(
            cell.grant.generation(),
            1,
            "the first heartbeat's reply brought one"
        );
        cell
    }

    async fn hello(&mut self) {
        let hello = json!({
            "host_id": self.host,
            "pool": POOL,
            "agent_version": "0.1.0",
            "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
            "capabilities": { "per-job": "true", "log-upload": "true", "arch": "x86_64" },
            "phase": "serving",
        });
        let router = self.server.agent_router();
        let welcome = routed(&router, "POST", "/v1/agent/hello", JOIN, Some(&hello)).await;
        assert_eq!(
            welcome.status,
            200,
            "{}",
            String::from_utf8_lossy(&welcome.body)
        );
        let welcome: Value = serde_json::from_slice(&welcome.body).expect("JSON");
        let epoch = welcome["epoch"].as_u64().expect("an epoch");
        let session = welcome["session"].as_str().expect("a session").to_owned();
        self.session = (epoch, session);
    }

    /// One heartbeat listing `jobs` and the cell's credential state; the reply's credential,
    /// when it brings one, goes into the cell, as the agent's session puts it.
    async fn beat(&self, jobs: Vec<HeldJob>) -> HeartbeatReply {
        let beat = Heartbeat {
            host_id: self.host.clone(),
            epoch: self.session.0,
            session: self.session.1.clone(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load: None,
            disk_gib: None,
            jobs,
            retired_jobs: Vec::new(),
            log_grant_state: Some(self.grant.state()),
        };
        let body = serde_json::to_value(&beat).expect("a heartbeat");
        let router = self.server.agent_router();
        let answer = routed(&router, "POST", "/v1/agent/heartbeat", JOIN, Some(&body)).await;
        assert_eq!(
            answer.status,
            200,
            "{}",
            String::from_utf8_lossy(&answer.body)
        );
        let reply: HeartbeatReply = serde_json::from_slice(&answer.body).expect("a reply");
        if let Some(grant) = &reply.log_grant {
            self.grant.replace(grant);
            let mut delivered = self.delivered.lock().expect("delivered");
            delivered.push(grant.access_key_id.clone());
        }
        reply
    }

    /// Submits a job through the gateway, and beats until its start arrives; makes its
    /// capture files.
    async fn job(&self) -> Job {
        let spec = json!({
            "spec": {
                "image": IMAGE,
                "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
                "wallLimitSeconds": "600",
            }
        });
        let router = self.server.job_router().expect("the job service is on");
        let submitted = routed(&router, "POST", "/v1/jobs", TOKEN, Some(&spec)).await;
        assert_eq!(
            submitted.status,
            201,
            "{}",
            String::from_utf8_lossy(&submitted.body)
        );
        let mut start = None;
        for _ in 0..3 {
            let reply = self.beat(Vec::new()).await;
            start = reply
                .commands
                .into_iter()
                .find_map(|command| match command {
                    Command::JobStart(spec) => Some(*spec),
                    _ => None,
                });
            if start.is_some() {
                break;
            }
        }
        let start = start.expect("the job's start arrives within three heartbeats");
        let [stdout, stderr] = ["stdout", "stderr"].map(|stream| {
            let path = self.scratch.path(&format!("{}.{stream}", start.job_id));
            File::create(&path).expect("a capture file");
            path
        });
        Job {
            start,
            stdout,
            stderr,
        }
    }

    /// The uploader of `job`, over the sink its start carried, with the cell's credential,
    /// clock and a manual wake, chunks of [`CHUNK`] bytes cut as soon as they are seen; its
    /// first step is over.
    fn upload(&self, job: &Job) -> (Uploader, Arc<ManualWake>) {
        let sink = Sink::of_start(&job.start)
            .expect("a good sink")
            .expect("the start carries a sink");
        let made = self.wakes.clone();
        let logs = LiveLogs {
            file_cap: 64 << 20,
            grant: self.grant.clone(),
            transport: Arc::new(UreqTransport::new(Duration::from_secs(30))),
            clock: self.clock.clone(),
            wakes: Arc::new(move || {
                let wake = ManualWake::new();
                made.lock().expect("wakes").push(wake.clone());
                wake as Arc<dyn Wake>
            }),
            limits: Limits {
                chunk: CHUNK,
                flush: Duration::ZERO,
                ..Limits::default()
            },
        };
        let uploader =
            Uploader::start(&sink, (&job.stdout, &job.stderr), &logs).expect("it starts");
        let wake = self
            .wakes
            .lock()
            .expect("wakes")
            .last()
            .cloned()
            .expect("a wake");
        assert_eq!(wake.settled(), Settled::Asleep);
        (uploader, wake)
    }

    /// The job listed as running.
    fn running(job: &Job) -> Vec<HeldJob> {
        vec![HeldJob {
            job_id: job.start.job_id.clone(),
            state: HeldState::Running,
            result: None,
        }]
    }

    /// Ends `job` as the agent does: the uploader is told the totals and runs out (its
    /// finals written, its chunks deleted), and the result with the stream lengths goes out
    /// in a heartbeat, which the server accepts.
    async fn end(&self, job: &Job, uploader: &Uploader, wake: &ManualWake) {
        let totals = [&job.stdout, &job.stderr].map(|path| len(path));
        let report = uploader.finish((totals[0], totals[1]));
        assert_eq!(report.stdout_dropped + report.stderr_dropped, 0);
        while wake.step() != Settled::Ended {}
        self.end_with(job, report).await;
    }

    /// Reports the end of `job` with `report` in a heartbeat, which the server accepts.
    async fn end_with(&self, job: &Job, report: LogReport) {
        let result = JobResult {
            job_id: job.start.job_id.clone(),
            allocation_id: job.start.allocation_id.clone(),
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            detail: None,
            log_tail: None,
            logs: Some(report),
        };
        let ended = vec![HeldJob {
            job_id: job.start.job_id.clone(),
            state: HeldState::ExitedUnacked,
            result: Some(result),
        }];
        let reply = self.beat(ended).await;
        let acked: Vec<&str> = reply
            .job_acks
            .iter()
            .map(|ack| ack.job_id.as_str())
            .collect();
        assert_eq!(acked, [job.start.job_id.as_str()], "the result is accepted");
    }

    /// The path of a `GetLogs` of `stream` (`STDOUT` or `STDERR`) of `job` from `from`.
    fn logs_path(job: &Job, stream: &str, from: u64, follow: bool) -> String {
        format!(
            "/v1/jobs/{}/logs?stream=LOG_STREAM_{stream}&fromOffset={from}&maxBytes={PAGE}&follow={follow}",
            job.start.job_id
        )
    }

    /// One `GetLogs` call, answered.
    async fn get_logs(&self, job: &Job, stream: &str, from: u64, follow: bool) -> Reply {
        let router = self.server.job_router().expect("the job service is on");
        routed(
            &router,
            "GET",
            &Self::logs_path(job, stream, from, follow),
            TOKEN,
            None,
        )
        .await
    }

    /// The bytes `GetLogs` reads of `stream` of `job` from `from`, page after page while
    /// pages hold data, with no gap; and whether the last page was at the end.
    async fn read_from(&self, job: &Job, stream: &str, from: u64) -> (Vec<u8>, bool) {
        let mut bytes = Vec::new();
        let mut at = from;
        for _ in 0..16 {
            let page = page_of(&self.get_logs(job, stream, at, false).await);
            assert!(page.gaps.is_empty(), "no gap from {at}: {:?}", page.gaps);
            assert_eq!(page.offset, at, "a page begins where it was asked");
            assert_eq!(page.next_offset, at + page.data.len() as u64);
            at = page.next_offset;
            let (empty, eof) = (page.data.is_empty(), page.eof);
            bytes.extend(page.data);
            if empty || eof {
                return (bytes, eof);
            }
        }
        panic!("more than 16 pages from {from}");
    }
}

/// The length of the file `path`.
fn len(path: &Path) -> u64 {
    std::fs::metadata(path).expect("a capture file").len()
}

/// Appends `bytes` to the capture file `path`, as the capture does.
fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).expect("open");
    file.write_all(bytes).expect("append");
}

/// Milliseconds since the Unix epoch at `at`.
fn ms(at: SystemTime) -> u64 {
    u64::try_from(at.duration_since(UNIX_EPOCH).expect("later").as_millis()).expect("ms")
}

/// Proves `GetLogs` gives back exactly the bytes the job wrote, as the agent's uploader put
/// them in the store under the sink the server sent and with the credential the server's STS
/// minted: while the job runs (from the chunks) and after it ended (from the finals), for the
/// whole stream, from an offset on a chunk boundary (4096) and from one inside a chunk
/// (5000), for both streams; a page read from the chunks carries the time its first byte was
/// seen; after the end the last page is at the end, a read at the length is an empty page at
/// the end, and one past it is refused as out of range. Catches a chunk key the agent writes
/// other than the server reads, a sink prefix other than the host's prefix the credential
/// covers, a request signed for a region or with a payload hash the store refuses, and a page
/// that loses or repeats bytes across chunks.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn get_logs_reads_back_what_the_job_wrote_from_any_offset() {
    runtime().block_on(async {
        let cell = Cell::start("logs-round-trip").await;
        let job = cell.job().await;
        let (uploader, wake) = cell.upload(&job);
        cell.beat(Cell::running(&job)).await;
        let out = stdout_bytes();
        let err = b"a warning on stderr\n".to_vec();
        append(&job.stdout, &out);
        append(&job.stderr, &err);
        let seen_at = ms(cell.clock.now());
        assert_eq!(wake.step(), Settled::Asleep);

        for from in [0_usize, 4096, 5000] {
            let (bytes, eof) = cell.read_from(&job, "STDOUT", from as u64).await;
            assert!(
                bytes == out[from..],
                "stdout from {from} while the job runs"
            );
            assert!(!eof, "a running job's stream has no end");
        }
        let first = page_of(&cell.get_logs(&job, "STDOUT", 0, false).await);
        assert_eq!(first.first_byte_at_ms, Some(seen_at));
        assert_eq!(
            cell.read_from(&job, "STDERR", 0).await,
            (err.clone(), false)
        );

        cell.end(&job, &uploader, &wake).await;
        for from in [0_usize, 4096, 5000] {
            let (bytes, eof) = cell.read_from(&job, "STDOUT", from as u64).await;
            assert!(bytes == out[from..], "stdout from {from} after the end");
            assert!(eof, "the last page is at the end");
        }
        assert_eq!(cell.read_from(&job, "STDERR", 0).await, (err, true));
        let at_end = page_of(&cell.get_logs(&job, "STDOUT", out.len() as u64, false).await);
        assert!(at_end.data.is_empty() && at_end.eof && at_end.next_offset == out.len() as u64);
        let past = cell
            .get_logs(&job, "STDOUT", out.len() as u64 + 1, false)
            .await;
        assert_eq!(past.status, 400, "{}", String::from_utf8_lossy(&past.body));
    });
}

/// Starts a follow of stdout of `job` from `from` and returns once it has read the store and
/// waits: a call that must see bytes written after it began. The one second it waits passes
/// only once nothing else is to be done, which the half second the test waits here comes
/// before.
async fn follow_from(
    cell: &Arc<Cell>,
    job: &Arc<Job>,
    from: u64,
) -> tokio::task::JoinHandle<Reply> {
    let (cell, job) = (Arc::clone(cell), Arc::clone(job));
    let call = tokio::spawn(async move { cell.get_logs(&job, "STDOUT", from, true).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!call.is_finished(), "the follow waits for news");
    call
}

/// Proves a follow answers with the bytes written after it began and ends with the job: a
/// follow from the end of what the job wrote reads the store, finds nothing and waits; the
/// job writes a line, the uploader puts it, and the follow answers it, from where it was
/// asked. A second follow from the new end waits; the job ends (finals written, the result
/// with the lengths accepted) and the follow answers an empty page at the end. Catches a
/// follow that answers before anything new is there, one that never sees a chunk put while it
/// waits, and one that answers anything but the end once the job ended.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_follow_returns_bytes_written_after_it_began_and_ends_with_the_job() {
    runtime().block_on(async {
        let cell = Arc::new(Cell::start("logs-follow").await);
        let job = Arc::new(cell.job().await);
        let (uploader, wake) = cell.upload(&job);
        cell.beat(Cell::running(&job)).await;
        let before = b"written before the follow\n";
        append(&job.stdout, before);
        assert_eq!(wake.step(), Settled::Asleep);

        let follow = follow_from(&cell, &job, before.len() as u64).await;
        let news = b"written while the follow waits\n";
        append(&job.stdout, news);
        assert_eq!(wake.step(), Settled::Asleep);
        let page = page_of(&follow.await.expect("the follow answers"));
        assert_eq!(page.offset, before.len() as u64);
        assert_eq!(page.data, news);
        assert!(!page.eof);

        let end = (before.len() + news.len()) as u64;
        let last = follow_from(&cell, &job, end).await;
        cell.end(&job, &uploader, &wake).await;
        let page = page_of(&last.await.expect("the follow answers"));
        assert!(page.data.is_empty() && page.gaps.is_empty(), "nothing new");
        assert_eq!((page.offset, page.next_offset, page.eof), (end, end, true));
    });
}

/// Proves a credential renewed by the server mid-run keeps the stream whole: the uploader
/// puts a chunk with the credential the first reply brought; the store then refuses it (the
/// cell records the refusal, as the uploader does on the store's answer), the next heartbeat
/// reports it stale, the server mints a new one from the store's STS, and the reply after
/// that brings it; the next chunk is put with it, and `GetLogs` reads both chunks as one
/// stream with no gap. Catches a server that does not mint again on a stale report of the
/// credential it holds, a renewal the reply never carries, and a credential minted with a
/// scope or region the store refuses.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_renewal_the_server_mints_on_a_stale_report_keeps_the_stream_whole() {
    runtime().block_on(async {
        let cell = Cell::start("logs-renewal").await;
        let job = cell.job().await;
        let (uploader, wake) = cell.upload(&job);
        cell.beat(Cell::running(&job)).await;
        let first = b"put with the first credential\n";
        append(&job.stdout, first);
        assert_eq!(wake.step(), Settled::Asleep);

        cell.grant.refused(cell.grant.generation());
        assert!(cell.grant.state().stale);
        cell.beat(Cell::running(&job)).await;
        settle().await;
        cell.beat(Cell::running(&job)).await;
        assert_eq!(cell.grant.generation(), 2, "the renewal is held");
        assert!(!cell.grant.state().stale);
        let keys = cell.delivered.lock().expect("delivered").clone();
        assert_eq!(keys.len(), 2, "the first credential and the renewal");
        assert_ne!(keys[0], keys[1], "the renewal is a new key");

        let second = b"put with the renewal\n";
        append(&job.stdout, second);
        assert_eq!(wake.step(), Settled::Asleep);
        let whole = [first.as_slice(), second].concat();
        assert_eq!(
            cell.read_from(&job, "STDOUT", 0).await,
            (whole.clone(), false)
        );
        cell.end(&job, &uploader, &wake).await;
        assert_eq!(cell.read_from(&job, "STDOUT", 0).await, (whole, true));
    });
}
