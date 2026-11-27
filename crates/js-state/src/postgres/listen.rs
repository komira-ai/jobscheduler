//! Notices between the processes of one database (`docs/design/multi-instance.md`, section 0,
//! G16): [`Store::notify`](crate::Store::notify) sends a write's notices with `pg_notify` after
//! the write's commit, and a [`Listener`] hears them on a connection of its own.
//!
//! A notice is best effort, and the rows decide: a process that waits for a write still polls
//! for it, and a notice only ends that wait early. So a notice that cannot be sent is logged and
//! dropped, and nothing waits for one. Postgres delivers a notice to every connection that
//! listens on its channel when it is sent, in the order sent, and to none that listens later: a
//! listener whose connection was gone may have missed notices, so it says so when its connection
//! is made again ([`Heard::Listening`]), and its caller wakes every waiter once.
//!
//! The listening connection is made as the store's own are (the configuration's TLS mode and
//! network limits, so a peer that vanishes is found by its keepalives), without the server limits
//! a session of the store sets: it holds no transaction and runs one statement, its `LISTEN`.
//! It is not one of the pool's connections, so it takes no slot of the store's.

use std::time::Duration;

use super::conn::{Conn, Dial};
use super::{Fail, PostgresConfig, PostgresStore};
use crate::{Notice, StoreError};

/// What is logged of notices that were not sent.
const UNSENT: &str =
    "the notices of a commit were not sent: their listeners read the rows at their next poll";

/// The statement that sends notices: one `pg_notify` for each channel and payload, in order.
const SEND: &str = "SELECT pg_notify(c, p) FROM unnest($1::text[], $2::text[]) AS n(c, p)";

impl PostgresStore {
    /// Sends `notices` on a session of the pool, in one statement; a failure is logged.
    pub(super) fn send(&self, notices: &[Notice]) {
        let channels: Vec<&str> = notices.iter().map(|n| n.channel.as_str()).collect();
        let payloads: Vec<&str> = notices.iter().map(|n| n.payload.as_str()).collect();
        let sent = self.read(|s| s.client.execute(SEND, &[&channels, &payloads]));
        if let Err(error) = sent {
            let unsent = notices.len();
            tracing::warn!(%error, unsent, "{UNSENT}");
        }
    }
}

/// What [`Listener::next`] heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The connection was made, the first time or again, and listens on every channel: a notice
    /// sent while it was not made was missed, so whoever waits on one is to look again.
    Listening,
    /// A notice of a channel listened on.
    Notice(Notice),
    /// The wait passed and nothing came.
    Quiet,
    /// The connection could not be made, or was lost while it waited: the next call makes it
    /// again. The error says what the client met.
    Lost(StoreError),
}

/// A connection of its own that listens on a fixed set of channels: each call of
/// [`Listener::next`] answers the next thing it hears. Calls block; a caller on an async
/// runtime keeps the listener on a thread of its own.
pub struct Listener {
    /// How the connection is made.
    dial: Dial,
    /// The `LISTEN` of every channel, run on each connection made.
    listen: String,
    /// The connection, while it is made.
    conn: Option<Conn>,
}

impl Listener {
    /// A listener on `channels` of the database `config` names. Nothing is connected until the
    /// first [`Listener::next`].
    #[must_use]
    pub fn new(config: &PostgresConfig, channels: &[&str]) -> Listener {
        let listen = channels
            .iter()
            .map(|channel| format!("LISTEN \"{}\";", channel.replace('"', "\"\"")))
            .collect();
        Listener {
            dial: config.dial_ddl(),
            listen,
            conn: None,
        }
    }

    /// The next thing heard, waiting at most `wait` for a notice. With no connection, it is made
    /// and listens ([`Heard::Listening`], waiting for nothing), or could not be
    /// ([`Heard::Lost`]). With one, the next notice of a channel listened on, a notice already
    /// received first ([`Heard::Notice`]); nothing within `wait` ([`Heard::Quiet`]), after the
    /// connection answered its `LISTEN` again (so a connection that closed while it waited, which
    /// a wait alone does not tell apart from a quiet one, is found); or the connection was lost
    /// ([`Heard::Lost`]), and the next call makes it again.
    pub fn next(&mut self, wait: Duration) -> Heard {
        let Some(conn) = self.conn.as_mut() else {
            return self.connect();
        };
        match heard(conn, wait, &self.listen) {
            Ok(heard) => heard,
            Err(error) => {
                self.conn = None;
                Heard::Lost(Fail::Pg(error).into_store())
            }
        }
    }

    /// Makes the connection and listens on it.
    fn connect(&mut self) -> Heard {
        let made = Conn::connect(&self.dial)
            .map_err(Fail::from)
            .and_then(|mut conn| {
                conn.batch_execute(&self.listen)
                    .map(|()| conn)
                    .map_err(Fail::Pg)
            });
        match made {
            Ok(conn) => {
                self.conn = Some(conn);
                Heard::Listening
            }
            Err(fail) => Heard::Lost(fail.into_store()),
        }
    }
}

/// What `conn` hears within `wait`: a notice, or, when none came, quiet once the connection has
/// answered `listen` (a connection gone answers with an error).
fn heard(conn: &mut Conn, wait: Duration, listen: &str) -> Result<Heard, postgres::Error> {
    match conn.notice(wait)? {
        Some(notice) => Ok(Heard::Notice(notice)),
        None => conn.batch_execute(listen).map(|()| Heard::Quiet),
    }
}
