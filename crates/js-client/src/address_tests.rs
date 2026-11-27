//! The tests of the server's book: TLS servers on 127.0.0.1 with certificates for example
//! names, an agent built as a client's is, and lookups scripted by the test (no lookup reaches
//! a resolver). Each test makes its own book, which looks the name up again on every new
//! connection (a refresh of zero) unless it says otherwise; a test waits for the background
//! lookup a request started with [`Book::settle`], never for a time.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tracing::Dispatch;
use tracing::subscriber::{DefaultGuard, NoSubscriber};
use ureq::Agent;
use ureq::tls::{Certificate, RootCerts};

use super::*;
use crate::client::{REQUEST_LIMIT, build_agent};

/// The server's name in these tests.
const NAME: &str = "js.example.test";

/// What the scripted lookups say when they fail: the resolver's words when it cannot reach
/// the zone.
const TRY_AGAIN: &str = "failed to lookup address information: Try again";

/// A certificate for one name, and a server configuration that presents it.
struct Issued {
    der: CertificateDer<'static>,
    config: Arc<ServerConfig>,
}

impl Issued {
    fn new(name: &str) -> Self {
        let made = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        let der = made.cert.der().clone();
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
        let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .unwrap();
        Self {
            der,
            config: Arc::new(config),
        }
    }
}

/// A TLS server on 127.0.0.1 that answers each request with 200 and closes the connection,
/// so that every request of a client makes a new connection. Stopped, it hangs up on each
/// connection it takes: an address that does not answer, whose port stays the test's.
struct Server {
    address: SocketAddr,
    accepted: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    answering: Arc<AtomicBool>,
}

impl Server {
    fn start(cert: &Issued) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let answering = Arc::new(AtomicBool::new(true));
        let (took, answered, open) = (
            Arc::clone(&accepted),
            Arc::clone(&served),
            Arc::clone(&answering),
        );
        let config = Arc::clone(&cert.config);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                took.fetch_add(1, Ordering::SeqCst);
                if open.load(Ordering::SeqCst) {
                    let _ = answer(stream, &config, &answered);
                }
            }
        });
        Self {
            address,
            accepted,
            served,
            answering,
        }
    }

    /// The connections taken, a handshake that failed included. A client's attempt on the
    /// server cannot fail before it is taken: the server's half of the handshake is what
    /// fails it.
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// The requests answered: counted before the answer is sent, so a request that got its
    /// answer is counted.
    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.answering.store(false, Ordering::SeqCst);
    }

    fn resume(&self) {
        self.answering.store(true, Ordering::SeqCst);
    }
}

/// The TLS handshake, one request read to the end of its body, and the answer.
fn answer(stream: TcpStream, config: &Arc<ServerConfig>, served: &AtomicUsize) -> io::Result<()> {
    let conn = ServerConnection::new(Arc::clone(config)).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(conn, stream);
    let (mut head, mut byte) = (Vec::new(), [0_u8; 1]);
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let length = String::from_utf8_lossy(&head)
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").map(str::to_owned))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    io::copy(&mut Read::take(&mut tls, length), &mut io::sink())?;
    served.fetch_add(1, Ordering::SeqCst);
    tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")?;
    tls.conn.send_close_notify();
    tls.flush()
}

/// A lookup that gives its answers in order (`None` fails as a resolver that cannot reach
/// the zone does), then the last one again and again.
#[derive(Debug)]
struct Script {
    answers: Mutex<Vec<Option<Vec<SocketAddr>>>>,
    asked: AtomicUsize,
}

impl Script {
    fn new(answers: Vec<Option<Vec<SocketAddr>>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers),
            asked: AtomicUsize::new(0),
        })
    }

    /// From now on, `answer` to every lookup.
    fn then(&self, answer: Option<Vec<SocketAddr>>) {
        *self.answers.lock().unwrap() = vec![answer];
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Lookup for Script {
    fn lookup(&self, host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        assert_eq!(host, NAME);
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut answers = self.answers.lock().unwrap();
        let answer = if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        };
        answer.ok_or_else(|| io::Error::other(TRY_AGAIN))
    }
}

/// A lookup that answers nothing until the test drops the sender of `wait`, and says on
/// `done` that it has answered.
#[derive(Debug)]
struct Stuck {
    wait: Mutex<mpsc::Receiver<()>>,
    done: Mutex<mpsc::Sender<()>>,
}

impl Lookup for Stuck {
    fn lookup(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        let _ = self.wait.lock().unwrap().recv();
        let released = Err(io::Error::other("released"));
        let _ = self.done.lock().unwrap().send(());
        released
    }
}

/// The book of [`NAME`] on `port`, looked up by `lookup` again on every new connection.
fn book_of(lookup: &Arc<Script>, port: u16) -> Arc<Book> {
    let lookup: Arc<dyn Lookup> = Arc::clone(lookup) as Arc<dyn Lookup>;
    Arc::new(Book::new(NAME, port, lookup, Duration::ZERO))
}

/// An agent as a token client builds it, that trusts each of `trusted` and finds the server
/// through `book`.
fn agent_of(book: &Arc<Book>, trusted: &[&Issued]) -> Agent {
    let certs = trusted
        .iter()
        .map(|issued| Certificate::from_der(issued.der.as_ref()).to_owned())
        .collect();
    let roots = RootCerts::Specific(Arc::new(certs));
    build_agent(true, REQUEST_LIMIT, REQUEST_LIMIT, &roots, None, book)
}

/// One request to [`NAME`], as a heartbeat is sent, and the status it got; then the
/// background lookup it started, if any, is learned.
fn beat(agent: &Agent, book: &Book) -> Result<u16, String> {
    let sent = agent
        .post(format!("https://{NAME}:{}/v1/heartbeat", book.port))
        .send("{}");
    book.settle();
    sent.map(|reply| reply.status().as_u16())
        .map_err(|err| err.to_string())
}

/// The lines logged on this test's thread and the threads it starts that take its subscriber.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Lines {
    /// Installs a subscriber that writes here, for this thread, until the guard is dropped.
    /// A dispatcher that wants nothing is registered first, once for the process, so the
    /// interest of a log line's callsite is never computed from another test thread's lack of
    /// a subscriber alone.
    fn capture(&self) -> DefaultGuard {
        static KEEPER: OnceLock<Dispatch> = OnceLock::new();
        KEEPER.get_or_init(|| Dispatch::new(NoSubscriber::new()));
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// The lines written so far, each trimmed.
    fn lines(&self) -> Vec<String> {
        let mut writer = self.clone();
        writer.flush().unwrap();
        let bytes = writer.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| line.trim().to_owned())
            .collect()
    }
}

/// The line logged when lookups start failing while `known` is remembered.
fn cannot_look_up(known: SocketAddr) -> String {
    format!(
        "cannot look up {NAME}: {TRY_AGAIN}; requests go to {known}, the address the last handshake with the server completed on, until a lookup answers"
    )
}

/// Asserts that once a request reached the server, requests keep reaching it while every
/// lookup of the name fails (each new connection goes to the remembered address), that the
/// failure is logged once and not once per request, and that one line says when a lookup
/// answers again. It catches a client that looks the name up before each request and fails
/// the request with the lookup, as ureq's own resolver does.
#[test]
fn a_lookup_that_fails_after_the_first_success_leaves_requests_flowing() {
    let lines = Lines::default();
    let _log = lines.capture();
    let cert = Issued::new(NAME);
    let server = Server::start(&cert);
    let script = Script::new(vec![Some(vec![server.address]), None]);
    let book = book_of(&script, server.address.port());
    let agent = agent_of(&book, &[&cert]);
    for _ in 0..4 {
        assert_eq!(beat(&agent, &book), Ok(200));
    }
    assert_eq!(server.served(), 4);
    // One lookup on the spot for the first request, which answered, then one in the
    // background for each later one, all three failed.
    assert_eq!(script.asked(), 4);
    assert_eq!(lines.lines(), vec![cannot_look_up(server.address)]);
    script.then(Some(vec![server.address]));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(
        lines.lines(),
        vec![
            cannot_look_up(server.address),
            format!("{NAME} is looked up again: [{}]", server.address),
        ]
    );
    assert_eq!(book.known(), Some(server.address));
}

/// Asserts that with an address given beside the name (`--server-address`) and no lookup
/// ever answering, requests reach the server at that address, the certificate checked
/// against the name; and that without one the same lookups fail the request, naming the
/// lookup's failure. It catches a given address that is not used, or used only after a lookup.
#[test]
fn a_lookup_that_fails_from_the_start_works_with_a_given_address() {
    let lines = Lines::default();
    let _log = lines.capture();
    let cert = Issued::new(NAME);
    let server = Server::start(&cert);
    let script = Script::new(vec![None]);
    let book = book_of(&script, server.address.port());
    let agent = agent_of(&book, &[&cert]);
    assert_eq!(
        beat(&agent, &book),
        Err(format!("io: {TRY_AGAIN}")),
        "with no address known the lookup's failure is the request's"
    );
    book.seed(IpAddr::V4(Ipv4Addr::LOCALHOST));
    for _ in 0..3 {
        assert_eq!(beat(&agent, &book), Ok(200));
    }
    assert_eq!(server.served(), 3);
    assert_eq!(book.known(), Some(server.address));
    assert_eq!(lines.lines(), vec![cannot_look_up(server.address)]);
}

/// Asserts that an address a lookup gives is taken only once a handshake with it completes:
/// one whose certificate is trusted but for another name is tried, fails, and the requests
/// stay on the remembered address; one that completes the handshake under the name is taken,
/// with one line. It catches a client that follows the newest answer before trying it, and
/// one that checks the certificate against the address instead of the name.
#[test]
fn a_new_address_is_taken_only_after_its_handshake_completes() {
    let lines = Lines::default();
    let _log = lines.capture();
    let (cert, other) = (Issued::new(NAME), Issued::new("other.example.test"));
    let first = Server::start(&cert);
    let impostor = Server::start(&other);
    let second = Server::start(&cert);
    let script = Script::new(vec![Some(vec![first.address])]);
    let book = book_of(&script, first.address.port());
    let agent = agent_of(&book, &[&cert, &other]);
    assert_eq!(beat(&agent, &book), Ok(200));
    script.then(Some(vec![impostor.address]));
    // Sent to the remembered address; its background lookup gives the impostor's.
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(impostor.accepted(), 0);
    // Tried on the impostor first, whose handshake fails, then on the remembered address.
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!((impostor.accepted(), impostor.served()), (1, 0));
    assert_eq!(first.served(), 3);
    assert_eq!(book.known(), Some(first.address));
    script.then(Some(vec![second.address]));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(first.served(), 4);
    assert_eq!(second.accepted(), 0);
    // The second server completes the handshake under the name and is taken.
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!((first.served(), second.served()), (4, 2));
    assert_eq!(impostor.served(), 0);
    assert_eq!(book.known(), Some(second.address));
    assert_eq!(
        lines.lines(),
        vec![format!(
            "{NAME} moved from {} to {}: a lookup gave it and a handshake with it completed",
            first.address, second.address
        )]
    );
}

/// Asserts that a remembered address that stops answering is not kept: the request that
/// finds it so looks the name up on the spot and goes to the new answer, which is kept. It
/// catches a remembered address that is used for good once learned.
#[test]
fn a_remembered_address_that_stops_answering_gives_way_to_a_new_lookup() {
    let lines = Lines::default();
    let _log = lines.capture();
    let cert = Issued::new(NAME);
    let (old, new) = (Server::start(&cert), Server::start(&cert));
    let script = Script::new(vec![Some(vec![old.address])]);
    let book = book_of(&script, old.address.port());
    let agent = agent_of(&book, &[&cert]);
    assert_eq!(beat(&agent, &book), Ok(200));
    old.stop();
    script.then(Some(vec![new.address]));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!((old.served(), new.served()), (1, 2));
    assert_eq!(book.known(), Some(new.address));
    assert_eq!(
        lines.lines(),
        vec![format!(
            "{NAME} moved from {} to {}: a lookup gave it and a handshake with it completed",
            old.address, new.address
        )]
    );
}

/// Asserts that when the remembered address stops answering and no lookup answers either,
/// the request fails rather than waits, and the address is still tried by the next request
/// (after a lookup that fails again), which reaches it once it answers again. It catches a client that forgets the only address
/// it has, and one that hangs on it.
#[test]
fn a_remembered_address_that_stops_answering_while_lookups_fail_fails_the_request() {
    let cert = Issued::new(NAME);
    let server = Server::start(&cert);
    let script = Script::new(vec![Some(vec![server.address]), None]);
    let book = book_of(&script, server.address.port());
    let agent = agent_of(&book, &[&cert]);
    assert_eq!(beat(&agent, &book), Ok(200));
    server.stop();
    assert!(beat(&agent, &book).is_err());
    server.resume();
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!(server.served(), 2);
    assert_eq!(book.known(), Some(server.address));
}

/// Asserts that once the remembered address failed a request, the next request looks the
/// name up before it tries that address again, and goes to the new answer: the remembered
/// address is not tried first (it is taken by no third connection). It catches a client that
/// puts an address that no longer answers, one that drops what is sent to it and so takes the
/// whole of each request's limit, first in every request after it.
#[test]
fn a_remembered_address_that_failed_is_tried_after_a_new_lookup() {
    let lines = Lines::default();
    let _log = lines.capture();
    let cert = Issued::new(NAME);
    let old = Server::start(&cert);
    let script = Script::new(vec![Some(vec![old.address])]);
    let book = book_of(&script, old.address.port());
    let agent = agent_of(&book, &[&cert]);
    assert_eq!(beat(&agent, &book), Ok(200));
    old.stop();
    script.then(None);
    assert!(beat(&agent, &book).is_err());
    let new = Server::start(&cert);
    script.then(Some(vec![new.address]));
    assert_eq!(beat(&agent, &book), Ok(200));
    assert_eq!((old.accepted(), new.served()), (2, 1));
    assert_eq!(book.known(), Some(new.address));
    assert_eq!(
        lines.lines(),
        vec![
            cannot_look_up(old.address),
            format!(
                "{NAME} moved from {} to {}: a lookup gave it and a handshake with it completed",
                old.address, new.address
            ),
        ]
    );
}

/// Asserts that with no address remembered, a lookup's answer that does not answer fails the
/// request without a second lookup on the spot (the addresses tried were a lookup's already),
/// and that an answer with no address is a failed lookup. It catches a client that looks the
/// name up twice for one request, and one that hands ureq an empty answer.
#[test]
fn with_no_address_remembered_a_failed_answer_is_not_looked_up_again() {
    let cert = Issued::new(NAME);
    let server = Server::start(&cert);
    server.stop();
    let script = Script::new(vec![Some(vec![server.address])]);
    let book = book_of(&script, server.address.port());
    let agent = agent_of(&book, &[&cert]);
    assert!(beat(&agent, &book).is_err());
    assert_eq!((script.asked(), server.accepted()), (1, 1));
    assert_eq!(book.known(), None);
    script.then(Some(Vec::new()));
    assert_eq!(
        beat(&agent, &book),
        Err("io: the name has no address".to_owned())
    );
}

/// Asserts that a lookup made on the spot that does not answer within the request's limit
/// fails it as a lookup that timed out. The lookup never answers before the test lets it, so
/// the limit (zero) is all that ends the wait. It catches a lookup waited for with no bound.
#[test]
fn a_lookup_that_does_not_answer_within_the_limit_times_out() {
    let (release, wait) = mpsc::channel();
    let (done, answered) = mpsc::channel();
    let lookup: Arc<dyn Lookup> = Arc::new(Stuck {
        wait: Mutex::new(wait),
        done: Mutex::new(done),
    });
    let book = Arc::new(Book::new(NAME, 7301, lookup, Duration::ZERO));
    let err = book.addresses(Duration::ZERO).unwrap_err();
    assert_eq!(err.to_string(), "timeout: resolve");
    // Let the lookup answer, and wait until it has, so no thread is left behind.
    drop(release);
    answered.recv().unwrap();
}

/// Asserts that a remembered address is not looked up again before the refresh is due: the
/// first request starts one lookup, and the next ones, within the hour, start none. It
/// catches a lookup per request, which a failing resolver would make of every heartbeat.
#[test]
fn the_name_is_looked_up_again_only_once_the_refresh_is_due() {
    let cert = Issued::new(NAME);
    let server = Server::start(&cert);
    let script = Script::new(vec![None]);
    let lookup: Arc<dyn Lookup> = Arc::clone(&script) as Arc<dyn Lookup>;
    let hour = Duration::from_secs(3600);
    let book = Arc::new(Book::new(NAME, server.address.port(), lookup, hour));
    book.seed(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let agent = agent_of(&book, &[&cert]);
    for _ in 0..3 {
        assert_eq!(beat(&agent, &book), Ok(200));
    }
    assert_eq!(script.asked(), 1);
}

/// Asserts that a given address is a first one only: a second one, or one given after a
/// handshake taught the book its address, changes nothing. It catches an agent started again
/// in the same process that throws away the address it learned.
#[test]
fn a_given_address_does_not_replace_a_known_one() {
    let script = Script::new(vec![None]);
    let book = book_of(&script, 7301);
    book.seed(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
    book.seed(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)));
    assert_eq!(book.known(), Some("192.0.2.7:7301".parse().unwrap()));
}

/// Asserts the flag's rules: an address is given only beside an `https://` server, named by
/// the flag, and one given is the address of that server's book on its port. It catches a
/// bearer sent in clear to an address beside a plain-http loopback server.
#[test]
fn a_server_address_is_given_only_beside_an_https_server() {
    let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));
    assert_eq!(
        give_server_address("http://127.0.0.1:9", address)
            .unwrap_err()
            .to_string(),
        "--server-address: is given only with an https:// --server: a plain http:// server is the machine itself"
    );
    assert_eq!(
        give_server_address("ftp://given.example.test", address)
            .unwrap_err()
            .to_string(),
        "--server: \"ftp://given.example.test\" must start with https:// (or http:// for a loopback host)"
    );
    give_server_address("https://given.example.test:7301", address).unwrap();
    let server = ServerUrl::parse("https://given.example.test:7301").unwrap();
    assert_eq!(book(&server).known(), Some(SocketAddr::new(address, 7301)));
}

/// Asserts that the books of a process are one per host and port, so the clients a process
/// makes one after another (the dispatch poll's) share what the first learned. It catches a
/// book per client, which a client made for one poll would leave as empty as it found it.
#[test]
fn the_books_are_one_per_host_and_port() {
    let books = Books::new(Arc::new(SystemLookup), REFRESH);
    let parse = |text: &str| ServerUrl::parse(text).unwrap();
    let book = books.book(&parse("https://js.example.test:7301"));
    assert!(Arc::ptr_eq(
        &book,
        &books.book(&parse("https://js.example.test:7301"))
    ));
    assert!(!Arc::ptr_eq(
        &book,
        &books.book(&parse("https://js.example.test:7302"))
    ));
    assert!(!Arc::ptr_eq(
        &book,
        &books.book(&parse("https://other.example.test:7301"))
    ));
}

/// Asserts that the system lookup reads an address literal as itself, so a server named by
/// its address is never looked up (and this test asks no resolver).
#[test]
fn the_system_lookup_reads_a_literal_as_itself() {
    assert_eq!(
        SystemLookup.lookup("127.0.0.1", 7).unwrap(),
        vec!["127.0.0.1:7".parse::<SocketAddr>().unwrap()]
    );
}
