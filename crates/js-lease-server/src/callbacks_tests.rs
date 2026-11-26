//! The delivery of jobs' completion callbacks against loopback receivers (`callbacks_receiver.rs`):
//! the job tables on a manual store clock, a sender that admits loopback and resolves every
//! name itself, and a recorder that counts the answers. Every pass runs on the test's thread,
//! but for the threads' own test, which waits on the passes they count.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_callback::{AddressRule, ResolveError, SigningKey, sign};
use js_jobs::interfere::Interfere;
use js_jobs::{
    CallbackKey, CallbackState, CallbackStatus, InstanceId, JobCallback, JobId, JobSpec, NewJob,
    Retention,
};
use js_state::fake::Fake;
use js_state::{ManualClock as StoreClock, StoreTime};

use super::receiver::{Authority, HOST, Loopback, Receiver, status};
use super::*;
use crate::agent_api::job_rig::{START_MS, spec, tables_on};
use crate::api::rig::{Lines, process_sink};
use crate::metrics::{NoMetrics, Recorder};

/// The signing key every test's callback carries.
fn key() -> SigningKey {
    SigningKey::new(b"delivery-key-0123456789-abcdefgh".to_vec()).expect("a key")
}

/// The job tables on a manual clock, and what the delivery counts on.
struct Rig {
    clock: Arc<StoreClock>,
    store: Arc<Interfere<Fake>>,
    jobs: Arc<ServerJobs>,
    recorder: Arc<Recorder>,
}

impl Rig {
    fn new() -> Rig {
        let clock = Arc::new(StoreClock::default());
        clock.advance(Duration::from_millis(START_MS));
        let (store, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
        Rig {
            clock,
            store,
            jobs: Arc::new(jobs),
            recorder: Arc::new(Recorder::default()),
        }
    }

    /// Delivery from these tables as `server-a`, with `sender`, counting on the rig's recorder.
    fn delivery(&self, sender: Sender) -> Delivery {
        let meter = Meter::new(self.recorder.clone(), Arc::new(NoMetrics));
        self.delivery_metered(sender, meter)
    }

    /// Delivery from these tables as `server-a`, with `sender`, counting with `meter`.
    fn delivery_metered(&self, sender: Sender, meter: Meter) -> Delivery {
        let by = InstanceId::new("server-a");
        Delivery::new(Arc::clone(&self.jobs), meter, sender, by)
    }

    /// Records the job `id` with a callback to `url`, and ends it by a cancel, so its callback
    /// is due now.
    fn ended(&self, id: &str, url: &str) -> JobId {
        let callback = JobCallback {
            url: url.to_owned(),
            key: CallbackKey::Key(key()),
            deadline_seconds: Some(3600),
        };
        let spec = JobSpec {
            callback: Some(callback),
            ..spec()
        };
        let new = NewJob {
            id: Some(JobId::parse(id).expect("an id")),
            spec,
            priority: 0,
            client: None,
        };
        let id = self.jobs.insert_queued(new).expect("records").record.id;
        self.jobs.request_cancel(&id).expect("cancels");
        id
    }

    /// Where the callback of `id` stands.
    fn status(&self, id: &JobId) -> CallbackStatus {
        let status = self.jobs.callback_status(id).expect("reads");
        status.expect("a callback")
    }

    /// The store's clock now.
    fn now(&self) -> StoreTime {
        self.jobs.now().expect("the clock")
    }

    /// The answers counted: delivered, abandoned, failed and refused-address.
    fn counted(&self) -> [u64; 4] {
        self.recorder.callbacks()
    }
}

/// The counts after `delivered`, `abandoned`, `failed` and `refused` answers.
fn counts(delivered: u64, abandoned: u64, failed: u64, refused: u64) -> [u64; 4] {
    [delivered, abandoned, failed, refused]
}

/// Captures the log lines of this thread, without times, until the guard is dropped.
fn capture() -> (Lines, tracing::subscriber::DefaultGuard) {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (lines, tracing::subscriber::set_default(subscriber))
}

/// The log lines of `lines` from this crate's delivery and the sender, the rest (a TLS
/// library's, say) left out.
fn delivery_lines(lines: &Lines) -> String {
    let text = lines.take();
    let ours = text.lines().filter(|line| {
        line.contains(" js_lease_server::callbacks: ") | line.contains(" js_callback::")
    });
    ours.map(|line| format!("{line}\n")).collect()
}

/// The request line of `head` and its header lines, sorted.
fn parsed(head: &str) -> (String, Vec<String>) {
    let whole = head.strip_suffix("\r\n\r\n").expect("a whole head");
    let mut lines = whole.split("\r\n").map(str::to_owned);
    let first = lines.next().expect("a request line");
    let mut headers: Vec<String> = lines.collect();
    headers.sort();
    (first, headers)
}

/// The `webhook-id` header of the request head `head`.
fn webhook_id(head: &str) -> String {
    let line = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("webhook-id: "));
    line.expect("a webhook-id").to_owned()
}

/// Asserts that a pass with nothing due claims nothing and calls nobody; that a pass after the
/// job's end sends its callback once, as exactly a bare POST to the job's URL with the three
/// Standard Webhooks headers, its timestamp the claim's instant on the store's clock in seconds
/// and its signature the key's over the `webhook-id` and that timestamp; and that the 2xx is
/// recorded (delivered, one try, delivered at the answer's instant), counted under
/// `delivered`, and logged by job and result alone; and that the next pass finds nothing.
/// Catches a callback sent before its job ends, sent twice, signed over another timestamp, an
/// answer not recorded or not counted, and a log line that names the URL's path or the key.
#[test]
fn a_due_callback_is_sent_once_signed_and_recorded_delivered() {
    let rig = Rig::new();
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(204)]);
    let delivery = rig.delivery(authority.sender(AddressRule::new([]), Loopback::new()));
    let (lines, _guard) = capture();
    assert!(!delivery.pass(), "nothing is due");
    let id = rig.ended("dl-1", &receiver.url("/js-done/ref-9?k=v"));
    assert_eq!(rig.status(&id).state, CallbackState::Pending);
    let sent_at = rig.now();
    assert!(delivery.pass(), "the due callback");
    let heads = receiver.heads();
    assert_eq!(heads.len(), 1, "{heads:?}");
    let id_header = webhook_id(&heads[0]);
    let timestamp = sent_at.as_duration().as_secs();
    let expected = vec![
        "content-length: 0".to_owned(),
        format!("host: {HOST}:{}", receiver.port),
        format!("webhook-id: {id_header}"),
        format!("webhook-signature: {}", sign(&key(), &id_header, timestamp)),
        format!("webhook-timestamp: {timestamp}"),
    ];
    let request = "POST /js-done/ref-9?k=v HTTP/1.1".to_owned();
    assert_eq!(parsed(&heads[0]), (request, expected));
    // The answer is recorded at the store's clock after the send, which the fake store's
    // commits have moved on from the claim's instant.
    let delivered = rig.status(&id);
    let answered_at = delivered.delivered_at.expect("delivered at");
    assert!(answered_at > sent_at, "{delivered:?}");
    assert_eq!(
        delivered,
        CallbackStatus {
            state: CallbackState::Delivered,
            tries: 1,
            last_try_at: Some(sent_at),
            last_failure: None,
            delivered_at: Some(answered_at),
            deadline_at: delivered.deadline_at,
        }
    );
    assert!(!delivery.pass(), "nothing more is due");
    assert_eq!(receiver.heads(), Vec::<String>::new());
    assert_eq!(rig.counted(), counts(1, 0, 0, 0));
    assert_eq!(
        delivery_lines(&lines),
        " INFO js_lease_server::callbacks: a callback was tried job=dl-1 result=\"delivered\"\n"
    );
}

/// Asserts that a 500 is recorded as a failure (still pending, `failed` its last failure),
/// that the next try waits the first wait (5 s from the answer: none after 4 s, one after 5 s), and that a 410 then abandons it;
/// each counted under its result, and the 500 logged for the operator with the URL's host and
/// the status, never its path. Catches a failure tried again at once, a 410 tried again, and a
/// count under the wrong result.
#[test]
fn a_failure_waits_its_turn_and_a_410_abandons() {
    let rig = Rig::new();
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(500), status(410)]);
    let delivery = rig.delivery(authority.sender(AddressRule::new([]), Loopback::new()));
    let (lines, _guard) = capture();
    let id = rig.ended("fl-1", &receiver.url("/secret-path"));
    let tried_at = rig.now();
    assert!(delivery.pass());
    let failed = rig.status(&id);
    assert_eq!(
        (failed.state, failed.tries, failed.last_failure),
        (CallbackState::Pending, 1, Some(FailureClass::Failed))
    );
    assert_eq!(failed.last_try_at, Some(tried_at));
    rig.clock.advance(Duration::from_secs(4));
    assert!(!delivery.pass(), "the first wait has not passed");
    rig.clock.advance(Duration::from_secs(1));
    assert!(delivery.pass(), "the first wait has passed");
    assert_eq!(rig.status(&id).state, CallbackState::Abandoned);
    assert_eq!(receiver.heads().len(), 2);
    assert_eq!(rig.counted(), counts(0, 1, 1, 0));
    assert_eq!(
        delivery_lines(&lines),
        format!(
            " WARN js_callback::sender: a callback was answered with neither 2xx nor 410 host=\"{HOST}\" status=500\n INFO js_lease_server::callbacks: a callback was tried job=fl-1 result=\"failed\"\n INFO js_lease_server::callbacks: a callback was tried job=fl-1 result=\"abandoned\"\n"
        )
    );
}

/// Asserts that a receiver whose name resolves to the metadata address, or to an address of
/// the cluster's own (one the rule learned after the sender was built), is never connected to:
/// the receiver reads nothing, and each try is recorded with the class `refused-address` and
/// counted under it. Catches a call that goes inward, and a refusal counted as a plain failure.
#[test]
fn an_address_the_rule_refuses_is_never_called() {
    let rig = Rig::new();
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(200)]);
    let cluster = AddressRule::new([]);
    let metadata = IpAddr::from([169, 254, 169, 254]);
    let own = IpAddr::from([10, 0, 0, 9]);
    for (name, address) in [("rf-metadata", metadata), ("rf-own", own)] {
        let sender = authority.sender(cluster.clone(), Loopback(address));
        cluster.add_cluster_address(own);
        let id = rig.ended(name, &receiver.url("/x"));
        assert!(rig.delivery(sender).pass(), "{name}");
        let status = rig.status(&id);
        assert_eq!(
            (status.state, status.last_failure),
            (CallbackState::Pending, Some(FailureClass::RefusedAddress)),
            "{name}"
        );
    }
    assert_eq!(receiver.heads(), Vec::<String>::new());
    assert_eq!(rig.counted(), counts(0, 0, 0, 2));
}

/// Asserts that a callback whose URL this build's submit would refuse (here plain http,
/// recorded past the submit) is never sent: the try is recorded as a failure, counted, and
/// logged with the job and the fault's word alone. Catches such a URL handed to the sender, and
/// a log line that repeats it.
#[test]
fn a_url_this_build_refuses_is_not_sent() {
    let rig = Rig::new();
    let authority = Authority::new();
    let delivery = rig.delivery(authority.sender(AddressRule::new([]), Loopback::new()));
    let (lines, _guard) = capture();
    let id = rig.ended("rf-url", "http://hooks.test/secret-path");
    assert!(delivery.pass());
    let status = rig.status(&id);
    assert_eq!(
        (status.state, status.last_failure),
        (CallbackState::Pending, Some(FailureClass::Failed))
    );
    assert_eq!(rig.counted(), counts(0, 0, 1, 0));
    assert_eq!(
        delivery_lines(&lines),
        " WARN js_lease_server::callbacks: a callback whose URL this build refuses is not sent job=rf-url code=\"not-https\"\n INFO js_lease_server::callbacks: a callback was tried job=rf-url result=\"failed\"\n"
    );
}

/// Asserts that when a try's claim lapses during the send and another server claims the
/// callback, the first server's answer records nothing and counts nothing: the row stays the
/// other server's, its tries counted, and it is logged as lapsed. Catches an answer written
/// over another server's claim.
#[test]
fn an_answer_whose_claim_lapsed_records_nothing() {
    let rig = Rig::new();
    let authority = Authority::new();
    let (clock, jobs) = (Arc::clone(&rig.clock), Arc::clone(&rig.jobs));
    let taken = Arc::new(Mutex::new(0));
    let took = Arc::clone(&taken);
    let lapse = move || {
        clock.advance(Duration::from_secs(30));
        let now = jobs.now().expect("the clock");
        let by = InstanceId::new("server-b");
        let claims = jobs.claim_callbacks(&by, now, 1).expect("claims");
        *took.lock().unwrap_or_else(PoisonError::into_inner) = claims.len();
    };
    let receiver = Receiver::hooked(&authority, &[status(200)], Box::new(lapse));
    let delivery = rig.delivery(authority.sender(AddressRule::new([]), Loopback::new()));
    let (lines, _guard) = capture();
    let id = rig.ended("lp-1", &receiver.url("/x"));
    assert!(delivery.pass());
    assert_eq!(*taken.lock().unwrap_or_else(PoisonError::into_inner), 1);
    let status = rig.status(&id);
    assert_eq!((status.state, status.tries), (CallbackState::Pending, 2));
    assert_eq!(rig.counted(), counts(0, 0, 0, 0));
    assert_eq!(
        delivery_lines(&lines),
        " INFO js_lease_server::callbacks: a callback's claim lapsed before its answer was recorded job=lp-1 result=\"delivered\"\n"
    );
}

/// Asserts that a pass whose store cannot read its clock claims nothing and says so, and that
/// an answer the store fails to write is logged, not counted, and leaves the callback to be
/// tried again once its claim lapses (30 s), when it is delivered. Catches a failure that ends
/// the delivery, and a delivery counted that the store never recorded.
#[test]
fn a_store_that_fails_loses_no_callback() {
    let rig = Rig::new();
    let authority = Authority::new();
    let store = Arc::clone(&rig.store);
    let fails = Arc::new(Mutex::new(true));
    let failing = Arc::clone(&fails);
    let hook = move || {
        let mut first = failing.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut *first) {
            store.fail_next();
        }
    };
    let receiver = Receiver::hooked(&authority, &[status(200)], Box::new(hook));
    let delivery = rig.delivery(authority.sender(AddressRule::new([]), Loopback::new()));
    let (lines, _guard) = capture();
    let id = rig.ended("sf-1", &receiver.url("/x"));
    rig.store.fail_clock(true);
    assert!(!delivery.pass(), "no clock, no claim");
    rig.store.fail_clock(false);
    assert!(delivery.pass(), "the answer is not recorded");
    assert_eq!(rig.status(&id).state, CallbackState::Pending);
    assert_eq!(rig.counted(), counts(0, 0, 0, 0));
    rig.clock.advance(Duration::from_secs(29));
    assert!(!delivery.pass(), "the claim holds");
    rig.clock.advance(Duration::from_secs(1));
    assert!(delivery.pass(), "the claim lapsed");
    assert_eq!(rig.status(&id).state, CallbackState::Delivered);
    assert_eq!(receiver.heads().len(), 2);
    assert_eq!(rig.counted(), counts(1, 0, 0, 0));
    assert_eq!(
        delivery_lines(&lines),
        " WARN js_lease_server::callbacks: the due callbacks could not be claimed error=state store failure: the commit failed (injected)\n WARN js_lease_server::callbacks: a callback's answer was not recorded: it is tried again when its claim lapses job=sf-1 result=\"delivered\" error=state store failure: the commit failed (injected)\n INFO js_lease_server::callbacks: a callback was tried job=sf-1 result=\"delivered\"\n"
    );
}

impl Deliverers {
    /// Waits until the threads have made `passes` passes in all.
    fn wait_passes(&self, passes: u64) {
        let turns = self.gate.lock();
        let waited = self
            .gate
            .changed
            .wait_while(turns, |turns| turns.passes < passes);
        drop(waited.unwrap_or_else(PoisonError::into_inner));
    }

    /// Ends the wait of a thread that waits.
    fn wake(&self) {
        self.gate.lock().woken = true;
        self.gate.changed.notify_all();
    }
}

/// Asserts that the delivery threads try a callback that became due while they waited, once
/// woken, and look again at once after a pass that claimed one; and that their stop returns
/// once each has ended. The wait a thread makes when nothing is due is an hour here, so only
/// the wake and the stop end it. The threads count on the meter of a server without metrics,
/// as such a server's do, and count nothing. Catches threads that never look again, that wait
/// after a pass that claimed, and a stop that does not end them.
#[test]
fn the_threads_deliver_what_becomes_due_and_stop() {
    let rig = Rig::new();
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[status(200)]);
    let sender = authority.sender(AddressRule::new([]), Loopback::new());
    let delivery = rig.delivery_metered(sender, Meter::off().clone());
    let threads = Deliverers::start(Arc::new(delivery), 1, Duration::from_secs(3600));
    threads.wait_passes(1);
    let id = rig.ended("th-1", &receiver.url("/x"));
    threads.wake();
    let head = receiver.next_head();
    assert!(head.starts_with("POST /x HTTP/1.1\r\n"), "{head}");
    // The pass that claimed is followed at once by one that finds nothing.
    threads.wait_passes(3);
    assert_eq!(rig.status(&id).state, CallbackState::Delivered);
    threads.stop();
    assert_eq!(rig.counted(), counts(0, 0, 0, 0));
}

/// A resolver of fixed answers by name; a name it does not hold does not resolve.
#[derive(Debug)]
struct Named(Vec<(&'static str, Vec<IpAddr>)>);

impl Resolve for Named {
    fn resolve(&self, host: &str, within: Duration) -> Result<Vec<IpAddr>, ResolveError> {
        assert_eq!(within, SEND_LIMIT);
        let found = self.0.iter().find(|(name, _)| *name == host);
        found
            .map(|(_, addresses)| addresses.clone())
            .ok_or(ResolveError::TimedOut)
    }
}

/// Asserts that a store host given as an address is taken as written, a name is resolved to
/// every address it has, and a name that does not resolve adds nothing and is logged with the
/// name and why. Catches an address sent to the resolver, a name's later addresses dropped,
/// and a failed lookup that stops the rest.
#[test]
fn each_store_host_is_resolved_and_one_that_is_not_is_logged() {
    let (lines, _guard) = capture();
    let resolver = Named(vec![(
        "db.example",
        vec![IpAddr::from([10, 0, 0, 8]), "fd00::8".parse().expect("v6")],
    )]);
    let hosts = ["10.0.0.7", "gone.example", "db.example"].map(str::to_owned);
    assert_eq!(
        store_addresses(&hosts, &resolver),
        vec![
            IpAddr::from([10, 0, 0, 7]),
            IpAddr::from([10, 0, 0, 8]),
            "fd00::8".parse::<IpAddr>().expect("v6"),
        ]
    );
    assert_eq!(
        delivery_lines(&lines),
        " WARN js_lease_server::callbacks: a store host did not resolve: callbacks are not denied its addresses host=gone.example error=the lookup did not answer within its limit\n"
    );
}

/// Asserts that a server's start makes its listen addresses (an IPv4-mapped one as its IPv4
/// form) and its store hosts' addresses the cluster's own, denied to every callback while an
/// address of neither stays admitted; that it starts the delivery threads when the server holds
/// the job tables, which then stop; and that it starts none without them, the addresses denied
/// all the same. Catches a start that leaves the server's own or its store's address open to a
/// callback, and threads started with no tables to deliver from.
#[tokio::test]
async fn a_start_denies_the_servers_own_addresses_and_delivers_only_with_job_tables() {
    use crate::agent_api::job_rig::rig;
    let listening = ["10.1.2.3:8080", "[::ffff:10.1.2.4]:9090"].map(|at| at.parse().expect(at));
    for (name, tables) in [("start_tables", true), ("start_bare", false)] {
        let rig = rig(name, tables);
        let hosts = Some(vec!["10.0.0.7".to_owned()]);
        let started = start(&rig.agents, &listening, hosts, "server-a".to_owned()).await;
        for (text, admitted) in [
            ("10.1.2.3", false),
            ("10.1.2.4", false),
            ("10.0.0.7", false),
            ("10.1.2.5", true),
        ] {
            let address: IpAddr = text.parse().expect(text);
            assert_eq!(
                rig.agents.cluster.admits(address),
                admitted,
                "{name}: {text}"
            );
        }
        assert_eq!(started.is_some(), tables, "{name}");
        started.into_iter().for_each(Deliverers::stop);
    }
}
