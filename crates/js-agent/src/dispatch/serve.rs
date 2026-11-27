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

/// Polls for the sessions `cell` is given until it is closed, the answers handed to `worker`,
/// and a wait after a failure (any answer but a usable 200, any transport error) as `jitter`
/// says. The polls share one transport, as the session's heartbeats do, so they share its
/// kept connection and what its client knows of the server's address; a new one is made
/// ([`Dial::open`]) only after a poll for which none could be made. Messages the worker had no room
/// for are held and handed over when it makes room, before the next poll is posted; meanwhile
/// nothing is posted. The poller's lines go to the cell.
pub fn serve(cell: &Cell, dial: &Dial<'_>, worker: &Worker, jitter: Box<dyn Jitter>) {
    let mut poller = Poller::new(worker, jitter);
    let mut kept = None;
    while let Some((generation, route)) = cell.wait_route() {
        let next = poller
            .hand_over()
            .unwrap_or_else(|| match transport_for(&mut kept, dial) {
                Ok(transport) => poller.poll(&mut **transport, &route),
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

/// The transport for a poll: the kept one, otherwise a new one, which is kept from then on.
/// Every route of the session authenticates with the host's one certificate, so a kept
/// transport serves every later route.
///
/// # Errors
///
/// No transport could be made (see [`Dial::open`]); none is kept then.
fn transport_for<'k>(
    kept: &'k mut Option<Box<dyn Transport>>,
    dial: &Dial<'_>,
) -> Result<&'k mut Box<dyn Transport>, String> {
    let made = match kept.take() {
        Some(transport) => transport,
        None => dial.open()?,
    };
    Ok(kept.insert(made))
}
