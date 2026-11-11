//! The forwarder over a fake [`Listen`] that binds in the test's own network namespace (each
//! listener at port 0 of `127.0.0.1`, whatever job port it was asked for), and the kernel's
//! calls where an ordinary user can make them. Every host side is a listener of the test's
//! on `127.0.0.1` port 0.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};

use js_agent_wire::{HostPort, PortProtocol};

use super::kernel::{enter, job_namespace, listen_in, listen_on};
use super::{Forwarder, Kernel, Listen, OfferError};

/// How many times a test looks for what the forwarder's threads do before it fails: far more
/// than they take, so only a forwarder that never does it reaches this.
const TRIES: u32 = 10_000_000;

/// What the fake was asked, and the listeners it made.
#[derive(Debug, Default)]
struct Seen {
    /// `<pid> <job ports>`, one per ask.
    asks: Vec<String>,
    /// A clone of each listener it made, in order.
    listeners: Vec<TcpListener>,
    /// Whether the next ask fails.
    fails: bool,
}

/// Binds each listener on `127.0.0.1` port 0 of the test's own namespace.
#[derive(Debug, Default)]
struct Fake(Mutex<Seen>);

impl Fake {
    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The address of the `n`th listener made: where the job would connect.
    fn job_side(&self, n: usize) -> SocketAddr {
        self.seen().listeners[n].local_addr().expect("an address")
    }
}

impl Listen for Fake {
    fn listen(&self, pid: u32, ports: &[u16]) -> io::Result<Vec<TcpListener>> {
        let mut seen = self.seen();
        seen.asks.push(format!("{pid} {ports:?}"));
        if seen.fails {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let made: Vec<TcpListener> = ports
            .iter()
            .map(|_| TcpListener::bind("127.0.0.1:0").expect("a loopback listener"))
            .collect();
        for listener in &made {
            seen.listeners.push(listener.try_clone().expect("a clone"));
        }
        Ok(made)
    }
}

fn forwarder() -> (Arc<Fake>, Forwarder) {
    let fake = Arc::new(Fake::default());
    let forwarder = Forwarder::new(Arc::clone(&fake) as Arc<dyn Listen>);
    (fake, forwarder)
}

/// A host service of the test's: a listener on `127.0.0.1` port 0, and its port.
fn host_service() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().expect("an address").port();
    (listener, port)
}

fn host_port(host_port: u16, container_port: Option<u16>) -> HostPort {
    HostPort {
        host_port,
        container_port,
        protocol: PortProtocol::Tcp,
    }
}

/// Reads `stream` to its end.
fn read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read to the end");
    bytes
}

/// Proves the listeners are asked for at the job ports in the start's order (the host port
/// where the job port is left out, the job port where it is given), in the namespace of the
/// pid given, and that bytes go both ways between a job's connection to each and the host
/// port behind it, for two runs on one forwarder. Catches a forwarder that listens at the
/// host port where the job named its own, joins a listener to the wrong host port, or copies
/// one way only.
#[test]
fn bytes_go_both_ways_at_the_same_and_at_another_job_port() {
    let (fake, forwarder) = forwarder();
    let (first, first_port) = host_service();
    let (second, second_port) = host_service();
    let ports = [
        host_port(first_port, None),
        host_port(second_port, Some(8080)),
    ];
    let offered = forwarder.offer(41, &ports).expect("offered");
    let again = forwarder.offer(42, &ports[..1]).expect("offered again");
    assert_eq!(
        fake.seen().asks,
        [
            format!("41 [{first_port}, 8080]"),
            format!("42 [{first_port}]")
        ]
    );
    for (n, host) in [(0, &first), (1, &second), (2, &first)] {
        let mut job = TcpStream::connect(fake.job_side(n)).expect("the job connects");
        let (mut served, _) = host.accept().expect("the host is reached");
        job.write_all(format!("up {n}").as_bytes()).expect("write");
        job.shutdown(Shutdown::Write).expect("shut");
        assert_eq!(read_all(&mut served), format!("up {n}").as_bytes());
        served
            .write_all(format!("down {n}").as_bytes())
            .expect("write");
        drop(served);
        assert_eq!(read_all(&mut job), format!("down {n}").as_bytes());
    }
    assert_eq!(offered.accept_errors(), 0);
    drop((offered, again));
}

/// Proves that the host's close reaches the job as the end of what it reads while the job can
/// still send, and the job's close reaches the host the same way, and that one job port
/// carries connection after connection. Catches a forwarder that ends a connection when one
/// side closes, before the other has sent what it has, or that serves one connection only.
#[test]
fn each_side_closing_closes_the_other_once_it_has_sent() {
    let (fake, forwarder) = forwarder();
    let (host, port) = host_service();
    let _offered = forwarder
        .offer(7, &[host_port(port, None)])
        .expect("offered");
    // The host closes first.
    let mut job = TcpStream::connect(fake.job_side(0)).expect("the job connects");
    let (mut served, _) = host.accept().expect("the host is reached");
    served.write_all(b"bye").expect("write");
    served.shutdown(Shutdown::Write).expect("shut");
    assert_eq!(read_all(&mut job), b"bye");
    job.write_all(b"late").expect("the job may still send");
    drop(job);
    assert_eq!(read_all(&mut served), b"late");
    // Then the job closes first, on a second connection to the same port.
    let mut job = TcpStream::connect(fake.job_side(0)).expect("the job connects again");
    let (mut served, _) = host.accept().expect("the host is reached again");
    job.write_all(b"done").expect("write");
    job.shutdown(Shutdown::Write).expect("shut");
    assert_eq!(read_all(&mut served), b"done");
    served.write_all(b"ack").expect("the host may still send");
    drop(served);
    assert_eq!(read_all(&mut job), b"ack");
}

/// Proves that a job's connection to a host port nothing serves is closed with nothing read,
/// and that the listener goes on serving the next connection once the host side is there.
/// Catches a forwarder that holds the job's connection open when the host refuses it, or
/// stops listening after one refusal.
#[test]
fn a_refused_host_port_closes_the_jobs_connection() {
    let (fake, forwarder) = forwarder();
    // A port that is bound and does not listen refuses every connection; it stays bound so
    // nothing else can take it.
    let socket = rustix::net::socket(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::STREAM,
        None,
    )
    .expect("a socket");
    let any = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0);
    rustix::net::bind(&socket, &any).expect("bound");
    let bound = rustix::net::getsockname(&socket).expect("its name");
    let refusing = SocketAddr::try_from(bound).expect("an IP address").port();
    let (host, port) = host_service();
    let ports = [host_port(refusing, None), host_port(port, Some(9))];
    let _offered = forwarder.offer(7, &ports).expect("offered");
    let mut job = TcpStream::connect(fake.job_side(0)).expect("the job connects");
    assert_eq!(read_all(&mut job), b"");
    let mut job = TcpStream::connect(fake.job_side(1)).expect("the job connects");
    let (mut served, _) = host.accept().expect("the other port is reached");
    served.write_all(b"ok").expect("write");
    drop(served);
    assert_eq!(read_all(&mut job), b"ok");
}

/// Proves that dropping the offer closes a connection through it, and then the listener: a
/// job's read ends, and a new connection is refused. Catches a release that leaves a job's
/// connection to the host open after its run, or a listener in a namespace that outlives it.
#[test]
fn dropping_the_offer_closes_the_listeners_and_their_connections() {
    let (fake, forwarder) = forwarder();
    let (host, port) = host_service();
    let offered = forwarder
        .offer(7, &[host_port(port, None)])
        .expect("offered");
    let mut job = TcpStream::connect(fake.job_side(0)).expect("the job connects");
    let (mut served, _) = host.accept().expect("the host is reached");
    job.write_all(b"x").expect("write");
    let mut byte = [0];
    served.read_exact(&mut byte).expect("the byte is through");
    // The fake's clone of the listener would keep it open: it goes first.
    let listener = fake.seen().listeners.remove(0);
    let job_side = listener.local_addr().expect("an address");
    drop(listener);
    drop(offered);
    assert_eq!(read_all(&mut job), b"");
    assert_eq!(read_all(&mut served), b"");
    let refused = TcpStream::connect(job_side).expect_err("the listener is closed");
    assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);
}

/// Proves that a listener whose accept fails is not given up: the failures are counted on
/// the offer and accepted again, and the offer still drops whole. Catches a forwarder that
/// ends a job's host port on the first failed accept (the agent short of descriptors for a
/// moment), or that spins on one without counting it.
#[test]
fn a_failed_accept_is_counted_and_tried_again() {
    let (fake, forwarder) = forwarder();
    let (_host, port) = host_service();
    let offered = forwarder
        .offer(7, &[host_port(port, None)])
        .expect("offered");
    // A listening socket shut for reading fails every accept (EINVAL) from then on.
    let listener = fake.seen().listeners.remove(0);
    rustix::net::shutdown(&listener, rustix::net::Shutdown::Read).expect("shut");
    // Bounded: a forwarder that stopped accepting fails the test rather than hanging it.
    let counted = (0..TRIES).any(|_| {
        std::thread::yield_now();
        offered.accept_errors() >= 2
    });
    assert!(counted, "fewer than two failed accepts after {TRIES} looks");
    drop(offered);
}

/// Proves that a namespace step that fails fails the offer, naming the job ports, with
/// nothing listening. Catches an offer that reports success for a run with no listener,
/// whose init would then wait out its bound.
#[test]
fn a_failed_listen_fails_the_offer_naming_the_job_ports() {
    let (fake, forwarder) = forwarder();
    fake.seen().fails = true;
    let ports = [host_port(80, None), host_port(5432, Some(15432))];
    let failed = forwarder.offer(9, &ports).expect_err("refused");
    assert!(matches!(failed, OfferError::Listen { .. }), "{failed:?}");
    assert_eq!(
        failed.to_string(),
        "listen at the job's ports 80,15432 in its network namespace: permission denied"
    );
    assert!(fake.seen().listeners.is_empty());
}

/// Proves that the kernel's listener is on `127.0.0.1` at the port asked and accepts, and
/// that a port already bound there is refused as in use. Catches a listener bound to every
/// address of the job's namespace, or one bound without listening.
#[test]
fn the_kernel_listens_on_loopback_at_the_port_asked() {
    let listener = listen_on(0).expect("a listener");
    let address = listener.local_addr().expect("an address");
    assert!(address.ip().is_loopback(), "{address}");
    let _job = TcpStream::connect(address).expect("it accepts");
    listener.accept().expect("accepted");
    let taken = listen_on(address.port()).expect_err("the port is in use");
    assert_eq!(taken.kind(), io::ErrorKind::AddrInUse);
}

/// A step that leaves the thread in the namespace it is in: the test's own.
fn stay(_: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    Ok(())
}

/// Proves that once its thread is in the namespace, the listen makes a listener on loopback at
/// each port asked, in order, each accepting; that a port that cannot be bound fails the whole
/// listen, the listeners already made closed with it; and that a namespace that was not opened
/// is passed on as the error, nothing entered. Catches a listen that keeps a half of its
/// listeners, or binds where the thread has not entered.
#[test]
fn the_thread_in_the_namespace_binds_each_port_on_loopback() {
    let own = || File::open("/proc/self/ns/net");
    let made = listen_in(own(), &[0, 0], stay).expect("bound in the test's namespace");
    assert_eq!(made.len(), 2);
    for listener in &made {
        let address = listener.local_addr().expect("an address");
        assert!(address.ip().is_loopback(), "{address}");
        let _job = TcpStream::connect(address).expect("it accepts");
        listener.accept().expect("accepted");
    }
    let taken = made[1].local_addr().expect("an address").port();
    let failed = listen_in(own(), &[0, taken], stay).expect_err("the second port is taken");
    assert_eq!(failed.kind(), io::ErrorKind::AddrInUse);
    let unopened = Err(io::Error::from(io::ErrorKind::NotFound));
    let none = listen_in(unopened, &[0], stay).expect_err("no namespace");
    assert_eq!(none.kind(), io::ErrorKind::NotFound);
}

/// Proves that the kernel refuses the agent's own network namespace, and a process that is
/// not there, before it enters anything; and that entering a namespace is the kernel's to
/// allow (root may, an ordinary user may not). Catches a forwarder that would listen on the
/// host's own loopback for a container whose namespace it read too early.
#[test]
fn the_kernel_refuses_the_agents_own_namespace() {
    let own = std::process::id();
    let shared = job_namespace(own).expect_err("the agent's own namespace");
    assert_eq!(
        shared.to_string(),
        format!("process {own} shares the agent's network namespace")
    );
    let refused = Kernel
        .listen(own, &[0])
        .expect_err("refused by the kernel's listen too");
    assert_eq!(refused.to_string(), shared.to_string());
    let gone = job_namespace(u32::MAX).expect_err("no such process");
    assert_eq!(gone.kind(), io::ErrorKind::NotFound);
    let namespace = File::open("/proc/self/ns/net");
    let entered = listen_in(namespace, &[0], enter);
    match rustix::process::geteuid().is_root() {
        true => assert_eq!(entered.expect("root enters").len(), 1),
        false => {
            let denied = entered.expect_err("an ordinary user cannot enter a namespace");
            assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        }
    }
}
