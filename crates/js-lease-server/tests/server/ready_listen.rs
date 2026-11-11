//! `--ready-listen` and `server.ready_listen`: the setting, the loopback rule, an address in
//! use, the route's answer in each phase of a server's life, and a server that waits for the
//! store's writer lock instead of refusing to start. A server without the setting keeps its
//! refusal of a held lock (the tests of `store.rs` and `store_postgres.rs`).
//!
//! Nothing here sleeps or measures time: a retry is a step of a ticker the test owns, a drain
//! is held open by a request that is never finished, and the program is read line by line.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::process::Stdio;

use js_lease_server::{
    Config, Running, SharedClock, StartupError, channel_ticker, start, start_waiting,
};
use js_state::{WriterLock, WriterTarget};
use js_types::ManualClock;
use rustix::process::{Pid, Signal, kill_process};
use tokio::net::TcpStream;

use crate::agents_support::agent_args;
use crate::binary::{Guard, Lines, address_in, launch, launch_with_ready, spawn};
use crate::config_support::{PoolText, file, load, refusal, server};
use crate::limits::stall_in_body;
use crate::store_support::{sqlite_args_over, sqlite_db, sqlite_line, store_args, taken_address};
use crate::support::{LogCapture, Scratch, TOKEN, base_args, config, replace, request};

/// The body of the route while the server waits for the store's writer lock.
pub const WAITING: &str = "waiting for the store's writer lock";

/// A file of one joined pool, with `extra` in `[server]`.
fn static_file(scratch: &Scratch, extra: &str) -> String {
    let hosts = PoolText::joined("hosts");
    file(&server(scratch, extra), &[&hosts])
}

/// `args` with the readiness route on `listen`.
pub fn with_ready(mut args: Vec<OsString>, listen: &str) -> Vec<OsString> {
    args.extend(["--ready-listen", listen].map(OsString::from));
    args
}

/// The flags of a server over the SQLite store `scratch` holds, with the route on a port the
/// system chooses.
fn waiting_args(scratch: &Scratch) -> Vec<OsString> {
    let args = store_args(scratch, &sqlite_line(scratch), &["--allow-sqlite-store"]);
    with_ready(args, "127.0.0.1:0")
}

/// The status and body the route answers.
async fn readyz(addr: SocketAddr) -> (u16, String) {
    let reply = request(addr, "GET", "/readyz", None, None).await;
    (reply.status, reply.body)
}

/// What the refusal of a start says: its flag and its message.
fn refused<T>(result: Result<T, StartupError>) -> (&'static str, String) {
    let err = result.err().expect("the start should be refused");
    (err.flag(), err.to_string())
}

/// Starts the server `config` describes on clocks and tickers that stand still.
async fn started(config: Config) -> Result<Running, StartupError> {
    let (_, ticker) = channel_ticker();
    start(config, SharedClock::new(ManualClock::default()), ticker).await
}

/// Sends SIGTERM to `child`.
fn sigterm(child: &Guard) {
    kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
}

/// Asserts that the flag and the file key give the same address, that neither given is off,
/// and that the key must be an address. It catches a key read and dropped, and a route that
/// is on without either.
#[test]
fn ready_listen_flag_and_config_key_agree() {
    let scratch = Scratch::new("ready_listen_agree");
    let want: SocketAddr = "127.0.0.1:9101".parse().unwrap();
    let by_flag = config(with_ready(base_args(&scratch), "127.0.0.1:9101")).unwrap();
    assert_eq!(by_flag.ready_listen(), Some(want));
    let by_key = load(
        &scratch,
        &static_file(&scratch, "ready_listen = \"127.0.0.1:9101\""),
    );
    assert_eq!(by_key.unwrap().ready_listen(), Some(want));
    assert_eq!(config(base_args(&scratch)).unwrap().ready_listen(), None);
    let without = load(&scratch, &static_file(&scratch, "")).unwrap();
    assert_eq!(without.ready_listen(), None);
    assert_eq!(
        refusal(&scratch, &static_file(&scratch, "ready_listen = 9101")),
        "server.ready_listen: must be a string"
    );
}

/// Asserts the address rule: the route takes no token, so an address off the loopback is
/// refused at start naming `--ready-listen` or `server.ready_listen`, with the whole reason
/// (a private address and a wildcard included), and a loopback address of either family is
/// accepted without TLS. It catches a route served on a network, and a rule that is the
/// metrics' (private addresses) instead.
#[test]
fn a_ready_address_off_the_loopback_is_refused_naming_the_flag() {
    let scratch = Scratch::new("ready_listen_loopback");
    let reason = "is not a loopback address: the readiness route takes no token, so it is served on a loopback address only";
    for outside in ["10.0.0.5:9101", "0.0.0.0:9101", "[2001:db8::1]:9101"] {
        assert_eq!(
            refused(config(with_ready(base_args(&scratch), outside))),
            (
                "--ready-listen",
                format!("--ready-listen: {outside} {reason}")
            ),
            "{outside}"
        );
        let key = format!("ready_listen = \"{outside}\"");
        assert_eq!(
            refusal(&scratch, &static_file(&scratch, &key)),
            format!("server.ready_listen: {outside} {reason}"),
        );
    }
    for loopback in ["127.0.0.1:9101", "127.0.0.2:9101", "[::1]:9101"] {
        assert!(
            config(with_ready(base_args(&scratch), loopback)).is_ok(),
            "{loopback}"
        );
        let key = format!("ready_listen = \"{loopback}\"");
        assert!(
            load(&scratch, &static_file(&scratch, &key)).is_ok(),
            "{loopback}"
        );
    }
}

/// Asserts that a readiness address already in use refuses the start naming the flag or the
/// key that gave it, whether the start binds it or the caller did first with
/// `Config::bind_ready`, and that a refused start leaves the store's writer lock free. It
/// catches a bind failure reported as another flag's, and a lock held by a start that
/// failed.
#[tokio::test]
async fn a_ready_address_in_use_is_refused_naming_the_flag() {
    let busy = taken_address();
    let taken = busy.local_addr().unwrap().to_string();
    let scratch = Scratch::new("ready_listen_in_use");
    let args = with_ready(
        store_args(&scratch, &sqlite_line(&scratch), &["--allow-sqlite-store"]),
        &taken,
    );
    let flag = config(args.clone()).unwrap();
    assert_eq!(refused(started(flag).await).0, "--ready-listen");
    let target = WriterTarget::Sqlite(sqlite_db(&scratch));
    drop(WriterLock::acquire(&target).expect("the refused start left the lock free"));
    let mut bind_first = config(args).unwrap();
    assert_eq!(
        refused(bind_first.bind_ready()),
        ("--ready-listen", refused_text("--ready-listen", &busy))
    );

    let other = Scratch::new("ready_listen_in_use_file");
    let key = format!("listen = \"127.0.0.1:0\"\nready_listen = {taken:?}");
    let key = load(&other, &static_file(&other, &key)).unwrap();
    assert_eq!(refused(started(key).await).0, "server.ready_listen");
    let again = format!("listen = \"127.0.0.1:0\"\nready_listen = {taken:?}");
    let mut again = load(&other, &static_file(&other, &again)).unwrap();
    assert_eq!(refused(again.bind_ready()).0, "server.ready_listen");
}

/// The refusal of binding the address `busy` holds, as the system words it.
fn refused_text(flag: &str, busy: &std::net::TcpListener) -> String {
    let err = std::net::TcpListener::bind(busy.local_addr().unwrap()).unwrap_err();
    format!("{flag}: {err}")
}

/// Asserts that `bind_ready` gives the address the system chose for port 0 and that the
/// started server serves the route there, and that without the setting it binds nothing and
/// the server has no readiness address. It catches a start that binds the address a second
/// time, and a route reported on the wrong port.
#[tokio::test]
async fn bind_ready_names_the_port_the_start_serves() {
    let scratch = Scratch::new("ready_listen_bind");
    let mut with = config(with_ready(base_args(&scratch), "127.0.0.1:0")).unwrap();
    let ready = with.bind_ready().unwrap().unwrap();
    assert_ne!(ready.port(), 0);
    let running = started(with).await.unwrap();
    assert_eq!(running.ready_addr(), Some(ready));
    assert_eq!(readyz(ready).await, (200, "ready".to_owned()));
    running.shutdown().await.unwrap();

    let mut without = config(base_args(&scratch)).unwrap();
    assert_eq!(without.bind_ready().unwrap(), None);
    let running = started(without).await.unwrap();
    assert_eq!(running.ready_addr(), None);
    running.shutdown().await.unwrap();

    let unbound = config(with_ready(base_args(&scratch), "127.0.0.1:0")).unwrap();
    let running = started(unbound).await.unwrap();
    let chosen = running.ready_addr().unwrap();
    assert_eq!(readyz(chosen).await, (200, "ready".to_owned()));
    running.shutdown().await.unwrap();
}

/// Asserts the route over a server without a store: 200 `ready` once it serves, any other
/// path and a POST refused, then from the start of the stop 503 `draining` while a request
/// still open holds the drain (the route is read after the stop began and before the open
/// request ended), and the route's listener closed once the stop is done. It catches a route
/// that closes with the other listeners, one that stays ready while draining, and a route
/// that answers anything but its own path.
#[tokio::test]
async fn the_route_is_ready_while_serving_and_draining_from_the_stop_until_it_closes() {
    let scratch = Scratch::new("ready_listen_run");
    let mut config = config(with_ready(agent_args(&scratch), "127.0.0.1:0")).unwrap();
    let ready = config.bind_ready().unwrap().unwrap();
    let h = crate::support::Harness::over(scratch, config).await;
    assert_eq!(readyz(ready).await, (200, "ready".to_owned()));
    for path in ["/", "/healthz", "/metrics", "/readyz/extra"] {
        let other = request(ready, "GET", path, None, None).await;
        assert_eq!(other.status, 404, "{path}");
    }
    assert_eq!(
        request(ready, "POST", "/readyz", None, None).await.status,
        405
    );
    assert_eq!(
        request(h.addr(), "GET", "/readyz", Some(TOKEN), None)
            .await
            .status,
        404,
        "the lease port does not serve the route"
    );

    let stalled = stall_in_body(h.addr()).await;
    let mut stopping = Box::pin(h.running.shutdown());
    // Biased: the stop runs first, to its wait for the open request, then the route is read.
    let draining = tokio::select! {
        biased;
        ended = &mut stopping => panic!("the stop ended with a request open: {ended:?}"),
        draining = readyz(ready) => draining,
    };
    assert_eq!(draining, (503, "draining".to_owned()));
    drop(stalled);
    stopping.await.unwrap();
    assert!(TcpStream::connect(ready).await.is_err());
}

/// Asserts a server that waits: while another holder has the store's writer lock the route
/// answers 503 with the waiting body, the store is not opened, the state file not made and
/// the wait is logged once however many tries it takes; when the holder lets go and the
/// server tries again, it serves and the route answers 200 `ready`. It catches a start that
/// refuses a held lock when the route is on, a wait that logs on every try, one that opens
/// the store before the lock, and a route that does not follow the start.
#[tokio::test]
async fn a_server_waits_for_a_held_lock_and_serves_when_it_is_released() {
    let scratch = Scratch::new("ready_listen_wait");
    let logs = LogCapture::start();
    let target = WriterTarget::Sqlite(sqlite_db(&scratch));
    let held = WriterLock::acquire(&target).unwrap();
    let mut config = config(waiting_args(&scratch)).unwrap();
    let ready = config.bind_ready().unwrap().unwrap();
    let (_ticks, ticker) = channel_ticker();
    let (_checks, check_ticker) = channel_ticker();
    let (retry, retries) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let starting = start_waiting(config, clock, ticker, check_ticker, retries);
    let watching = async {
        assert_eq!(readyz(ready).await, (503, WAITING.to_owned()));
        retry.step().await;
        assert_eq!(readyz(ready).await, (503, WAITING.to_owned()));
        assert!(!sqlite_db(&scratch).exists(), "the store was not opened");
        assert!(!scratch.state().exists());
        drop(held);
        retry.step().await;
    };
    let (started, ()) = tokio::join!(starting, watching);
    let running = started.unwrap();
    assert_eq!(readyz(ready).await, (200, "ready".to_owned()));
    let health = request(running.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    assert_eq!(logs.text().matches(WAITING).count(), 1, "{}", logs.text());
    running.shutdown().await.unwrap();
    assert!(
        WriterLock::acquire(&target).is_ok(),
        "the stop gave the lock back"
    );
}

/// Asserts that a waiting server takes the lock before it binds any other listener: its
/// listen address is taken, and while it waits (after a try that found the lock held) its
/// route still answers and the start has not been refused naming `--listen`; once the lock
/// is free and taken, the bind that follows is the one refused, naming `--listen`, and
/// that start leaves the lock free. It catches a start that binds `--listen` before the lock.
#[tokio::test]
async fn a_waiting_server_binds_no_other_listener_until_it_has_the_lock() {
    let scratch = Scratch::new("ready_listen_order");
    let target = WriterTarget::Sqlite(sqlite_db(&scratch));
    let held = WriterLock::acquire(&target).unwrap();
    let listen = taken_address();
    let at = listen.local_addr().unwrap().to_string();
    let mut config = config(replace(waiting_args(&scratch), "--listen", at)).unwrap();
    let ready = config.bind_ready().unwrap().unwrap();
    let (_ticks, ticker) = channel_ticker();
    let (_checks, check_ticker) = channel_ticker();
    let (retry, retries) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let starting = start_waiting(config, clock, ticker, check_ticker, retries);
    let watching = async {
        retry.step().await;
        assert_eq!(readyz(ready).await, (503, WAITING.to_owned()));
        drop(held);
        retry.step().await;
    };
    let (started, ()) = tokio::join!(starting, watching);
    assert_eq!(refused(started).0, "--listen");
    drop(WriterLock::acquire(&target).expect("the refused start left the lock free"));
}

/// Asserts that a lock that cannot be tried at all (the store's directory is missing) is
/// refused at once with the same message with the route on as with it off, the retry ticker
/// never stepped: only a lock another process holds is waited for. It catches a wait that
/// retries a failure that no retry mends.
#[tokio::test]
async fn a_lock_that_cannot_be_tried_is_refused_not_waited_for() {
    let scratch = Scratch::new("ready_listen_failed");
    let line = format!("sqlite:{}", scratch.path("missing/state.db").display());
    let plain = store_args(&scratch, &line, &["--allow-sqlite-store"]);
    let ready = with_ready(plain.clone(), "127.0.0.1:0");
    let without = refused(started(config(plain).unwrap()).await);
    assert_eq!(without.0, "--store");
    assert!(
        without
            .1
            .starts_with("--store: the store's writer lock could not be taken: "),
        "{}",
        without.1
    );
    let mut config = config(ready).unwrap();
    config.bind_ready().unwrap().unwrap();
    let (_ticks, ticker) = channel_ticker();
    let (_checks, check_ticker) = channel_ticker();
    let (_retry, retries) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let waiting = start_waiting(config, clock, ticker, check_ticker, retries).await;
    assert_eq!(refused(waiting), without);
}

/// Asserts that without `--ready-listen` a lock another process holds refuses the start at
/// once, naming `--store`, and that a retry ticker given to the start is not consulted: the
/// refusal comes without a step. It catches a start that waits whenever it is given a retry
/// ticker.
#[tokio::test]
async fn without_a_route_a_held_lock_refuses_the_start_at_once() {
    let scratch = Scratch::new("ready_listen_off");
    let target = WriterTarget::Sqlite(sqlite_db(&scratch));
    let _held = WriterLock::acquire(&target).unwrap();
    let config = config(store_args(
        &scratch,
        &sqlite_line(&scratch),
        &["--allow-sqlite-store"],
    ))
    .unwrap();
    let (_ticks, ticker) = channel_ticker();
    let (_checks, check_ticker) = channel_ticker();
    let (_retry, retries) = channel_ticker();
    let clock = SharedClock::new(ManualClock::default());
    let result = start_waiting(config, clock, ticker, check_ticker, retries).await;
    assert_eq!(
        refused(result),
        (
            "--store",
            "--store: another process holds the store's writer lock".to_owned()
        )
    );
}

/// Asserts the program with the route on, beside a first server that holds the store: the
/// second prints the route's address before anything else, answers 503 with the waiting
/// body, makes no state file, and, once the first stops, takes the lock, prints the address it
/// listens on, answers 200 `ready` and serves; SIGTERM then stops it with status 0. It
/// catches a program that refuses a held lock when the route is on, one that prints its
/// address before it holds the lock, and a route that does not follow the start.
#[tokio::test]
async fn a_second_server_with_a_route_waits_for_the_first_and_takes_over() {
    let first = Scratch::new("ready_proc_first");
    let (mut holder, _) = launch(&sqlite_args_over(&first, &first), Stdio::null());
    let second = Scratch::new("ready_proc_second");
    let args = with_ready(sqlite_args_over(&second, &first), "127.0.0.1:0");
    let (mut waiting, lines) = spawn(&args, Stdio::null());
    let ready = address_in(&lines.next(), "ready on ");
    assert_eq!(readyz(ready).await, (503, WAITING.to_owned()));
    assert!(!second.state().exists());

    sigterm(&holder);
    assert!(holder.0.wait().unwrap().success());
    let listen = address_in(&lines.next(), "listening on ");
    assert_eq!(readyz(ready).await, (200, "ready".to_owned()));
    let health = request(listen, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    sigterm(&waiting);
    assert!(waiting.0.wait().unwrap().success());
    assert_eq!(lines.rest(), Vec::<String>::new());
}

/// Asserts that SIGTERM to a server that waits for the lock ends it with status 0, with
/// nothing printed after the route's address (no address it listens on), no state file
/// made and the holder's lock untouched. It catches a signal that is not handled until the
/// start returns, which would kill the process by the signal.
#[tokio::test]
async fn sigterm_to_a_waiting_server_exits_0_without_the_lock() {
    let scratch = Scratch::new("ready_proc_term_waiting");
    let target = WriterTarget::Sqlite(sqlite_db(&scratch));
    let held = WriterLock::acquire(&target).unwrap();
    let (mut child, lines) = spawn(&waiting_args(&scratch), Stdio::null());
    let ready = address_in(&lines.next(), "ready on ");
    assert_eq!(readyz(ready).await, (503, WAITING.to_owned()));
    sigterm(&child);
    assert_eq!(child.0.wait().unwrap().code(), Some(0));
    assert_eq!(lines.rest(), Vec::<String>::new());
    assert!(!scratch.state().exists());
    drop(held);
    assert!(WriterLock::acquire(&target).is_ok());
}

/// Asserts that SIGTERM to a server that serves makes the route answer 503 `draining` while
/// a request still open holds the drain, and that the server exits 0 once that request
/// ends. The test reads the route after the server logged that it drains, an event, not a
/// pause. It catches a route that stays ready, or closes, while the server drains.
#[tokio::test]
async fn sigterm_to_a_serving_server_makes_the_route_say_draining_until_it_exits() {
    let scratch = Scratch::new("ready_proc_drain");
    let args = with_ready(agent_args(&scratch), "127.0.0.1:0");
    let (mut child, listen, ready) = launch_with_ready(&args, Stdio::piped());
    let ready = ready.expect("the program printed the route's address");
    let logged = Lines::of(child.0.stderr.take().unwrap());
    assert_eq!(readyz(ready).await, (200, "ready".to_owned()));
    let stalled = stall_in_body(listen).await;
    sigterm(&child);
    logged.until("the readiness route answers 503");
    assert_eq!(readyz(ready).await, (503, "draining".to_owned()));
    drop(stalled);
    assert!(child.0.wait().unwrap().success());
}
