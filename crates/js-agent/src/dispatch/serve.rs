//! The poller's thread: wait for a route, poll, pause as the answer says.

use std::path::Path;

use super::{Cell, Next, Poller, ROOM_WAIT};
use crate::backoff::Jitter;
use crate::login::Login;
use crate::transport::{Connector, Transport};
use crate::worker::Worker;

/// How the poller makes the transport of a poll: as the session's was made, so it reaches
/// the server the same way and authenticates as the same host.
pub struct Dial<'a> {
    /// Makes the transport. It is shared with the thread of the poller, so it is `Sync`.
    pub connector: &'a (dyn Connector + Sync),
    /// The `--server` the session's transport was made for.
    pub server: &'a str,
    /// The login the session's transport was made with.
    pub login: &'a Login,
    /// The `--ca-cert`, when one was given.
    pub ca_cert: Option<&'a Path>,
}

impl Dial<'_> {
    /// A transport that authenticates as the session's does.
    ///
    /// # Errors
    ///
    /// The connector refused the flags; the text never holds a secret.
    pub fn open(&self) -> Result<Box<dyn Transport>, String> {
        self.connector
            .connect(self.server, self.login, self.ca_cert)
            .map_err(|why| why.to_string())
    }
}

/// Polls for the sessions `cell` is given until it is closed: each poll on a transport of
/// its own ([`Dial::open`]), the answers handed to `worker`, and a wait after a failure
/// (any answer but a usable 200, any transport error) as `jitter` says. Messages the worker
/// had no room for are held and handed over when it makes room, before the next poll is
/// posted; meanwhile no transport is made. The poller's lines go to the cell.
pub fn serve(cell: &Cell, dial: &Dial<'_>, worker: &Worker, jitter: Box<dyn Jitter>) {
    let mut poller = Poller::new(worker, jitter);
    while let Some((generation, route)) = cell.wait_route() {
        let next = poller.hand_over().unwrap_or_else(|| match dial.open() {
            Ok(mut transport) => poller.poll(&mut *transport, &route),
            Err(_) => poller.failed(None),
        });
        cell.note(poller.take_lines());
        match next {
            Next::Again => {}
            Next::After(wait) => {
                cell.pause(wait, generation);
            }
            Next::Room(seen) => {
                worker.wait_room(seen, ROOM_WAIT);
            }
        }
    }
}
