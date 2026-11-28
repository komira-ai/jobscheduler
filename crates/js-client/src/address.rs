//! The server's address: the `--server` name looked up, and the address a handshake with the
//! server last completed on, which the requests made while the name cannot be looked up go to.
//!
//! ureq asks its resolver for the server's address before every request, the requests that
//! reuse a kept connection included. A host whose resolver cannot answer for a while (the
//! name is in a public zone and the site's uplink is down) could then send no heartbeat to a
//! server it reaches perfectly well. So each server has a [`Book`], shared by every client of
//! the process (the agent's session, each dispatch poll and the hold keeper make their own
//! clients), which remembers the address the last new connection completed its handshake on:
//! the TLS handshake, in which the certificate is checked against the name as always, or the
//! TCP connect for a plain-http loopback server. A request then goes to that address with no
//! lookup at all.
//!
//! The name is looked up again in the background, one lookup at a time, once [`REFRESH`] has
//! passed since the last one ended. An answer that holds the remembered address changes
//! nothing. One that does not is tried first by the next new connection, each address within
//! [`FRESH_CONNECT`] and then the remembered one, and an address of it is taken only once a
//! handshake with it completes. A remembered address that stops answering is not kept for
//! good: when it fails, the name is looked up on the spot and the other addresses of the
//! answer are tried, and until a connection to it completes again the next connections look
//! the name up first and try it last. With no address remembered (a first start), the name is looked up on the
//! spot, within the request's own limit. `--server-address` ([`give_server_address`]) gives
//! the first address, so a first start needs no lookup either.
//!
//! A line is logged when lookups start failing while an address is remembered, one when they
//! answer again, and one when the client moves to a new address; never one per request. The
//! memory is the process's: an agent that starts again looks the name up again, or starts from
//! `--server-address`.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::Dispatch;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::time::Duration as Budget;
use ureq::unversioned::transport::{ConnectionDetails, Connector, NextTimeout};
use ureq::{Error, Timeout};

use crate::error::ClientError;
use crate::server_url::ServerUrl;

/// How long after a lookup ends the name is looked up again in the background. The answer of
/// a lookup is not kept for its own lifetime (the resolver does not say it): a minute is well
/// under the lifetime a record of a server's name has, and one lookup a minute is nothing.
pub(crate) const REFRESH: Duration = Duration::from_secs(60);

/// The longest a connection to an address no handshake has completed on yet (a new answer of
/// the name) may take, so that one that does not answer leaves the rest of the request's limit
/// to the remembered address.
pub(crate) const FRESH_CONNECT: Duration = Duration::from_secs(5);

/// The most addresses ureq takes from a resolver.
const MAX_ADDRESSES: usize = 16;

/// The books of the process, one per server name and port, looked up with the system's
/// resolver.
static BOOKS: LazyLock<Books> = LazyLock::new(|| Books::new(Arc::new(SystemLookup), REFRESH));

/// The book of `server`, shared by every client of the process that talks to it.
pub(crate) fn book(server: &ServerUrl) -> Arc<Book> {
    BOOKS.book(server)
}

/// Gives the clients of this process `address` as the address of the host `server` names
/// (`--server-address`, given beside `--server`), on the port of `server`. Requests go to it,
/// with no lookup, until a handshake completes on an address a later lookup of the name gives;
/// the certificate is checked against the name as always. When no handshake completes on it,
/// the name is looked up. An address the clients already learned is kept: this gives the first
/// one only.
///
/// # Errors
///
/// [`ClientError::Flag`] naming `--server` when `server` is refused (see [`ServerUrl::parse`]),
/// or `--server-address` when `server` is plain `http://`: the address of a loopback server
/// is its name's, and a bearer token never goes in clear to an address given beside it.
pub fn give_server_address(server: &str, address: IpAddr) -> Result<(), ClientError> {
    let server = ServerUrl::parse(server)?;
    if !server.https {
        return Err(ClientError::flag(
            "--server-address",
            "is given only with an https:// --server: a plain http:// server is the machine itself",
        ));
    }
    book(&server).seed(address);
    Ok(())
}

/// Looks a name up.
pub(crate) trait Lookup: std::fmt::Debug + Send + Sync + 'static {
    /// The addresses of `host` on `port`.
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// The system's resolver (`getaddrinfo`, through the standard library). An address literal is
/// read as itself, with no lookup.
#[derive(Debug)]
pub(crate) struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        (host, port).to_socket_addrs().map(Iterator::collect)
    }
}

/// The books of one process, by server name and port.
#[derive(Debug)]
pub(crate) struct Books {
    lookup: Arc<dyn Lookup>,
    refresh: Duration,
    books: Mutex<BTreeMap<(String, u16), Arc<Book>>>,
}

impl Books {
    /// No book yet; each looks its name up with `lookup`, again after `refresh`.
    pub(crate) fn new(lookup: Arc<dyn Lookup>, refresh: Duration) -> Self {
        Self {
            lookup,
            refresh,
            books: Mutex::new(BTreeMap::new()),
        }
    }

    /// The book of `server`'s host and port, made on first use.
    pub(crate) fn book(&self, server: &ServerUrl) -> Arc<Book> {
        let mut books = self.books.lock().unwrap_or_else(PoisonError::into_inner);
        let made = books
            .entry((server.host.clone(), server.port))
            .or_insert_with(|| {
                Arc::new(Book::new(
                    &server.host,
                    server.port,
                    Arc::clone(&self.lookup),
                    self.refresh,
                ))
            });
        Arc::clone(made)
    }
}

/// What a client of the process knows of one server's address.
#[derive(Debug)]
pub(crate) struct Book {
    host: String,
    port: u16,
    lookup: Arc<dyn Lookup>,
    refresh: Duration,
    state: Mutex<State>,
    /// Told when a background lookup ends.
    settled: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// The address the last new connection completed its handshake on, or the one
    /// `--server-address` gave before any did.
    known: Option<SocketAddr>,
    /// The newest answer of a background lookup when it does not hold `known`: tried before
    /// `known` by a new connection, and dropped as each fails. Never holds `known`.
    fresh: Vec<SocketAddr>,
    /// When the last background lookup ended.
    looked: Option<Instant>,
    /// A background lookup is running.
    looking: bool,
    /// The last background lookup failed (and that was logged).
    failing: bool,
    /// The last connection made to `known` failed before its handshake completed: the next
    /// new connection looks the name up first and tries `known` after the answer.
    known_failed: bool,
}

impl Book {
    /// A book of `host` on `port` that knows no address yet.
    pub(crate) fn new(host: &str, port: u16, lookup: Arc<dyn Lookup>, refresh: Duration) -> Self {
        Self {
            host: host.to_owned(),
            port,
            lookup,
            refresh,
            state: Mutex::new(State::default()),
            settled: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes `address`, on the server's port, as the server's when no address is known yet.
    pub(crate) fn seed(&self, address: IpAddr) {
        let mut state = self.lock();
        state.known = state.known.or(Some(SocketAddr::new(address, self.port)));
    }

    /// The addresses a new connection tries, in order: the fresh answer, then the remembered
    /// address, with no lookup on the way (ureq asks for them before every request, one that
    /// reuses a kept connection included), and a background lookup started when one is due
    /// and the connector is not about to look the name up itself (see
    /// [`Book::known_failed`]). With none remembered, the answer of a lookup made now, which
    /// may take `limit`.
    pub(crate) fn addresses(self: &Arc<Self>, limit: Duration) -> Result<Vec<SocketAddr>, Error> {
        let mut state = self.lock();
        let Some(known) = state.known else {
            drop(state);
            return self.look_up_now(limit);
        };
        let mut order = state.fresh.clone();
        order.push(known);
        let due = state
            .looked
            .is_none_or(|ended| ended.elapsed() >= self.refresh);
        if due & !state.looking & !state.known_failed {
            state.looking = true;
            drop(state);
            self.look_up_behind(known);
        }
        Ok(order)
    }

    /// Looks the name up on a thread of its own, which takes the caller's log subscriber, and
    /// learns the answer; `known` is the address requests go to meanwhile.
    fn look_up_behind(self: &Arc<Self>, known: SocketAddr) {
        let book = Arc::clone(self);
        let dispatch = tracing::dispatcher::get_default(Dispatch::clone);
        thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let found = book.lookup.lookup(&book.host, book.port);
                book.learn(found, known);
            });
        });
    }

    /// Takes the answer of a background lookup.
    fn learn(&self, found: io::Result<Vec<SocketAddr>>, known: SocketAddr) {
        let mut state = self.lock();
        state.looked = Some(Instant::now());
        state.looking = false;
        match found.and_then(some) {
            Ok(addresses) => {
                if state.failing {
                    tracing::warn!("{} is looked up again: {addresses:?}", self.host);
                }
                state.failing = false;
                let kept = state.known.is_some_and(|known| addresses.contains(&known));
                state.fresh = if kept { Vec::new() } else { addresses };
            }
            Err(err) => {
                if !state.failing {
                    tracing::warn!(
                        "cannot look up {}: {err}; requests go to {known}, the address the last handshake with the server completed on, until a lookup answers",
                        self.host
                    );
                }
                state.failing = true;
            }
        }
        drop(state);
        self.settled.notify_all();
    }

    /// Looks the name up and waits for the answer for at most `limit`.
    fn look_up_now(&self, limit: Duration) -> Result<Vec<SocketAddr>, Error> {
        let (send, receive) = mpsc::sync_channel(1);
        let (lookup, host, port) = (Arc::clone(&self.lookup), self.host.clone(), self.port);
        thread::spawn(move || {
            // The receiver is gone when the wait ended first: the answer then goes unread.
            let _ = send.send(lookup.lookup(&host, port));
        });
        match receive.recv_timeout(limit) {
            Ok(found) => found.and_then(some).map_err(Error::Io),
            Err(_) => Err(Error::Timeout(Timeout::Resolve)),
        }
    }

    /// The longest a connection to `address` may take, out of the `budget` left: all of it for
    /// the remembered address and when none is remembered (a first start has nothing to fall
    /// back on), at most [`FRESH_CONNECT`] for another.
    fn limit_for(&self, address: SocketAddr, budget: Duration) -> Duration {
        let known = self.lock().known;
        let whole = (known == Some(address)) | known.is_none();
        let cap = if whole { budget } else { FRESH_CONNECT };
        budget.min(cap)
    }

    /// The last connection made to the remembered address failed: a new connection then looks
    /// the name up before it tries that address again, so an address that no longer answers
    /// (even one that drops what is sent to it, and so takes a connection's whole limit) is
    /// not the first one tried by every connection after it.
    fn known_failed(&self) -> bool {
        self.lock().known_failed
    }

    /// A new connection to `address` completed its handshake: it is the server's address now.
    /// A move to it from another drops the rest of the fresh answer, which is not needed any
    /// more; a handshake on the address already in use keeps the fresh answer for the next
    /// new connection to try.
    fn handshook(&self, address: SocketAddr) {
        let mut state = self.lock();
        state.known_failed = false;
        let before = state.known.replace(address);
        let Some(old) = before.filter(|old| *old != address) else {
            return;
        };
        state.fresh.clear();
        drop(state);
        tracing::warn!(
            "{} moved from {old} to {address}: a lookup gave it and a handshake with it completed",
            self.host
        );
    }

    /// A new connection to `address` failed before its handshake completed.
    fn failed(&self, address: SocketAddr) {
        let mut state = self.lock();
        state.fresh.retain(|fresh| *fresh != address);
        let remembered = state.known == Some(address);
        state.known_failed |= remembered;
    }

    /// The addresses to try once every one of `tried` failed: when they held the remembered
    /// address, the other addresses of a lookup made now (within `limit`); otherwise none,
    /// since `tried` was a lookup's answer already.
    fn after_failure(&self, tried: &[SocketAddr], limit: Duration) -> Vec<SocketAddr> {
        let remembered = self
            .lock()
            .known
            .is_some_and(|known| tried.contains(&known));
        if !remembered {
            return Vec::new();
        }
        self.other_answers(tried, limit)
    }

    /// The addresses a lookup made now (within `limit`) gives that are not in `tried`; none
    /// when the lookup fails.
    fn other_answers(&self, tried: &[SocketAddr], limit: Duration) -> Vec<SocketAddr> {
        let mut found = self.look_up_now(limit).unwrap_or_default();
        found.retain(|address| !tried.contains(address));
        found
    }

    /// Waits until no background lookup runs: a test's explicit sign that the one a request
    /// started has been learned.
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        let state = self.lock();
        let _settled = self
            .settled
            .wait_while(state, |state| state.looking)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// The address requests go to now, for a test.
    #[cfg(test)]
    pub(crate) fn known(&self) -> Option<SocketAddr> {
        self.lock().known
    }
}

/// An answer with no address is a failed lookup.
fn some(found: Vec<SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    if found.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the name has no address",
        ));
    }
    Ok(found)
}

/// The resolver of a client's agent: the addresses of its book. The agent is the client's
/// alone and sends to the one server the book is of, so the URI is not read.
#[derive(Debug)]
pub(crate) struct Resolve(pub(crate) Arc<Book>);

impl Resolver for Resolve {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let mut out = self.empty();
        for address in self
            .0
            .addresses(*timeout.after)?
            .into_iter()
            .take(MAX_ADDRESSES)
        {
            out.push(address);
        }
        Ok(out)
    }
}

/// The connector of a client's agent: `inner` (TCP, then TLS) tried on each address the
/// resolver gave, one at a time, and on the other addresses of a lookup made on the spot when
/// the remembered address was among them and all failed. When the last connection to the
/// remembered address failed, the lookup is made first and its other addresses tried before
/// the resolver's. A lookup made here waits at most [`FRESH_CONNECT`], and at most half of
/// what is left of the connection's limit, so the addresses after it keep time to be tried.
/// The first address a handshake completes on is the book's from then on.
#[derive(Debug)]
pub(crate) struct Remember<C> {
    /// The connector that makes one connection, handshake included.
    pub(crate) inner: C,
    /// The book of the server.
    pub(crate) book: Arc<Book>,
}

impl<C: Connector<()>> Connector<()> for Remember<C> {
    type Out = C::Out;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        let try_at = Try {
            details,
            started: Instant::now(),
            budget: *details.timeout.after,
        };
        let tried = &details.addrs;
        if self.book.known_failed() {
            let more = self.book.other_answers(tried, try_at.lookup_limit());
            return self
                .first(&try_at, &more, Error::HostNotFound)
                .or_else(|last| self.first(&try_at, tried, last))
                .map(Some);
        }
        self.first(&try_at, tried, Error::HostNotFound)
            .or_else(|last| {
                let more = self.book.after_failure(tried, try_at.lookup_limit());
                self.first(&try_at, &more, last)
            })
            .map(Some)
    }
}

/// One new connection being made: what ureq asked for, when the connector started, and the
/// limit ureq gave it, counted from then (all the attempts share it).
struct Try<'a, 'b> {
    details: &'a ConnectionDetails<'b>,
    started: Instant,
    budget: Duration,
}

impl Try<'_, '_> {
    /// What is left of the limit.
    fn left(&self) -> Duration {
        self.budget.saturating_sub(self.started.elapsed())
    }

    /// The longest a lookup made by the connector may be waited for.
    fn lookup_limit(&self) -> Duration {
        FRESH_CONNECT.min(self.left() / 2)
    }
}

impl<C: Connector<()>> Remember<C> {
    /// The first connection of `addresses` that completes its handshake, or the last failure
    /// (`last` when there is none to try).
    fn first(
        &self,
        try_at: &Try<'_, '_>,
        addresses: &[SocketAddr],
        mut last: Error,
    ) -> Result<C::Out, Error> {
        for &address in addresses {
            match self.attempt(try_at, address) {
                Ok(made) => return Ok(made),
                Err(err) => last = err,
            }
        }
        Err(last)
    }

    /// One connection to `address`, handshake included, told to the book.
    fn attempt(&self, try_at: &Try<'_, '_>, address: SocketAddr) -> Result<C::Out, Error> {
        let details = try_at.details;
        let mut addrs = details.resolver.empty();
        addrs.push(address);
        // ureq counts the limit from `details.now`, the start of the whole connection, so a
        // capped attempt's deadline is the time already spent and its own limit. A deadline at
        // or past the whole limit keeps ureq's own form of it (one that never ends, with no
        // global limit, cannot be written as a duration: it would overflow the deadline).
        // The clock is read once: what is spent and what is left add up to the whole limit.
        let spent = try_at.started.elapsed();
        let left = try_at.budget.saturating_sub(spent);
        let limit = self.book.limit_for(address, left);
        let after = if limit < left {
            Budget::Exact(spent.saturating_add(limit))
        } else {
            details.timeout.after
        };
        let one = ConnectionDetails {
            uri: details.uri,
            addrs,
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: NextTimeout {
                after,
                ..details.timeout
            },
            current_time: Arc::clone(&details.current_time),
            run_connector: Arc::clone(&details.run_connector),
            register_shutdown: Arc::clone(&details.register_shutdown),
        };
        let made = self
            .inner
            .connect(&one, None)
            .and_then(|made| made.ok_or(Error::ConnectionFailed));
        match &made {
            Ok(_) => self.book.handshook(address),
            Err(_) => self.book.failed(address),
        }
        made
    }
}

#[cfg(test)]
#[path = "address_tests.rs"]
mod tests;
