//! Whether the host can take a job, for the process that supervises the agent.
//!
//! The agent that is tainted answers every start with a refusal, and the lease server that
//! holds a taint for the host places no job on it. Both are quiet from outside: the unit is
//! active and the heartbeats go on. With `--health-listen` the agent says it on a route of
//! the loopback, which hostkeeper reads as the package's `healthUrl` (a `2xx` is healthy,
//! anything else is reported as the service being unhealthy), so a host that cannot take a
//! job is shown as unhealthy and holds a rollout's bake instead of looking well.
//!
//! # The verdict
//!
//! [`Health`] holds what the session knows and gives one [`Verdict`]: the host is placeable,
//! or it is not, with the first of these reasons that holds:
//!
//! 1. the session holds no welcome of the lease server: it was never admitted, or the server
//!    lost the session and the agent has not said hello again;
//! 2. the lease server holds a taint for the host (the `host_taint` of the last reply it
//!    accepted, with its reason, check and run id);
//! 3. the agent holds a taint (the `tainted` of the last heartbeat it sent), with what changed
//!    in one line when the check that found it said so (`the agent holds a taint on this host:
//!    <detail>`; the detail's control characters are spaces, so the reason stays one line).
//!
//! The session feeds it ([`Session::with_health`](crate::Session::with_health)) and nothing
//! else does. It never acts on the verdict: the taints are shown, not obeyed, so a verdict
//! that is wrong cannot refuse a job or end a taint.
//!
//! Every change of the verdict is one line for the journal ([`Health::take_lines`], printed
//! by the loop that drives the session), so the cause of an unhealthy report is in the log:
//! `health: placeable`, or `health: not placeable: <reason>`. The first verdict is a line too.
//!
//! # The route
//!
//! [`Route`] is a listener on a loopback address. `GET /healthz` answers `200` with the body
//! `placeable` and a newline, or `503` with `not placeable: <reason>` and a newline; any other
//! request is `404`. Only the first line of a request is judged, at most [`REQUEST_CAP`] bytes
//! of it: a line that does not end within the cap, and a client that closes or says nothing for
//! [`READ_LIMIT`], are answered `404` too. After the answer the rest of the request is read and
//! dropped (at most [`DRAIN_CAP`] bytes) until the client closes, so that the close does not
//! reset the connection under the answer. The route answers one connection at a time, each
//! read bounded by [`READ_LIMIT`].
//! An address that is not a loopback one is refused when the route is bound: the verdict is for
//! the host's own supervisor, and the route has no authentication.

use std::fmt;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::Taint;

/// The most bytes of a request the route reads before it answers: a request line is short, and
/// a client that sends more without ending the line is answered `404` and closed.
pub const REQUEST_CAP: usize = 1024;

/// How long the route waits for a silent client before it answers `404` and closes. It bounds
/// a read; the answer is written after the read, so a client that sends nothing costs this
/// once.
pub const READ_LIMIT: Duration = Duration::from_secs(2);

/// The path of the route.
pub const PATH: &str = "/healthz";

/// The most bytes of a request read after its answer was written, until the client closes,
/// each read bounded by [`READ_LIMIT`].
const DRAIN_CAP: u64 = 64 * 1024;

/// The reason while the session holds no welcome.
const NO_SESSION: &str = "no session with the lease server";

/// The reason while the agent's own taint is held.
const OWN_TAINT: &str = "the agent holds a taint on this host";

/// Whether the host can take a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The agent is admitted and neither it nor the lease server holds a taint.
    Placeable,
    /// The host cannot take a job, for this reason.
    NotPlaceable(String),
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placeable => f.write_str("placeable"),
            Self::NotPlaceable(why) => write!(f, "not placeable: {why}"),
        }
    }
}

/// What the session told, and the lines not yet printed.
#[derive(Default)]
struct State {
    /// The session holds a welcome.
    admitted: bool,
    /// The last heartbeat sent said the agent holds a taint.
    own: bool,
    /// What changed, in one line, as the agent's taint says it; none when it says none.
    own_detail: Option<String>,
    /// The taint the last accepted reply said the lease server holds.
    server: Option<Taint>,
    /// The verdict the last line was printed for.
    logged: Option<Verdict>,
    /// The lines of the changes since [`Health::take_lines`].
    pending: Vec<String>,
}

impl State {
    /// The verdict of what is known: the first reason that holds, in the order of the module
    /// documentation.
    fn verdict(&self) -> Verdict {
        let reasons = [
            (!self.admitted).then_some(NO_SESSION.to_owned()),
            self.server.as_ref().map(server_reason),
            self.own.then(|| own_reason(self.own_detail.as_deref())),
        ];
        reasons
            .into_iter()
            .flatten()
            .next()
            .map_or(Verdict::Placeable, Verdict::NotPlaceable)
    }

    /// Keeps a line for the verdict when it is not the one the last line was for.
    fn settle(&mut self) {
        let now = self.verdict();
        if self.logged.as_ref() != Some(&now) {
            self.pending.push(format!("health: {now}"));
            self.logged = Some(now);
        }
    }
}

/// The reason the agent's own taint gives: the fixed words, and `detail` after them when the
/// taint has one.
fn own_reason(detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{OWN_TAINT}: {detail}"),
        None => OWN_TAINT.to_owned(),
    }
}

/// `text` as one line: each control character (a line break included) is a space.
pub(crate) fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The reason the lease server's taint gives: what it names, and nothing else of it.
fn server_reason(taint: &Taint) -> String {
    let parts = [
        Some(format!("reason {}", taint.reason)),
        taint.check.as_ref().map(|check| format!("check {check}")),
        taint.run_id.as_ref().map(|run| format!("run {run}")),
    ];
    let named: Vec<String> = parts.into_iter().flatten().collect();
    format!(
        "the lease server holds a taint on this host ({})",
        named.join(", ")
    )
}

/// What the session knows about the host's taints, shared between the session's thread, which
/// feeds it, and the route's, which reads the verdict (see the module documentation).
pub struct Health {
    state: Mutex<State>,
}

impl Default for Health {
    /// A host that has no session yet: not placeable, and the first line is waiting.
    fn default() -> Self {
        let health = Self {
            state: Mutex::new(State::default()),
        };
        health.update(|_| ());
        health
    }
}

impl Health {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state with `change` and keeps a line when the verdict changed.
    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut guard = self.lock();
        let state: &mut State = &mut guard;
        change(state);
        state.settle();
    }

    /// The session holds a welcome (`true`), or lost it (`false`).
    pub fn admitted(&self, admitted: bool) {
        self.update(|state| state.admitted = admitted);
    }

    /// Whether the heartbeat just sent said the agent holds a taint, and what changed when the
    /// check that found it said so (`detail`, taken as one line: its control characters are
    /// spaces, and a detail that is blank says nothing). The detail of a host that is not
    /// tainted is not kept.
    pub fn own_taint(&self, tainted: bool, detail: Option<&str>) {
        let detail = detail
            .map(one_line)
            .filter(|line| !line.trim().is_empty())
            .filter(|_| tainted);
        self.update(|state| {
            state.own = tainted;
            state.own_detail = detail;
        });
    }

    /// The taint the lease server holds for the host, as the last reply it sent said; `None`
    /// when the reply named none.
    pub fn server_taint(&self, taint: Option<Taint>) {
        self.update(|state| state.server = taint);
    }

    /// The verdict now.
    pub fn verdict(&self) -> Verdict {
        self.lock().verdict()
    }

    /// The lines of the changes of the verdict since the last call, oldest first, each
    /// without the program's prefix.
    pub fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().pending)
    }
}

/// The text of an answer: its status line, and the body with a newline.
fn render(verdict: &Verdict) -> String {
    let (status, text) = match verdict {
        Verdict::Placeable => (200, "OK"),
        Verdict::NotPlaceable(_) => (503, "Service Unavailable"),
    };
    answer(status, text, &format!("{verdict}\n"))
}

/// A complete answer that closes the connection.
fn answer(status: u16, text: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The first line of the request `stream` sends, without its newline: empty when the line does
/// not end within [`REQUEST_CAP`] bytes, or the client closes or stops sending first, or a read
/// fails. At most [`REQUEST_CAP`] bytes are read.
fn request_line(stream: &mut impl Read) -> Vec<u8> {
    let mut buffer = [0u8; REQUEST_CAP];
    let mut len = 0;
    while len < REQUEST_CAP && !buffer[..len].contains(&b'\n') {
        match stream.read(&mut buffer[len..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => len += read,
        }
    }
    let end = buffer[..len].iter().position(|byte| *byte == b'\n');
    end.map_or_else(Vec::new, |end| buffer[..end].to_vec())
}

/// What the route answers a request whose first line is `line`.
fn respond(health: &Health, line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut words = text.split_whitespace();
    match (words.next(), words.next()) {
        (Some("GET"), Some(PATH)) => render(&health.verdict()),
        _ => answer(404, "Not Found", "not found\n"),
    }
}

/// Reads one request from `stream` and answers it.
fn serve_one<S: Read + Write>(health: &Health, stream: &mut S) {
    let line = request_line(stream);
    let _ = stream.write_all(respond(health, &line).as_bytes());
}

/// A listener on a loopback address that answers [`PATH`] (see the module documentation).
pub struct Route {
    listener: TcpListener,
    addr: SocketAddr,
}

impl Route {
    /// Listens on `addr`, which must be a loopback address; port 0 takes a free port, which
    /// [`Route::addr`] then names.
    ///
    /// # Errors
    ///
    /// The text of the refusal, which names `--health-listen`: the address is not a loopback
    /// one, or it cannot be bound.
    pub fn bind(addr: SocketAddr) -> Result<Self, String> {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "--health-listen: {addr} is not a loopback address: the route has no \
                 authentication and is for the host's own supervisor"
            ));
        }
        let listener = TcpListener::bind(addr)
            .map_err(|why| format!("--health-listen: cannot listen on {addr}: {why}"))?;
        let addr = listener.local_addr().unwrap_or(addr);
        Ok(Self { listener, addr })
    }

    /// The address the route listens on, with the port that was taken.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Answers connections one at a time until `stop` is set and a connection arrives
    /// ([`Route::wake`] makes one), each read for at most [`READ_LIMIT`]. After its answer a
    /// connection's write side is shut and what is left of the request is read, at most
    /// [`DRAIN_CAP`] bytes, until the client closes.
    pub fn serve(&self, health: &Health, stop: &AtomicBool) {
        for mut stream in self.listener.incoming().flatten() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let _ = stream.set_read_timeout(Some(READ_LIMIT));
            let _ = stream.set_write_timeout(Some(READ_LIMIT));
            serve_one(health, &mut stream);
            // The rest of the request is read before the stream closes: a close with unread
            // input resets the connection, which can lose the answer at the client.
            let _ = stream.shutdown(Shutdown::Write);
            let _ = std::io::copy(&mut (&stream).take(DRAIN_CAP), &mut std::io::sink());
        }
    }

    /// Makes the route's thread look at its stop flag: a connection that is closed at once.
    pub fn wake(&self) {
        let _ = TcpStream::connect(self.addr);
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
