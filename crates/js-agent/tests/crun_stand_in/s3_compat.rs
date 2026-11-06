//! A restart of an agent that uploads live logs to the S3-compatible test store with STS
//! (`js_testkit::s3`; CI's `test` job runs it in a service container), over the stand-in: the
//! agent's client, signer, key layout and progress record against the store's own checks, the
//! host's credential minted by the store's STS. A proxy in the test process stands between the
//! uploaders and the store, to record every put and to lose one answer.
//!
//! The test is `#[ignore]`d, with the reason, so the default `cargo test` never reaches for the
//! store; the `test` job runs it with `--ignored`. The uploaders' wakes are manual and their
//! clock is one the test moves, from the real time on (the store refuses a signing time more
//! than 15 minutes from its own); nothing waits for time.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent::crun::CrunRuntime;
use js_agent::lifecycle::{RunHandle, RunSpec, Runtime, Status};
use js_agent::logs::{HostGrant, Limits, LiveLogs, ManualWake, Settled, Sink, Wake};
use js_agent_wire::LogGrant;
use js_logmint::{Bucket, LogPrefix, MintRequest, Minter, RoleArn, StsEndpoint, StsMinter};
use js_logstore::{
    Credential, Endpoint, HttpRequest, HttpResponse, JobPrefix, ManualClock, Method, Store, Stream,
    SystemClock, Transport, TransportError, UreqTransport,
};
use js_testkit::s3::{TestStore, unique};

use crate::node::{LONG, Node, PID};

/// How long one request may take: the store is on this machine.
const CALL_LIMIT: Duration = Duration::from_secs(30);

/// The role every mint asks for; the session policy is what narrows the credential.
const ROLE: &str = "arn:aws:iam::000000000000:role/js-live-logs";

/// The shortest lifetime the test store's STS issues.
const SHORTEST: Duration = Duration::from_secs(900);

/// The root key's expiry: it has none.
const NEVER: Duration = Duration::from_secs(1 << 40);

/// The run: `-hold` makes the stand-in hold until it is stopped, writing nothing itself.
const RUN: &str = "r1-hold";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The path and the body of every put, in order.
type Puts = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// The transport between the uploaders and the store: it records the key and body of every
/// put, and loses the store's answer to the next put when told to.
#[derive(Clone)]
struct Proxy {
    store: Arc<UreqTransport>,
    puts: Puts,
    lose_next: Arc<Mutex<bool>>,
}

impl Transport for Proxy {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        if request.method != Method::Put {
            return self.store.send(request);
        }
        let path = request.url.split('?').next().unwrap_or_default().to_owned();
        lock(&self.puts).push((path, request.body.clone()));
        let answer = self.store.send(request);
        match std::mem::take(&mut *lock(&self.lose_next)) {
            true => {
                answer.expect("the store answers");
                Err(TransportError::Io(ErrorKind::ConnectionReset))
            }
            false => answer,
        }
    }
}

/// The test store, a log prefix and host of the test's own, the proxy, the uploaders' clock,
/// and the settings each agent process's runtime is given, whose uploaders' wakes are kept in
/// the order they were made.
struct Live {
    test: TestStore,
    endpoint: Endpoint,
    request: MintRequest,
    clock: Arc<ManualClock>,
    proxy: Proxy,
    wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
}

impl Live {
    fn new() -> Self {
        let test = TestStore::from_env();
        let endpoint =
            Endpoint::parse(&test.base_url, &test.region, &test.bucket, true).expect("an endpoint");
        let request = MintRequest {
            bucket: Bucket::new(&test.bucket).expect("a bucket"),
            prefix: LogPrefix::new(&unique("stand-in")).expect("a log prefix"),
            host: unique("host"),
            ttl: SHORTEST,
        };
        let proxy = Proxy {
            store: Arc::new(UreqTransport::new(CALL_LIMIT)),
            puts: Arc::default(),
            lose_next: Arc::default(),
        };
        Self {
            test,
            endpoint,
            request,
            clock: Arc::new(ManualClock::new(SystemTime::now())),
            proxy,
            wakes: Arc::default(),
        }
    }

    fn root_key(&self) -> Credential {
        Credential {
            access_key_id: self.test.access_key_id.clone(),
            secret_access_key: self.test.secret_access_key.clone(),
            session_token: None,
            expires_at: UNIX_EPOCH + NEVER,
        }
    }

    /// The settings of one agent process: a credential cell of its own, holding a credential
    /// the store's STS minted (as the first heartbeat's reply brings it), and the rig's
    /// proxy, clock and wakes. Chunks are cut as soon as they are seen.
    fn agent(&self) -> LiveLogs {
        let sts = StsEndpoint::parse(&self.test.base_url, &self.test.region).expect("an STS");
        let minter = StsMinter::new(
            UreqTransport::new(CALL_LIMIT),
            SystemClock,
            sts,
            RoleArn::new(ROLE).expect("a role"),
            self.root_key(),
        );
        let credential = minter.mint(&self.request).expect("the store's STS mints");
        let expires = credential
            .expires_at
            .duration_since(UNIX_EPOCH)
            .expect("later");
        let grant = Arc::new(HostGrant::new(self.clock.clone()));
        grant.replace(&LogGrant {
            access_key_id: credential.access_key_id,
            secret_access_key: credential.secret_access_key,
            session_token: credential.session_token,
            expires_at_ms: u64::try_from(expires.as_millis()).expect("ms"),
        });
        let made = self.wakes.clone();
        LiveLogs {
            file_cap: 2 << 20,
            grant,
            transport: Arc::new(self.proxy.clone()),
            clock: self.clock.clone(),
            wakes: Arc::new(move || {
                let wake = ManualWake::new();
                lock(&made).push(wake.clone());
                wake as Arc<dyn Wake>
            }),
            limits: Limits {
                flush: Duration::ZERO,
                ..Limits::default()
            },
        }
    }

    fn wake(&self, index: usize) -> Arc<ManualWake> {
        lock(&self.wakes)[index].clone()
    }

    /// The run's prefix, under the host's.
    fn prefix(&self) -> JobPrefix {
        let host = self.request.host_prefix().expect("a host prefix");
        JobPrefix::new(host.as_str().trim_end_matches('/'), RUN).expect("a job prefix")
    }

    /// A client of the store with its root key.
    fn root(&self) -> Store<UreqTransport, SystemClock> {
        Store::new(
            UreqTransport::new(CALL_LIMIT),
            SystemClock,
            self.endpoint.clone(),
            self.root_key(),
        )
    }

    /// The stream the chunks of stdout make, read with the root key in offset order; each
    /// chunk must begin where the one before it ended, so a gap or an overlap fails here.
    fn chunked_stdout(&self) -> Vec<u8> {
        let root = self.root();
        let prefix = self.prefix().stream_prefix(Stream::Stdout);
        let page = root.list(&prefix, None, 1000).expect("list");
        let mut stream = Vec::new();
        for entry in page.entries {
            let offset: u64 = entry.key[prefix.len()..].parse().expect("a chunk");
            assert_eq!(offset, stream.len() as u64, "a chunk begins at {offset}");
            let object = root.get_range(&entry.key, 0, 1 << 20).expect("read");
            stream.extend(object.expect("listed").bytes);
        }
        stream
    }
}

/// Appends `bytes` to the file `path`, as the capture does.
fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).expect("open");
    file.write_all(bytes).expect("append");
}

/// Stages `spec`, starts it, waits for its script to say it is up, and puts its init into its
/// cgroup: a run a restart can adopt.
fn held(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> RunHandle {
    let id = runtime.stage(spec).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    node.place(&id.0, PID);
    id
}

/// Proves an agent killed with a put in flight, and started again, goes on uploading with no
/// gap and no repeat: the first agent puts the first bytes as chunk 0, then puts the next as a
/// chunk the store takes but whose answer is lost, and is killed (its runtime dropped) before
/// it can retry; the job writes more while no agent runs. The restarted agent adopts the run,
/// makes its uploader from the progress record, puts the chunk that was in flight again, the
/// same key and bytes, and then what was written while it was down, from where that chunk
/// ends; the chunks read back in offset order are the stream, and at the end the final is the
/// whole stream. Catches a resume from an offset one past or one short of the recorded one,
/// and a chunk in flight put under another key, which a reader would read twice.
#[test]
#[ignore = "needs the S3-compatible test store: JS_TEST_S3_URL and JS_TEST_S3_REGION (CI's test job)"]
fn a_restarted_agent_goes_on_from_its_record_with_no_gap_and_no_repeat() {
    let node = Node::new("s3-resume");
    let live = Live::new();
    let mut first = node.open().with_live_logs(live.agent());
    let mut spec = node.job(RUN, 600_000);
    spec.log_sink = Some(Sink::new(live.endpoint.clone(), live.prefix()));
    let id = held(&node, &mut first, &spec);
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    let stdout = node.lease_of(&id.0).join("stdout");
    let before = b"written while the first agent ran\n";
    append(&stdout, before);
    assert_eq!(wake.step(), Settled::Asleep);
    let in_flight = b"put, and its answer lost\n";
    append(&stdout, in_flight);
    *lock(&live.proxy.lose_next) = true;
    assert_eq!(wake.step(), Settled::Asleep);
    drop(first);
    assert_eq!(wake.step(), Settled::Ended);
    let during = b"written while no agent ran\n";
    append(&stdout, during);

    let mut second = node.open().with_live_logs(live.agent());
    assert_eq!(second.swept().adopted, [RUN]);
    assert_eq!(second.poll(&id), Ok(Status::Running));
    let wake = live.wake(1);
    assert_eq!(wake.settled(), Settled::Asleep);
    assert_eq!(wake.step(), Settled::Asleep);
    let whole = [before.as_slice(), in_flight, during].concat();
    assert_eq!(live.chunked_stdout(), whole);
    let key = |offset: usize| live.prefix().chunk_key(Stream::Stdout, offset as u64);
    let puts: Vec<(String, Vec<u8>)> = lock(&live.proxy.puts).clone();
    let chunk_puts: Vec<(&str, &[u8])> = puts
        .iter()
        .filter(|(path, _)| path.contains("/stdout/"))
        .map(|(path, body)| (path.rsplit('/').next().unwrap_or_default(), body.as_slice()))
        .collect();
    let name = |offset: usize| {
        key(offset)
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let (at_in_flight, at_during) = (name(before.len()), name(before.len() + in_flight.len()));
    assert_eq!(
        chunk_puts,
        [
            (name(0).as_str(), before.as_slice()),
            (at_in_flight.as_str(), in_flight.as_slice()),
            (at_in_flight.as_str(), in_flight.as_slice()),
            (at_during.as_str(), during.as_slice()),
        ],
        "the chunk in flight is put again as it was, and the rest after it"
    );

    second.stop(&id, LONG).expect("stopped");
    assert!(matches!(second.poll(&id), Ok(Status::Exited(_))));
    second.remove(&id).expect("removed");
    while wake.step() != Settled::Ended {}
    let final_key = live.prefix().final_key(Stream::Stdout);
    let stored = live.root().get_range(&final_key, 0, 1 << 20).expect("read");
    assert_eq!(stored.map(|object| object.bytes), Some(whole));
}
