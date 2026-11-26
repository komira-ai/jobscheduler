//! The sender against loopback receivers (`receiver.rs`): every address comes from a fake
//! resolver or a literal, every receiver listens on `127.0.0.1` port 0 with a test authority's
//! certificate, and the only time limit a test reaches is one whose receiver never answers.

use std::io;
use std::net::{IpAddr, TcpListener};
use std::sync::{Arc, Mutex, PoisonError};

use super::receiver::{Authority, HOST, Receiver, url_of};
use super::*;
use crate::resolve::ResolveError;

/// The `webhook-id` every test sends: the one minted from the bytes 0 to 15.
const ID: &str = "AAECAwQFBgcICQoLDA0ODw";

/// The `webhook-timestamp` every test sends.
const TS: u64 = 1_798_761_600;

/// A fake resolver: answers each lookup with the next of its answers (the last one again once
/// they run out), `None` being a failed lookup, and records every name it was asked.
#[derive(Debug, Clone)]
struct Names(Arc<Mutex<(Vec<Option<Vec<IpAddr>>>, Vec<String>)>>);

impl Names {
    /// A resolver with `answers`, each a list of addresses ([`at`]) or `None`.
    fn new(answers: Vec<Option<Vec<IpAddr>>>) -> Names {
        Names(Arc::new(Mutex::new((answers, Vec::new()))))
    }

    /// Every name asked so far, in order.
    fn asked(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .clone()
    }
}

impl Resolve for Names {
    fn resolve(&self, host: &str, _: Duration) -> Result<Vec<IpAddr>, ResolveError> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (answers, asked) = &mut *held;
        asked.push(host.to_owned());
        let answer = answers[(asked.len() - 1).min(answers.len() - 1)].clone();
        answer.ok_or_else(|| {
            ResolveError::Lookup(io::Error::new(io::ErrorKind::NotFound, "no such name"))
        })
    }
}

/// `text` as an address.
fn ip(text: &str) -> IpAddr {
    text.parse().expect(text)
}

/// A lookup's answer of `texts`.
fn at(texts: &[&str]) -> Option<Vec<IpAddr>> {
    Some(texts.iter().map(|text| ip(text)).collect())
}

/// The key every test signs with: the bytes 0 to 31.
fn key() -> SigningKey {
    SigningKey::new((0..32).collect()).expect("a key")
}

/// A sender with a 60 s limit, the tests' rule (loopback admitted), `names`, and `authority`
/// as its only root.
fn sender(names: &Names, authority: &Authority) -> Sender {
    let rule = AddressRule::new([]).admit_loopback();
    Sender::new(
        Duration::from_secs(60),
        rule,
        Box::new(names.clone()),
        authority.roots(),
    )
}

/// One try of `url` by `sender`, with the tests' key, id and timestamp.
fn send(sender: &Sender, url: &CallbackUrl) -> CallbackAnswer {
    sender.send(url, &key(), ID, TS)
}

/// A reply with `status` and an empty body.
fn status(code: u16) -> String {
    format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\n\r\n")
}

/// A resolver that answers every lookup with `127.0.0.1`.
fn loopback() -> Names {
    Names::new(vec![at(&["127.0.0.1"])])
}

/// The failed answer of the class `failed`.
const FAILED: CallbackAnswer = CallbackAnswer::Failed(FailureClass::Failed);

/// The failed answer of the class `refused-address`.
const REFUSED: CallbackAnswer = CallbackAnswer::Failed(FailureClass::RefusedAddress);

/// Asserts the whole request a receiver reads, by exact equality: the request line (POST, the
/// URL's path and query) and the set of header lines, which is `content-length: 0`, `host`
/// and the three Standard Webhooks headers, the signature being the reference value a
/// separate HMAC implementation computed. Catches a body, a content type, a user agent or an
/// accept header sent along, a header holding anything of the job, and a signature over
/// another text.
#[test]
fn the_request_is_a_bare_signed_post() {
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[&status(200)]);
    let url = receiver.url("/js-done/ref-7?x=1");
    assert_eq!(
        send(&sender(&loopback(), &authority), &url),
        CallbackAnswer::Delivered
    );
    let heads: Vec<(String, Vec<String>)> = receiver.heads().iter().map(|h| parsed(h)).collect();
    let expected = vec![
        "content-length: 0".to_owned(),
        format!("host: {HOST}:{}", receiver.port),
        format!("webhook-id: {ID}"),
        "webhook-signature: v1,LnmnFoYopwXHnsZ0Wh/Xiu8raPGSGuBjRcb43YsTs6M=".to_owned(),
        format!("webhook-timestamp: {TS}"),
    ];
    assert_eq!(
        heads,
        [("POST /js-done/ref-7?x=1 HTTP/1.1".to_owned(), expected)]
    );
}

/// The request line of `head` and its header lines, sorted.
fn parsed(head: &str) -> (String, Vec<String>) {
    let mut lines = head
        .strip_suffix("\r\n\r\n")
        .expect("a whole head")
        .split("\r\n")
        .map(str::to_owned);
    let first = lines.next().expect("a request line");
    let mut headers: Vec<String> = lines.collect();
    headers.sort();
    (first, headers)
}

/// Asserts that 200, 204 and 299 each deliver, one request each. Catches a 2xx other than 200
/// read as a failure, and a delivery that sends twice.
#[test]
fn a_2xx_delivers() {
    let authority = Authority::new();
    let replies = [status(200), status(204), status(299)];
    let receiver = Receiver::start(&authority, &replies.each_ref().map(String::as_str));
    let sender = sender(&loopback(), &authority);
    let url = receiver.url("/");
    let answers: Vec<CallbackAnswer> = (0..3).map(|_| send(&sender, &url)).collect();
    assert_eq!(answers, [CallbackAnswer::Delivered; 3]);
    assert_eq!(receiver.heads().len(), 3);
}

/// Asserts that an answer of 200 that promises a megabyte of body and never sends a byte of it
/// delivers. Had the sender read the body, it would wait out its 60 s limit and fail. Catches
/// a sender that reads, or drains, the answer's body.
#[test]
fn an_answers_body_is_never_read() {
    let authority = Authority::new();
    let promised = "HTTP/1.1 200 X\r\ncontent-length: 1000000\r\n\r\n";
    let receiver = Receiver::start(&authority, &[promised]);
    let answer = send(&sender(&loopback(), &authority), &receiver.url("/"));
    assert_eq!(answer, CallbackAnswer::Delivered);
}

/// Asserts that 410 abandons, and that its neighbours 409 and 411 fail. Catches a 4xx range
/// read as abandoned, and a 410 tried again.
#[test]
fn a_410_abandons() {
    let authority = Authority::new();
    let replies = [status(410), status(409), status(411)];
    let receiver = Receiver::start(&authority, &replies.each_ref().map(String::as_str));
    let sender = sender(&loopback(), &authority);
    let url = receiver.url("/");
    let answers: Vec<CallbackAnswer> = (0..3).map(|_| send(&sender, &url)).collect();
    assert_eq!(answers, [CallbackAnswer::Abandoned, FAILED, FAILED]);
}

/// Asserts that each failure the design names is the class `failed`, which the store tries
/// again: 500, 503, 404, 300, a 307 and a 302 redirect (whose target receives nothing), a
/// time limit reached against a receiver that never answers, a refused connection, a
/// certificate from another authority, a name that does not resolve and one with no
/// address; and the wait before each next try: 5 s, doubling to 10 minutes. Catches a
/// redirect followed, a failure read as delivered, a refused connection read as a refused
/// address, and a wait that does not double, overflows, or passes its cap.
#[test]
fn each_failure_is_retried_with_backoff() {
    let authority = Authority::new();
    let target = Receiver::start(&authority, &[&status(200)]);
    let redirect = |code: u16| {
        format!(
            "HTTP/1.1 {code} X\r\nlocation: https://{HOST}:{}/moved\r\ncontent-length: 0\r\n\r\n",
            target.port
        )
    };
    let replies = [
        status(500),
        status(503),
        status(404),
        status(300),
        redirect(307),
        redirect(302),
    ];
    let receiver = Receiver::start(&authority, &replies.each_ref().map(String::as_str));
    let names = loopback();
    let sender = sender(&names, &authority);
    let url = receiver.url("/");
    let answers: Vec<CallbackAnswer> = (0..6).map(|_| send(&sender, &url)).collect();
    assert_eq!(answers, [FAILED; 6]);
    assert_eq!((receiver.heads().len(), target.heads()), (6, Vec::new()));

    // A receiver that reads the request and never answers: the limit is the only way out.
    let silent = Receiver::start(&authority, &[""]);
    let short = Sender::new(
        Duration::from_millis(200),
        AddressRule::new([]).admit_loopback(),
        Box::new(names.clone()),
        authority.roots(),
    );
    let closed = TcpListener::bind("127.0.0.1:0")
        .expect("binds loopback")
        .local_addr()
        .expect("an address")
        .port();
    let stranger = Receiver::start(&Authority::new(), &[&status(200)]);
    let unknown = Names::new(vec![None]);
    let empty = Names::new(vec![at(&[])]);
    let answers = [
        send(&short, &silent.url("/")),
        send(&sender, &url_of(HOST, closed, "/")),
        send(&sender, &stranger.url("/")),
        send(&self::sender(&unknown, &authority), &url),
        send(&self::sender(&empty, &authority), &url),
    ];
    assert_eq!(answers, [FAILED; 5]);
    assert_eq!(stranger.heads(), Vec::<String>::new());

    let waits = [0, 1, 2, 3, 7, 8, 9, u32::MAX].map(|failed| retry_wait(failed).as_secs());
    assert_eq!(waits, [5, 5, 10, 20, 320, 600, 600, 600]);
}

/// Asserts that a try goes to the address the resolver gave and the rule admitted (the name
/// is asked once and the receiver gets the call), that a literal host is connected to with no
/// lookup, and that an answer holding one denied address among admitted ones refuses the
/// whole try before any connection. Catches a sender that resolves the name a second way, one
/// that connects to an unchecked address, and one that skips the denied address and uses the
/// rest.
#[test]
fn the_sender_connects_only_to_a_checked_address() {
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[&status(200)]);
    let names = Names::new(vec![
        at(&["127.0.0.1"]),
        at(&["127.0.0.1", "169.254.169.254"]),
    ]);
    let sender = sender(&names, &authority);
    let by_name = send(&sender, &receiver.url("/"));
    let by_literal = send(&sender, &url_of("127.0.0.1", receiver.port, "/"));
    let one_denied = send(&sender, &receiver.url("/"));
    assert_eq!(
        [by_name, by_literal, one_denied],
        [
            CallbackAnswer::Delivered,
            CallbackAnswer::Delivered,
            REFUSED
        ]
    );
    assert_eq!(receiver.heads().len(), 2);
    assert_eq!(names.asked(), [HOST, HOST]);
}

/// Asserts that a name that answers loopback, then the metadata address, then both, then
/// loopback again is resolved again at each try and refused on exactly the tries whose answer
/// holds a denied address, which send nothing. Catches a sender that caches the first answer
/// (DNS rebinding), and one that checks a name once and connects to it again later.
#[test]
fn a_name_that_turns_inward_is_refused_on_that_try() {
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[&status(200)]);
    let names = Names::new(vec![
        at(&["127.0.0.1"]),
        at(&["169.254.169.254"]),
        at(&["127.0.0.1", "169.254.169.254"]),
        at(&["127.0.0.1"]),
    ]);
    let sender = sender(&names, &authority);
    let url = receiver.url("/");
    let answers: Vec<CallbackAnswer> = (0..4).map(|_| send(&sender, &url)).collect();
    let delivered = CallbackAnswer::Delivered;
    assert_eq!(answers, [delivered, REFUSED, REFUSED, delivered]);
    assert_eq!(receiver.heads().len(), 2);
    assert_eq!(names.asked(), [HOST; 4]);
}

/// Asserts that the server's rule, with the WebPki roots, refuses before any connection a
/// name that resolves to the metadata address, carrier-grade NAT or loopback, and the
/// literal hosts `2130706433`, `0x7f.1`, `[::1]`, `[::ffff:a9fe:a9fe]` and `168.63.129.16`,
/// none of which is looked up. Catches a numeric host passed to a resolver, and an inward
/// address reached because the rule of the tests was not the one in force.
#[test]
fn the_server_rule_refuses_inward_addresses_before_connecting() {
    let names = Names::new(vec![
        at(&["169.254.169.254"]),
        at(&["100.100.100.200"]),
        at(&["127.0.0.1"]),
    ]);
    let sender = Sender::new(
        Duration::from_secs(60),
        AddressRule::new([]),
        Box::new(names.clone()),
        TlsRoots::webpki(),
    );
    let hosts = [
        HOST,
        HOST,
        HOST,
        "2130706433",
        "0x7f.1",
        "[::1]",
        "[::ffff:a9fe:a9fe]",
        "168.63.129.16",
    ];
    let answers = hosts.map(|host| send(&sender, &url_of(host, 443, "/x")));
    assert_eq!(answers, [REFUSED; 8]);
    assert_eq!(names.asked(), [HOST; 3]);
}

/// Asserts, by exact equality, every line the operator's log gets for a 500 and for a refused
/// address, sent to a URL whose path and query carry the receiver's reference and a token:
/// the host, the status or the refused address, and nothing of the path or query; and that
/// the answer holds only the class, shown as `failed` and `refused-address`. Catches a log
/// line that names the whole URL, and a status code or address reaching the submitter.
#[test]
fn a_failure_reports_only_its_class() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let authority = Authority::new();
    let receiver = Receiver::start(&authority, &[&status(500)]);
    let names = Names::new(vec![at(&["127.0.0.1"]), at(&["169.254.169.254"])]);
    let sender = sender(&names, &authority);
    let url = receiver.url("/js-done/ref-7?token=secret");
    let answers = [send(&sender, &url), send(&sender, &url)];
    assert_eq!(answers, [FAILED, REFUSED]);
    let words = [FailureClass::Failed, FailureClass::RefusedAddress].map(FailureClass::as_str);
    assert_eq!(words, ["failed", "refused-address"]);
    let logged: Vec<String> = lines.take().lines().map(str::to_owned).collect();
    assert_eq!(
        logged,
        [
            format!(
                " WARN a callback was answered with neither 2xx nor 410 host=\"{HOST}\" \
                 status=500"
            ),
            format!(
                " WARN a callback try failed host=\"{HOST}\" error=io: the address rule \
                 refuses 169.254.169.254"
            ),
        ]
    );
}

/// What a test's subscriber wrote.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

impl Lines {
    /// Everything written so far, as text.
    fn take(&self) -> String {
        io::Write::flush(&mut self.clone()).expect("flushes");
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(std::mem::take(&mut *held)).expect("text")
    }
}

/// Installs once per process a global subscriber that wants every line and writes it
/// nowhere, so no thread settles a line as never wanted before the capturing test reaches it.
fn process_sink() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .try_init()
            .ok();
    });
}
