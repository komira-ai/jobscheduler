//! The blocking `postgres` client, made safe to call from a thread that is inside a tokio
//! runtime.
//!
//! `postgres::Client` owns a private current-thread runtime and `block_on`s it for every
//! call, connect and drop. Tokio panics ("Cannot start a runtime from within a runtime")
//! when that happens on a thread a runtime is running tasks on, and a panic in a
//! destructor during unwinding aborts the process. The store's calls are blocking by
//! design (as the SQLite store's are) and the scheduler calls them from async tasks, so
//! every client call goes through [`off_runtime`]: where a runtime is current, the call
//! runs on a short-lived scoped thread that has none, and the caller waits for it, as it
//! would wait on the socket. Where no runtime is current the call runs in place.
//!
//! [`Conn::connect`] is the one place a connection is made: with the configuration's TLS
//! ([`super::tls`]) and network limits, and, for the store's own sessions, its server limits
//! ([`super::timeouts`]).

use std::error::Error as _;
use std::io;
use std::panic::resume_unwind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use postgres::error::SqlState;
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::ToSql;
use postgres::{Client, Config, Row, Statement, ToStatement};

use super::tls::{Tls, TlsError};
use crate::Notice;

/// Runs `f` on a thread that is not inside a tokio runtime and returns its result, adding one
/// to `moved` for each thread started for it. A panic in `f` is raised again in the caller.
pub(super) fn off_runtime<R: Send>(moved: &AtomicU64, f: impl FnOnce() -> R + Send) -> R {
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    moved.fetch_add(1, Ordering::Relaxed);
    thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => resume_unwind(panic),
    })
}

type Params<'a> = &'a [&'a (dyn ToSql + Sync)];

/// How to make a connection: the client's settings with the network limits and the TLS mode
/// in them, the TLS settings, the statements a new session runs first, the count every
/// connection made from it adds each message it sends to, and the count it adds each thread
/// it starts for a call to ([`Conn`]).
#[derive(Clone)]
pub(super) struct Dial {
    pub(super) pg: Config,
    pub(super) tls: Tls,
    pub(super) session: String,
    pub(super) sent: Arc<AtomicU64>,
    pub(super) moved: Arc<AtomicU64>,
}

/// Why [`Conn::connect`] failed.
#[derive(Debug)]
pub(super) enum DialError {
    /// The client's error: the network, the server's refusal, a session setting refused.
    Pg(postgres::Error),
    /// The TLS the configuration asks for could not be had.
    Tls(TlsError),
}

/// The first I/O error among the causes of `err`: the network failed under the call.
pub(super) fn io_cause(err: &postgres::Error) -> Option<&io::Error> {
    let mut cause = err.source();
    while let Some(e) = cause {
        if let Some(io) = e.downcast_ref::<io::Error>() {
            return Some(io);
        }
        cause = e.source();
    }
    None
}

/// Whether `err` means the connection is gone, so the call can be run again on a new one: the
/// client found it closed, the network failed under it, or the server ended the session (an
/// administrator's or a crash's shutdown, an idle timeout, a connection exception). A
/// statement the server refused (a syntax error, a lock wait or a statement timed out, a
/// serialization failure) leaves the connection as it was and is not one.
///
/// Which of the three a killed connection shows first depends on when the client reads the
/// server's last message, so every test is asked of every error (`|`, not `||`).
pub(super) fn connection_lost(err: &postgres::Error) -> bool {
    let ended = err
        .code()
        .is_some_and(|code| code.code().starts_with("08") | ENDED.contains(code));
    ended | err.is_closed() | io_cause(err).is_some()
}

/// The server's codes for a session it ended, outside class 08 (connection exception).
const ENDED: [SqlState; 5] = [
    SqlState::ADMIN_SHUTDOWN,
    SqlState::CRASH_SHUTDOWN,
    SqlState::CANNOT_CONNECT_NOW,
    SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
    SqlState::IDLE_SESSION_TIMEOUT,
];

/// A `postgres::Client` whose calls, and whose drop, are safe inside a runtime.
pub struct Conn {
    // Present until `drop`.
    client: Option<Client>,
    /// The messages that carried statements to the server, each a round trip: one per call of
    /// [`Conn::batch_execute`] (however many statements its text holds), [`Conn::execute`] and
    /// the `query` calls, counted with every other connection of the same [`Dial`]. A prepare
    /// is not counted.
    sent: Arc<AtomicU64>,
    /// The threads started to make a call, its connect and its drop included, off a runtime
    /// ([`off_runtime`]), counted with every other connection of the same [`Dial`].
    moved: Arc<AtomicU64>,
}

impl Conn {
    /// Connects as `dial` says and runs its session statements.
    ///
    /// # Errors
    /// [`DialError::Tls`] when the TLS the mode asks for could not be had, [`DialError::Pg`]
    /// for any other failure.
    pub(super) fn connect(dial: &Dial) -> Result<Conn, DialError> {
        let started = Arc::new(AtomicBool::new(false));
        let connector = dial.tls.connector(&started);
        let client = off_runtime(&dial.moved, || dial.pg.connect(connector)).map_err(|err| {
            match dial.tls.refusal(err, started.load(Ordering::Relaxed)) {
                Ok(tls) => DialError::Tls(tls),
                Err(err) => DialError::Pg(err),
            }
        })?;
        let mut conn = Conn {
            client: Some(client),
            sent: Arc::clone(&dial.sent),
            moved: Arc::clone(&dial.moved),
        };
        conn.batch_execute(&dial.session).map_err(DialError::Pg)?;
        Ok(conn)
    }

    /// The client, and the count of the threads started for its calls.
    fn client(&mut self) -> (&mut Client, &AtomicU64) {
        let client = self.client.as_mut();
        let client = client.expect("the client is present until the connection is dropped");
        (client, &self.moved)
    }

    /// The client, for a call that sends statements: the call is counted.
    fn sending(&mut self) -> (&mut Client, &AtomicU64) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.client()
    }

    /// Whether the server closed the connection.
    pub fn is_closed(&self) -> bool {
        self.client.as_ref().is_none_or(postgres::Client::is_closed)
    }

    /// Waits at most `wait` for the next notice of a channel this connection listens on (a
    /// notice already received first): `None` when none came, or when the connection is gone
    /// (a statement sent next tells which). Nothing is sent, so nothing is counted.
    ///
    /// # Errors
    /// The client's error: the connection failed while it waited.
    pub(super) fn notice(&mut self, wait: Duration) -> Result<Option<Notice>, postgres::Error> {
        let (client, moved) = self.client();
        let heard = off_runtime(moved, || client.notifications().timeout_iter(wait).next())?;
        Ok(heard.map(|notice| Notice {
            channel: notice.channel().to_owned(),
            payload: notice.payload().to_owned(),
        }))
    }

    /// Prepares a statement.
    ///
    /// # Errors
    /// The client's error.
    pub fn prepare(&mut self, query: &str) -> Result<Statement, postgres::Error> {
        let (client, moved) = self.client();
        off_runtime(moved, || client.prepare(query))
    }

    /// Runs statements that take no parameters and return no rows.
    ///
    /// # Errors
    /// The client's error.
    pub fn batch_execute(&mut self, query: &str) -> Result<(), postgres::Error> {
        let (client, moved) = self.sending();
        off_runtime(moved, || client.batch_execute(query))
    }

    /// Runs a statement; the number of rows it changed.
    ///
    /// # Errors
    /// The client's error.
    pub fn execute<T: ?Sized + ToStatement + Sync>(
        &mut self,
        query: &T,
        params: Params<'_>,
    ) -> Result<u64, postgres::Error> {
        let (client, moved) = self.sending();
        off_runtime(moved, || client.execute(query, params))
    }

    /// Runs a statement; its rows.
    ///
    /// # Errors
    /// The client's error.
    pub fn query<T: ?Sized + ToStatement + Sync>(
        &mut self,
        query: &T,
        params: Params<'_>,
    ) -> Result<Vec<Row>, postgres::Error> {
        let (client, moved) = self.sending();
        off_runtime(moved, || client.query(query, params))
    }

    /// Runs a statement that returns exactly one row.
    ///
    /// # Errors
    /// The client's error, and when the statement returns no row or more than one.
    pub fn query_one<T: ?Sized + ToStatement + Sync>(
        &mut self,
        query: &T,
        params: Params<'_>,
    ) -> Result<Row, postgres::Error> {
        let (client, moved) = self.sending();
        off_runtime(moved, || client.query_one(query, params))
    }

    /// Runs a statement that returns at most one row.
    ///
    /// # Errors
    /// The client's error, and when the statement returns more than one row.
    pub fn query_opt<T: ?Sized + ToStatement + Sync>(
        &mut self,
        query: &T,
        params: Params<'_>,
    ) -> Result<Option<Row>, postgres::Error> {
        let (client, moved) = self.sending();
        off_runtime(moved, || client.query_opt(query, params))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Dropping the client drops its runtime, which panics inside another one.
        self.client
            .take()
            .into_iter()
            .for_each(|client| off_runtime(&self.moved, move || drop(client)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::{self, ThreadId};

    use super::off_runtime;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    }

    fn multi_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("a runtime")
    }

    // One call site for every case: the coverage of a generic function is that of its
    // best single instantiation, so every path (no runtime, each kind of thread of a
    // runtime, a panic) goes through the same closure. The closure does what the client
    // does on every call: it blocks on a current-thread runtime of its own, which tokio
    // refuses ("Cannot start a runtime from within a runtime") where a runtime runs.
    fn on_a_thread(moved: &AtomicU64, panics: bool) -> ThreadId {
        off_runtime(moved, move || {
            runtime().block_on(async { assert!(!panics, "the client broke") });
            thread::current().id()
        })
    }

    // Catches a call that runs on a current-thread runtime's own thread (the client would
    // panic): there the work moves to a thread of its own, which is counted; outside any
    // runtime it stays, and nothing is counted.
    #[test]
    fn a_call_inside_a_current_thread_runtime_runs_on_a_thread_of_its_own() {
        let moved = AtomicU64::new(0);
        let here = thread::current().id();
        assert_eq!(on_a_thread(&moved, false), here);
        assert_eq!(moved.load(Ordering::Relaxed), 0);
        let inside = runtime().block_on(async { on_a_thread(&moved, false) });
        assert_ne!(inside, here);
        assert_eq!(moved.load(Ordering::Relaxed), 1);
    }

    // Catches a panic in the moved call being swallowed (the caller would carry on with
    // a result that does not exist): it must come out in the caller, with its message.
    #[test]
    fn a_panic_in_a_call_moved_off_the_runtime_is_raised_in_the_caller() {
        let moved = AtomicU64::new(0);
        let caught =
            std::panic::catch_unwind(|| runtime().block_on(async { on_a_thread(&moved, true) }));
        let panic = caught.expect_err("the panic comes out");
        let said = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied());
        assert_eq!(said, Some("the client broke"));
    }

    // Catches a thread started for every call the server makes: its store calls run on the
    // pool for blocking calls of a multi-thread runtime, where the client's own runtime may
    // run, so each call runs on the pool's thread that makes it and starts no thread. A
    // thread started per statement, thousands a second under load, is what kept every
    // thread of the server waking (the start and end of a thread take the process's memory
    // map lock, which every thread's page faults wait on).
    #[test]
    fn a_call_from_the_pool_for_blocking_calls_runs_on_that_thread_and_starts_none() {
        let moved = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&moved);
        let blocking = move || (thread::current().id(), on_a_thread(&counted, false));
        let (caller, ran_on) = multi_thread()
            .block_on(async { tokio::task::spawn_blocking(blocking).await })
            .unwrap();
        assert_eq!(ran_on, caller);
        assert_eq!(moved.load(Ordering::Relaxed), 0);
    }

    // Catches a call from a task of a multi-thread runtime, or from the thread that blocks
    // on one, that panics because the client blocks on its own runtime there, or that starts
    // a thread for it: the call runs where it is made, with the runtime's work handed to
    // another thread meanwhile, and a panic in it comes out in the task.
    #[test]
    fn a_call_from_a_task_of_a_multi_thread_runtime_runs_where_it_is_made() {
        let moved = Arc::new(AtomicU64::new(0));
        let runtime = multi_thread();
        let counted = Arc::clone(&moved);
        let task = async move { (thread::current().id(), on_a_thread(&counted, false)) };
        let (caller, ran_on) = runtime
            .block_on(async { tokio::spawn(task).await })
            .unwrap();
        assert_eq!(ran_on, caller);
        let here = thread::current().id();
        assert_eq!(runtime.block_on(async { on_a_thread(&moved, false) }), here);
        assert_eq!(moved.load(Ordering::Relaxed), 0);
        let counted = Arc::clone(&moved);
        let task = async move { on_a_thread(&counted, true) };
        let panicked = runtime.block_on(async { tokio::spawn(task).await });
        assert!(panicked.unwrap_err().is_panic());
    }
}
