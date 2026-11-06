//! The live-log sink of a job start through the lifecycle: checked once, handed to the
//! runtime in the run's spec, and the runtime's report carried in the job's result. The
//! runtime is the fake; the uploader itself is the `logs` module's and the stand-in's, and
//! `s3_compat` below runs it against the S3-compatible test store with STS.

use js_agent::lifecycle::code;
use js_agent::logs::Sink;
use js_agent_wire::{BookedResources, Command, JobStartSpec, LogReport, LogSink, Outcome};

use super::{Harness, job};

const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start of `job_id` whose sink names the prefix `prefix` in the bucket `bucket`.
fn start(job_id: &str, bucket: &str, prefix: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: IMAGE.to_owned(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: Some(LogSink {
            endpoint: "https://store.example".to_owned(),
            region: "test-1".to_owned(),
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            path_style: false,
        }),
        kind: Default::default(),
    }
}

fn report() -> LogReport {
    LogReport {
        stdout_bytes: 3_000_000,
        stderr_bytes: 12,
        stdout_dropped: 900_000,
        stderr_dropped: 0,
    }
}

/// Proves a checked sink reaches the runtime and the runtime's report reaches the result: the
/// run's spec holds the sink the start's checks give, and the job's listed result carries
/// `logs` beside its outcome. A start that waits for its image keeps its sink until the
/// container is made; a run start and a job start without a sink hand none, and a job
/// whose runtime reports nothing lists no `logs`. Catches a sink lost on the way, a report
/// left out of the result, or one made up for a run that has none.
#[test]
fn a_checked_sink_reaches_the_runtime_and_its_report_the_result() {
    let mut h = Harness::new("live-sink");
    let given = start("j1", "logs", "js/j1/");
    let want = Sink::of_start(&given).expect("a good sink");
    h.apply(vec![Command::JobStart(Box::new(given))]);
    h.fake.report_logs("j1", report());
    h.fake.exit("j1", 0);
    h.life.step();
    let listed = h.life.held_jobs();
    let result = listed[0].result.as_ref().expect("a result");
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.logs, Some(report()));

    h.fake.pull_pending(IMAGE);
    let mut bare = start("j2", "logs", "js/j2/");
    bare.log_sink = None;
    h.apply(vec![
        Command::JobStart(Box::new(start("j3", "logs", "js/j3/"))),
        Command::JobStart(Box::new(bare)),
    ]);
    h.fake.pull_done(IMAGE);
    h.life.step();
    h.start(job("r4"));
    let sinks: Vec<(String, Option<Sink>)> = h
        .fake
        .created()
        .into_iter()
        .map(|spec| (spec.run_id, spec.log_sink))
        .collect();
    let of = |job_id: &str| {
        Sink::of_start(&start(job_id, "logs", &format!("js/{job_id}/"))).expect("a good sink")
    };
    assert_eq!(
        sinks,
        [
            ("j1".to_owned(), want),
            ("j3".to_owned(), of("j3")),
            ("j2".to_owned(), None),
            ("r4".to_owned(), None),
        ]
    );
    h.fake.exit("j2", 0);
    h.life.step();
    let listed = h.life.held_jobs();
    let j2 = listed
        .iter()
        .find(|job| job.job_id == "j2")
        .expect("listed");
    assert_eq!(j2.result.as_ref().expect("a result").logs, None);
}

/// Proves a sink that fails its check costs the job its live logs and nothing else: a prefix
/// that is not the job's, and a bucket the store client refuses, each start the job with no
/// sink in its run and one `log-sink-ignored` note naming the field, never its value; the
/// job runs to its normal result. Catches a bad sink that refuses the job, one that reaches
/// the runtime, a note that repeats or one that prints what was sent.
#[test]
fn a_bad_sink_starts_the_job_without_live_logs() {
    let mut h = Harness::new("live-bad-sink");
    h.apply(vec![
        Command::JobStart(Box::new(start("j1", "logs", "js/another-job/"))),
        Command::JobStart(Box::new(start("j2", "UPPER-CASE", "js/j2/"))),
    ]);
    let sinks: Vec<_> = h
        .fake
        .created()
        .into_iter()
        .map(|spec| spec.log_sink)
        .collect();
    assert_eq!(sinks, [None, None]);
    let notes: Vec<(Option<String>, String)> = h
        .life
        .findings()
        .into_iter()
        .filter(|finding| finding.code == code::LOG_SINK_IGNORED)
        .map(|finding| (finding.run_id, finding.message))
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(notes[0].0.as_deref(), Some("j1"));
    assert!(notes[0].1.contains("log_sink.prefix"), "{notes:?}");
    assert!(!notes[0].1.contains("another-job"), "{notes:?}");
    assert_eq!(notes[1].0.as_deref(), Some("j2"));
    assert!(notes[1].1.contains("bucket"), "{notes:?}");
    assert!(!notes[1].1.contains("UPPER-CASE"), "{notes:?}");
    for job_id in ["j1", "j2"] {
        h.fake.exit(job_id, 0);
    }
    h.life.step();
    for listed in h.life.held_jobs() {
        let result = listed.result.expect("a result");
        assert_eq!(result.outcome, Outcome::Succeeded);
        assert_eq!(result.logs, None);
    }
}

/// The uploader against the S3-compatible test store with STS (`js_testkit::s3`; CI's `test`
/// job runs it in a service container): the agent's own client, signer and key layout, the
/// store's STS minting the host's credential, and the store's checks deciding. A proxy in the
/// test process stands between the uploader and the store, to record what was sent and to
/// fail one request; a stand-in of the server answers the session's heartbeats with credentials
/// the store's STS mints.
///
/// Every test is `#[ignore]`d, with the reason, so the default `cargo test` never reaches for
/// the store; the `test` job runs them with `--ignored`. The uploaders' wakes are manual and
/// their clock is one the test moves, from the real time on: the store refuses a signing time
/// more than 15 minutes from its own, so no test moves it further than that. Nothing waits for
/// time.
mod s3_compat {
    use std::collections::{BTreeMap, VecDeque};
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Write as _};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use js_agent::logs::{
        BACKOFF_FIRST, GRANT_STALE_MARGIN, HostGrant, Limits, LiveLogs, ManualWake, Settled, Sink,
        Uploader, Wake,
    };
    use js_agent::{Answer, HELLO_PATH, Session, Transport as AgentTransport};
    use js_agent_wire::{LogGrant, LogReport};
    use js_logmint::{Bucket, LogPrefix, MintRequest, Minter, RoleArn, StsEndpoint, StsMinter};
    use js_logstore::layout::FIRST_BYTE_AT_MS;
    use js_logstore::{
        Credential, Endpoint, HttpRequest, HttpResponse, JobPrefix, ManualClock, Method, Store,
        Stream, SystemClock, Transport, TransportError, UreqTransport,
    };
    use js_testkit::dir::TestDir;
    use js_testkit::s3::{TestStore, unique};
    use serde_json::{Value, json};

    use crate::support::{hello, lowest, welcome};

    /// How long one request may take: the store is on this machine.
    const CALL_LIMIT: Duration = Duration::from_secs(30);

    /// The role every mint asks for; the session policy is what narrows the credential.
    const ROLE: &str = "arn:aws:iam::000000000000:role/js-live-logs";

    /// The shortest lifetime the test store's STS issues: the host's first credential.
    const SHORTEST: Duration = Duration::from_secs(900);

    /// The lifetime of a renewal, longer than the first so that it ends the stale report.
    const RENEWED: Duration = Duration::from_secs(1800);

    /// The root key's expiry: it has none.
    const NEVER: Duration = Duration::from_secs(1 << 40);

    /// The chunk size of the tests' uploaders, small so that a few kilobytes make several.
    const CHUNK: usize = 4096;

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One request the proxy was sent: its method, the key it names, the access key id it was
    /// signed with and its body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Sent {
        method: Method,
        key: String,
        key_id: String,
        body: Vec<u8>,
    }

    /// What the proxy does to the next put.
    #[derive(Debug, Clone, Copy)]
    enum Fault {
        /// The connection breaks before the store sees the request.
        Drop,
        /// The store takes the request, and its answer is lost on the way back.
        LoseAnswer,
        /// The proxy answers in the store's place, with this status and S3 error code.
        Answer(u16, &'static str),
    }

    /// The transport between the uploader and the store: it records every request, and does
    /// to each put what the next queued fault says (none passes it through).
    #[derive(Clone)]
    struct Proxy {
        store: Arc<UreqTransport>,
        base: String,
        faults: Arc<Mutex<VecDeque<Fault>>>,
        sent: Arc<Mutex<Vec<Sent>>>,
    }

    impl Proxy {
        /// The puts sent so far.
        fn puts(&self) -> Vec<Sent> {
            lock(&self.sent)
                .iter()
                .filter(|sent| sent.method == Method::Put)
                .cloned()
                .collect()
        }

        /// The next put meets `fault`.
        fn fail_next(&self, fault: Fault) {
            lock(&self.faults).push_back(fault);
        }
    }

    impl Transport for Proxy {
        fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            let key = request
                .url
                .strip_prefix(&self.base)
                .and_then(|rest| rest.split('?').next())
                .unwrap_or_default()
                .to_owned();
            let key_id = request
                .header("authorization")
                .and_then(|value| value.split_once("Credential="))
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(id, _)| id.to_owned())
                .unwrap_or_default();
            lock(&self.sent).push(Sent {
                method: request.method,
                key,
                key_id,
                body: request.body.clone(),
            });
            let fault = match request.method {
                Method::Put => lock(&self.faults).pop_front(),
                _ => None,
            };
            match fault {
                None => self.store.send(request),
                Some(Fault::Drop) => Err(TransportError::Io(ErrorKind::ConnectionReset)),
                Some(Fault::LoseAnswer) => {
                    self.store.send(request).expect("the store answers");
                    Err(TransportError::Io(ErrorKind::ConnectionReset))
                }
                Some(Fault::Answer(status, code)) => Ok(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: format!("<Error><Code>{code}</Code></Error>").into_bytes(),
                }),
            }
        }
    }

    /// One agent's uploads to the test store: the store, a log prefix and a host of the test's
    /// own, the host's credential cell, the proxy, the uploaders' clock and their wakes.
    struct Rig {
        test: TestStore,
        endpoint: Endpoint,
        prefix: LogPrefix,
        host: String,
        clock: Arc<ManualClock>,
        grant: Arc<HostGrant>,
        proxy: Proxy,
        wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
        dir: TestDir,
    }

    /// One job's sink, prefix and capture files.
    struct Job {
        sink: Sink,
        prefix: JobPrefix,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    impl Rig {
        /// A rig on the store the variables name, its clock at the real time now.
        fn new(name: &str) -> Self {
            let test = TestStore::from_env();
            let endpoint = Endpoint::parse(&test.base_url, &test.region, &test.bucket, true)
                .expect("an endpoint");
            let clock = Arc::new(ManualClock::new(SystemTime::now()));
            let proxy = Proxy {
                store: Arc::new(UreqTransport::new(CALL_LIMIT)),
                base: endpoint.url(&format!("/{}/", test.bucket)),
                faults: Arc::default(),
                sent: Arc::default(),
            };
            let dir = TestDir::under(
                PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("js-agent-s3"),
                name,
            );
            Self {
                prefix: LogPrefix::new(&unique("agent")).expect("a log prefix"),
                host: unique("host"),
                grant: Arc::new(HostGrant::new(clock.clone())),
                test,
                endpoint,
                clock,
                proxy,
                wakes: Arc::default(),
                dir,
            }
        }

        /// A client of the store with its root key, at the system clock.
        fn root(&self) -> Store<UreqTransport, SystemClock> {
            let root = Credential {
                access_key_id: self.test.access_key_id.clone(),
                secret_access_key: self.test.secret_access_key.clone(),
                session_token: None,
                expires_at: UNIX_EPOCH + NEVER,
            };
            Store::new(
                UreqTransport::new(CALL_LIMIT),
                SystemClock,
                self.endpoint.clone(),
                root,
            )
        }

        /// What a mint for the rig's host asks for, for `ttl`.
        fn request(&self, ttl: Duration) -> MintRequest {
            MintRequest {
                bucket: Bucket::new(&self.test.bucket).expect("a bucket"),
                prefix: self.prefix.clone(),
                host: self.host.clone(),
                ttl,
            }
        }

        /// A credential the store's STS mints for the host for `ttl`, as a heartbeat reply
        /// carries it.
        fn mint(&self, ttl: Duration) -> LogGrant {
            let sts = StsEndpoint::parse(&self.test.base_url, &self.test.region).expect("an STS");
            let minter = StsMinter::new(
                UreqTransport::new(CALL_LIMIT),
                SystemClock,
                sts,
                RoleArn::new(ROLE).expect("a role"),
                self.root_credential(),
            );
            let credential = minter
                .mint(&self.request(ttl))
                .expect("the store's STS mints");
            let expires = credential
                .expires_at
                .duration_since(UNIX_EPOCH)
                .expect("later");
            LogGrant {
                access_key_id: credential.access_key_id,
                secret_access_key: credential.secret_access_key,
                session_token: credential.session_token,
                expires_at_ms: u64::try_from(expires.as_millis()).expect("ms"),
            }
        }

        fn root_credential(&self) -> Credential {
            Credential {
                access_key_id: self.test.access_key_id.clone(),
                secret_access_key: self.test.secret_access_key.clone(),
                session_token: None,
                expires_at: UNIX_EPOCH + NEVER,
            }
        }

        /// The settings of the agent's runtime: the proxy, the rig's clock and cell, manual
        /// wakes, and chunks of [`CHUNK`] bytes cut as soon as they are seen.
        fn logs(&self) -> LiveLogs {
            let made = self.wakes.clone();
            LiveLogs {
                file_cap: 64 << 20,
                grant: self.grant.clone(),
                transport: Arc::new(self.proxy.clone()),
                clock: self.clock.clone(),
                wakes: Arc::new(move || {
                    let wake = ManualWake::new();
                    lock(&made).push(wake.clone());
                    wake as Arc<dyn Wake>
                }),
                limits: Limits {
                    chunk: CHUNK,
                    flush: Duration::ZERO,
                    ..Limits::default()
                },
            }
        }

        /// The job `job_id` of the host: its sink under the host's prefix, and two empty
        /// capture files.
        fn job(&self, job_id: &str) -> Job {
            let host = self.request(SHORTEST).host_prefix().expect("a host prefix");
            let prefix =
                JobPrefix::new(host.as_str().trim_end_matches('/'), job_id).expect("a job prefix");
            let [stdout, stderr] = ["stdout", "stderr"].map(|stream| {
                let path = self.dir.path().join(format!("{job_id}.{stream}"));
                File::create(&path).expect("a capture file");
                path
            });
            Job {
                sink: Sink::new(self.endpoint.clone(), prefix.clone()),
                prefix,
                stdout,
                stderr,
            }
        }

        /// Starts the uploader of `job` and waits for its first step.
        fn start(&self, job: &Job) -> (Uploader, Arc<ManualWake>) {
            let uploader = Uploader::start(&job.sink, (&job.stdout, &job.stderr), &self.logs())
                .expect("the uploader starts");
            let wake = lock(&self.wakes).last().cloned().expect("a wake");
            assert_eq!(wake.settled(), Settled::Asleep);
            (uploader, wake)
        }

        /// The chunks of `stream` of `job` the store holds, by offset, read with the root key.
        fn chunks(&self, job: &JobPrefix, stream: Stream) -> BTreeMap<u64, Vec<u8>> {
            let root = self.root();
            let prefix = job.stream_prefix(stream);
            let page = root.list(&prefix, None, 1000).expect("list");
            page.entries
                .iter()
                .map(|entry| {
                    let name = entry.key.strip_prefix(&prefix).expect("under the prefix");
                    let offset = name.parse::<u64>().expect("a chunk, not a marker");
                    (offset, self.read(&entry.key).expect("listed"))
                })
                .collect()
        }

        /// The object `key`, whole, read with the root key; `None` when there is none.
        fn read(&self, key: &str) -> Option<Vec<u8>> {
            match self.root().get_range(key, 0, 1 << 20) {
                Ok(object) => object.map(|object| object.bytes),
                Err(js_logstore::StoreError::OutOfRange) => Some(Vec::new()),
                Err(other) => panic!("a read of {key}: {other}"),
            }
        }

        /// Every key the store holds under `job`.
        fn keys(&self, job: &JobPrefix) -> Vec<String> {
            let page = self.root().list(job.as_str(), None, 1000).expect("list");
            page.entries.into_iter().map(|entry| entry.key).collect()
        }
    }

    /// The stream the chunks make, read in offset order; each chunk must begin where the one
    /// before it ended, so a gap or an overlap fails here.
    fn whole(chunks: &BTreeMap<u64, Vec<u8>>) -> Vec<u8> {
        let mut stream = Vec::new();
        for (offset, bytes) in chunks {
            assert_eq!(*offset, stream.len() as u64, "a chunk begins at {offset}");
            stream.extend_from_slice(bytes);
        }
        stream
    }

    /// Appends `bytes` to the capture file `path`, as the capture does.
    fn append(path: &PathBuf, bytes: &[u8]) {
        let mut file = OpenOptions::new().append(true).open(path).expect("open");
        file.write_all(bytes).expect("append");
    }

    /// The bytes `0, 1, ..., 255, 0, 1, ...`, `len` of them.
    fn every_byte(len: usize) -> Vec<u8> {
        (0..=255u8).cycle().take(len).collect()
    }

    /// Steps the uploader of `wake` until it has ended.
    fn run_out(wake: &ManualWake) {
        while wake.step() != Settled::Ended {}
    }

    /// Milliseconds since the Unix epoch at `at`.
    fn ms(at: SystemTime) -> u64 {
        u64::try_from(at.duration_since(UNIX_EPOCH).expect("later").as_millis()).expect("ms")
    }

    /// The server, as far as the host's credential goes: it welcomes the session, records the
    /// `log_grant_state` of every heartbeat, and answers a heartbeat that reports `stale` with
    /// the next credential of its queue (the first is the host's first credential, each later
    /// one a renewal), and any other with none.
    struct Server {
        grants: VecDeque<LogGrant>,
        states: Vec<Value>,
    }

    impl AgentTransport for Server {
        fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
            if path == HELLO_PATH {
                return welcome(1);
            }
            let state = body["log_grant_state"].clone();
            let mut reply = json!({ "commands": [], "result_acks": [], "taint_ack": false });
            if state["stale"] == json!(true)
                && let Some(grant) = self.grants.pop_front()
            {
                reply["log_grant"] = serde_json::to_value(grant).expect("a grant");
            }
            self.states.push(state);
            Ok(Answer {
                status: 200,
                body: reply,
                retry_after: None,
            })
        }

        fn adopt_credential(&mut self, _secret: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// The `log_grant_state` of a heartbeat: `stale`, and the expiry of the credential held.
    fn state(stale: bool, until: Option<&LogGrant>) -> Value {
        match until {
            Some(grant) => json!({ "stale": stale, "until_ms": grant.expires_at_ms }),
            None => json!({ "stale": stale }),
        }
    }

    /// Proves the bytes a job writes are what the store holds, chunk by chunk under the keys the
    /// layout names, while the job runs, and then as its finals: 10,000 bytes of every value cut
    /// at 0, 4096 and 8192, a line on stderr, a later line as the next chunk at the offset where
    /// the last ended, each chunk with the time it was first seen; at the end the report has
    /// the totals and nothing dropped, the finals are each whole stream, and the chunks are
    /// deleted. Catches a chunk key the agent writes other than the layout names, a body or an
    /// offset that does not match the bytes written, and a request the store refuses.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_jobs_output_is_stored_as_chunks_and_then_as_finals_byte_for_byte() {
        let rig = Rig::new("s3-bytes");
        rig.grant.replace(&rig.mint(SHORTEST));
        let job = rig.job("job-bytes");
        let (uploader, wake) = rig.start(&job);
        let head = [b"line one\n".to_vec(), every_byte(10_000)].concat();
        let err = b"warning: on stderr\n".to_vec();
        append(&job.stdout, &head);
        append(&job.stderr, &err);
        let seen_at = ms(js_logstore::Clock::now(&rig.clock));
        assert_eq!(wake.step(), Settled::Asleep);

        let chunks = rig.chunks(&job.prefix, Stream::Stdout);
        assert_eq!(chunks.keys().copied().collect::<Vec<_>>(), [0, 4096, 8192]);
        assert!(
            whole(&chunks) == head,
            "the stdout chunks are not what was written"
        );
        assert_eq!(whole(&rig.chunks(&job.prefix, Stream::Stderr)), err);
        let first = rig
            .root()
            .get_range(&job.prefix.chunk_key(Stream::Stdout, 4096), 0, 1)
            .expect("read")
            .expect("there");
        assert_eq!(first.metadata[FIRST_BYTE_AT_MS], seen_at.to_string());

        let tail = b"the last line\n".to_vec();
        append(&job.stdout, &tail);
        assert_eq!(wake.step(), Settled::Asleep);
        let out = [head, tail].concat();
        let chunks = rig.chunks(&job.prefix, Stream::Stdout);
        assert!(chunks.contains_key(&10_009), "{:?}", chunks.keys());
        assert!(
            whole(&chunks) == out,
            "the stdout chunks are not what was written"
        );

        let totals = (out.len() as u64, err.len() as u64);
        assert_eq!(
            uploader.finish(totals),
            LogReport {
                stdout_bytes: totals.0,
                stderr_bytes: totals.1,
                stdout_dropped: 0,
                stderr_dropped: 0,
            }
        );
        run_out(&wake);
        let finals = [Stream::Stdout, Stream::Stderr].map(|s| job.prefix.final_key(s));
        assert!(rig.read(&finals[0]) == Some(out), "the stdout final");
        assert_eq!(rig.read(&finals[1]), Some(err));
        let [out_final, err_final] = finals;
        assert_eq!(
            rig.keys(&job.prefix),
            [err_final, out_final],
            "the finals alone, in key order: the chunks are deleted"
        );
    }

    /// Proves a renewal delivered in a heartbeat reply is what the next put is signed with: the
    /// first heartbeat reports no credential and its reply brings one the store's STS minted
    /// with the shortest lifetime it issues; a chunk goes out signed with it. The uploaders'
    /// clock is then moved to two minutes before that credential's expiry (13 minutes on, inside
    /// the store's window), the next heartbeat reports it stale with its expiry, and its reply
    /// brings a renewal; the next heartbeat reports the renewal held and not stale, and the
    /// next chunk is signed with the renewal's key, taken by the store, and follows the first
    /// with no gap. Catches a renewal the session does not put in the cell, an uploader that
    /// keeps signing with the old key, and a stale report that never goes out.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_renewal_in_a_heartbeat_reply_signs_the_next_put() {
        let rig = Rig::new("s3-renewal");
        let (first, renewal) = (rig.mint(SHORTEST), rig.mint(RENEWED));
        let mut server = Server {
            grants: VecDeque::from([first.clone(), renewal.clone()]),
            states: Vec::new(),
        };
        let mut session =
            Session::new(hello(&rig.host), &mut server, lowest()).with_log_grant(rig.grant.clone());
        session.tick();
        session.tick();
        assert_eq!(rig.grant.generation(), 1);

        let job = rig.job("job-renewal");
        let (uploader, wake) = rig.start(&job);
        append(&job.stdout, b"before the renewal\n");
        assert_eq!(wake.step(), Settled::Asleep);

        let stale_at = UNIX_EPOCH + Duration::from_millis(first.expires_at_ms) - GRANT_STALE_MARGIN;
        let now = js_logstore::Clock::now(&rig.clock);
        rig.clock
            .advance(stale_at.duration_since(now).expect("ahead"));
        session.tick();
        session.tick();
        assert_eq!(rig.grant.generation(), 2);

        append(&job.stdout, b"after the renewal\n");
        assert_eq!(wake.step(), Settled::Asleep);
        drop(session);
        assert_eq!(
            server.states,
            [
                state(true, None),
                state(true, Some(&first)),
                state(false, Some(&renewal)),
            ]
        );
        let signers: Vec<String> = rig.proxy.puts().into_iter().map(|p| p.key_id).collect();
        assert_eq!(signers, [first.access_key_id, renewal.access_key_id]);
        let chunks = rig.chunks(&job.prefix, Stream::Stdout);
        assert_eq!(whole(&chunks), b"before the renewal\nafter the renewal\n");
        drop(uploader);
        assert_eq!(wake.step(), Settled::Ended);
    }

    /// What a test of a put that fails once checks, for the fault `fault`: the first chunk's put
    /// meets the fault, the store holds `stored_after_fault` of it, the retry after the first
    /// backoff puts the very key and bytes again, the next chunk begins where the first ended,
    /// and the final is the whole stream; the stream's chunks never overlap or leave a hole.
    fn fails_once(name: &str, fault: Fault, stored_after_fault: Option<&[u8]>) {
        let rig = Rig::new(name);
        rig.grant.replace(&rig.mint(SHORTEST));
        let job = rig.job("job-retry");
        let (uploader, wake) = rig.start(&job);
        let first = b"the first chunk\n";
        rig.proxy.fail_next(fault);
        append(&job.stdout, first);
        assert_eq!(wake.step(), Settled::Asleep);
        let key = job.prefix.chunk_key(Stream::Stdout, 0);
        assert_eq!(rig.read(&key).as_deref(), stored_after_fault);

        rig.clock.advance(BACKOFF_FIRST);
        assert_eq!(wake.step(), Settled::Asleep);
        let puts = rig.proxy.puts();
        assert_eq!(puts.len(), 2, "one put and its retry: {puts:?}");
        assert_eq!(puts[0], puts[1], "the retry puts the same key and bytes");
        assert_eq!(rig.read(&key).as_deref(), Some(first.as_slice()));

        let second = b"the second chunk\n";
        append(&job.stdout, second);
        assert_eq!(wake.step(), Settled::Asleep);
        let out = [first.as_slice(), second].concat();
        assert_eq!(whole(&rig.chunks(&job.prefix, Stream::Stdout)), out);
        uploader.finish((out.len() as u64, 0));
        run_out(&wake);
        assert_eq!(rig.read(&job.prefix.final_key(Stream::Stdout)), Some(out));
    }

    /// Proves a put whose request never reached the store is put again, the same key and bytes,
    /// after the backoff, and the stream goes on with nothing lost or repeated. Catches a cut
    /// dropped after a failed put, and a retry that reads the file from another offset.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_put_the_store_never_saw_is_retried_without_a_gap() {
        fails_once("s3-drop", Fault::Drop, None);
    }

    /// Proves a put the store took, whose answer was lost, is put again with the same key and
    /// bytes, so the store holds the chunk once, as it was, and the stream goes on from its end.
    /// Catches a retry that writes the chunk under another key or offset, which a reader would
    /// read twice.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_put_whose_answer_was_lost_is_retried_without_a_repeat() {
        fails_once("s3-lost", Fault::LoseAnswer, Some(b"the first chunk\n"));
    }

    /// What a test of a credential the store will not take checks: the host holds `held` (the
    /// first heartbeat's reply brought it), the first put is refused, the next heartbeat reports
    /// the credential stale, its reply brings a renewal the store's STS minted, and the same
    /// chunk is put again with it at once, so the stream has no gap and no repeat.
    fn renewed_after_a_refusal(
        name: &str,
        held: impl FnOnce(&Rig) -> LogGrant,
        fault: Option<Fault>,
    ) {
        let rig = Rig::new(name);
        let held = held(&rig);
        let renewal = rig.mint(RENEWED);
        let mut server = Server {
            grants: VecDeque::from([held.clone(), renewal.clone()]),
            states: Vec::new(),
        };
        let mut session =
            Session::new(hello(&rig.host), &mut server, lowest()).with_log_grant(rig.grant.clone());
        session.tick();
        session.tick();

        let job = rig.job("job-refused");
        let (uploader, wake) = rig.start(&job);
        if let Some(fault) = fault {
            rig.proxy.fail_next(fault);
        }
        let first = b"put again with the renewal\n";
        append(&job.stdout, first);
        assert_eq!(wake.step(), Settled::Asleep);
        let key = job.prefix.chunk_key(Stream::Stdout, 0);
        assert_eq!(rig.read(&key), None, "the refused put stored nothing");

        session.tick();
        assert_eq!(rig.grant.generation(), 2, "the renewal is held");
        assert_eq!(wake.step(), Settled::Asleep);
        let second = b"and the stream goes on\n";
        append(&job.stdout, second);
        assert_eq!(wake.step(), Settled::Asleep);
        drop(session);
        assert_eq!(
            server.states,
            [state(true, None), state(true, Some(&held))],
            "the refusal is reported stale"
        );
        let puts = rig.proxy.puts();
        let signers: Vec<&str> = puts.iter().map(|put| put.key_id.as_str()).collect();
        let renewed = renewal.access_key_id.as_str();
        assert_eq!(signers, [held.access_key_id.as_str(), renewed, renewed]);
        assert_eq!(puts[0].key, puts[1].key, "the refused chunk is put again");
        let out = [first.as_slice(), second].concat();
        assert_eq!(whole(&rig.chunks(&job.prefix, Stream::Stdout)), out);
        uploader.finish((out.len() as u64, 0));
        run_out(&wake);
        assert_eq!(rig.read(&job.prefix.final_key(Stream::Stdout)), Some(out));
    }

    /// Proves a credential the store no longer takes (its session token is not one the store
    /// issued, as after a revocation) is reported stale in the next heartbeat on the store's own
    /// refusal, and the upload goes on with the renewal that heartbeat's reply brings. Catches a
    /// store's refusal of the token read as a refusal to retry with the same credential, which
    /// stops the upload until the credential's expiry nears.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_credential_the_store_refuses_is_renewed_and_the_upload_goes_on() {
        let revoked = |rig: &Rig| {
            let mut grant = rig.mint(SHORTEST);
            grant.session_token = grant.session_token.map(|token| format!("{token}x"));
            grant
        };
        renewed_after_a_refusal("s3-revoked", revoked, None);
    }

    /// Proves a credential the store answers as expired (the explicit expiry step: the proxy
    /// gives, in the store's place, the answer to a temporary key past its expiry) is reported
    /// stale in the next heartbeat, and the upload goes on with the renewal that heartbeat's
    /// reply brings: for the test store's own answer (403 `InvalidAccessKeyId`: it forgets an
    /// expired temporary key) and for S3's (400 `ExpiredToken`). Catches an uploader that keeps
    /// retrying the dead key, or gives up, instead of asking for and taking the renewal.
    #[test]
    #[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
    fn a_credential_the_store_answers_as_expired_is_renewed_and_the_upload_goes_on() {
        let answers = [(403, "InvalidAccessKeyId"), (400, "ExpiredToken")];
        for (status, code) in answers {
            let expired = Fault::Answer(status, code);
            renewed_after_a_refusal(
                &format!("s3-{code}"),
                |rig| rig.mint(SHORTEST),
                Some(expired),
            );
        }
    }
}
