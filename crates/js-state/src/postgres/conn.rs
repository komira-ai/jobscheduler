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
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use postgres::error::SqlState;
use postgres::types::ToSql;
use postgres::{Client, Config, Row, Statement, ToStatement};

use super::tls::{Tls, TlsError};

/// Runs `f` on a thread that is not inside a tokio runtime and returns its result. A
/// panic in `f` is raised again in the caller.
pub(super) fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    if tokio::runtime::Handle::try_current().is_err() {
        return f();
    }
    thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => resume_unwind(panic),
    })
}

type Params<'a> = &'a [&'a (dyn ToSql + Sync)];

/// How to make a connection: the client's settings with the network limits and the TLS mode
/// in them, the TLS settings, and the statements a new session runs first.
#[derive(Clone)]
pub(super) struct Dial {
    pub(super) pg: Config,
    pub(super) tls: Tls,
    pub(super) session: String,
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
        let client = off_runtime(|| dial.pg.connect(connector)).map_err(|err| {
            match dial.tls.refusal(err, started.load(Ordering::Relaxed)) {
                Ok(tls) => DialError::Tls(tls),
                Err(err) => DialError::Pg(err),
            }
        })?;
        let mut conn = Conn {
            client: Some(client),
        };
        conn.batch_execute(&dial.session).map_err(DialError::Pg)?;
        Ok(conn)
    }

    fn client(&mut self) -> &mut Client {
        self.client
            .as_mut()
            .expect("the client is present until the connection is dropped")
    }

    /// Whether the server closed the connection.
    pub fn is_closed(&self) -> bool {
        self.client.as_ref().is_none_or(postgres::Client::is_closed)
    }

    /// Prepares a statement.
    ///
    /// # Errors
    /// The client's error.
    pub fn prepare(&mut self, query: &str) -> Result<Statement, postgres::Error> {
        let client = self.client();
        off_runtime(|| client.prepare(query))
    }

    /// Runs statements that take no parameters and return no rows.
    ///
    /// # Errors
    /// The client's error.
    pub fn batch_execute(&mut self, query: &str) -> Result<(), postgres::Error> {
        let client = self.client();
        off_runtime(|| client.batch_execute(query))
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
        let client = self.client();
        off_runtime(|| client.execute(query, params))
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
        let client = self.client();
        off_runtime(|| client.query(query, params))
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
        let client = self.client();
        off_runtime(|| client.query_one(query, params))
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
        let client = self.client();
        off_runtime(|| client.query_opt(query, params))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Dropping the client drops its runtime, which panics inside another one.
        self.client
            .take()
            .into_iter()
            .for_each(|client| off_runtime(move || drop(client)));
    }
}

#[cfg(test)]
mod tests {
    use super::off_runtime;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    }

    // One call site for every case: the coverage of a generic function is that of its
    // best single instantiation, so the three paths (no runtime, a runtime, a panic) go
    // through the same closure.
    fn on_a_thread(panics: bool) -> std::thread::ThreadId {
        off_runtime(move || {
            assert!(!panics, "the client broke");
            std::thread::current().id()
        })
    }

    // Catches a call that runs on the runtime's own thread (the client would panic with
    // "Cannot start a runtime from within a runtime"): inside a runtime the work moves to
    // another thread, outside it stays.
    #[test]
    fn a_call_inside_a_runtime_runs_on_another_thread() {
        let here = std::thread::current().id();
        assert_eq!(on_a_thread(false), here);
        let inside = runtime().block_on(async { on_a_thread(false) });
        assert_ne!(inside, here);
    }

    // Catches a panic in the moved call being swallowed (the caller would carry on with
    // a result that does not exist): it must come out in the caller, with its message.
    #[test]
    fn a_panic_in_a_call_moved_off_the_runtime_is_raised_in_the_caller() {
        let caught = std::panic::catch_unwind(|| runtime().block_on(async { on_a_thread(true) }));
        let panic = caught.expect_err("the panic comes out");
        let said = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied());
        assert_eq!(said, Some("the client broke"));
    }
}
