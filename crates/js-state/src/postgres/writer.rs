//! The Postgres side of [`crate::WriterLock`]: a session-level advisory lock held on a
//! connection of its own, outside the store's pool.
//!
//! A session-level lock lives as long as the session that took it, so it is held on a
//! dedicated connection that nothing else uses: a pooled one would be handed to other
//! work, and a transaction-level lock would end with the first commit. The session ending
//! for any reason (the process exits, the connection breaks, an administrator terminates
//! the backend) releases the lock on the server, which is what lets another process take
//! it after a crash; [`LockSession::check`] is how the holder learns its session is gone.
//!
//! Once the lock is held, [`LockSession::take`] increments the store's writer generation, in
//! its own transaction on the same connection: the update waits for every commit that holds
//! the row shared (each commit of a fenced store does), so the old holder's last commit is
//! ordered before the new holder starts, and none after it can succeed.
//!
//! [`probe`] says who holds the lock without trying it: it reads `pg_locks`, on a
//! connection of its own that takes no lock.

use postgres::error::SqlState;

use super::conn::{Conn, DialError};
use super::{PostgresConfig, describe, failed};
use crate::writer::LockRefused;
use crate::{StoreError, WriterGeneration};

/// The key of the writer lock: the advisory lock (single `bigint` form) the active writer
/// of a store holds for its life (design section 10.2, "one writer"). Its bytes spell
/// `jswriter` in ASCII. It lies outside the 32-bit range, so it can never equal the key
/// of the schema-migration lock, `hashtext('kbf-state-schema')`, which is a 32-bit hash
/// widened to `bigint`, nor any other key taken through `hashtext`.
///
/// Advisory locks are scoped to a database: every store in one database (whatever its
/// schema) shares this lock, and stores in different databases do not. The value never
/// changes: two releases with different keys would not exclude each other.
pub const WRITER_LOCK_KEY: i64 = 0x6a73_7772_6974_6572;

/// Takes the lock without waiting: `true` when this session now holds it.
const TRY_LOCK: &str = "SELECT pg_try_advisory_lock($1)";

/// Gives the lock back while the session is still open, so that the release is over on
/// the server when the drop returns (closing alone lets the backend release it later).
const UNLOCK: &str = "SELECT pg_advisory_unlock($1)";

/// The backend that holds the writer lock in the connection's database, and when that
/// backend started, in microseconds since the Unix epoch. A single-`bigint` advisory key
/// is split into `classid` (its high 32 bits) and `objid` (its low 32 bits), with
/// `objsubid` 1. `backend_start` is NULL when the probing role may not see the holder's
/// session (another role, without `pg_read_all_stats`).
const HOLDER: &str = "SELECT l.pid, (extract(epoch FROM a.backend_start) * 1000000)::bigint \
                      FROM pg_locks l LEFT JOIN pg_stat_activity a ON a.pid = l.pid \
                      WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 \
                      AND l.database = \
                      (SELECT oid FROM pg_database WHERE datname = current_database()) \
                      AND ((l.classid::bigint << 32) | l.objid::bigint) = $1";

/// Why an acquire on a schema older than version 2 is refused.
const NO_TABLE: &str = "the state schema has no writer generation table (it is older than \
                        version 2): run the installer to bring it forward";

/// The connection that holds the writer lock, and the generation it took.
pub(crate) struct LockSession {
    conn: Conn,
    generation: WriterGeneration,
}

impl LockSession {
    /// Opens a connection of its own to the database `config` names, takes the writer lock
    /// on it, once, and increments the writer generation of `config`'s schema.
    ///
    /// # Errors
    /// [`LockRefused::Held`] when another session holds the lock; [`LockRefused::Failed`]
    /// when the server cannot be reached or refuses the login or a call, or the schema has no
    /// writer generation row (a schema older than version 2: the installer brings it
    /// forward). The lock is given back on every error.
    pub(crate) fn take(config: &PostgresConfig) -> Result<LockSession, LockRefused> {
        let mut conn =
            Conn::connect(&config.dial()).map_err(|err| LockRefused::Failed(dial_failed(err)))?;
        let taken: bool = conn
            .query_one(TRY_LOCK, &[&WRITER_LOCK_KEY])
            .map_err(refused)?
            .get(0);
        if !taken {
            return Err(LockRefused::Held);
        }
        // Held from here, so the drop gives the lock back if the increment fails.
        let mut session = LockSession {
            conn,
            generation: WriterGeneration::new(0),
        };
        let bumped: Option<i64> = session
            .conn
            .query_opt(
                &format!(
                    "UPDATE {}.writer_generation SET generation = generation + 1 \
                     RETURNING generation",
                    config.schema
                ),
                &[],
            )
            .map_err(|err| {
                if err.code() == Some(&SqlState::UNDEFINED_TABLE) {
                    LockRefused::Failed(NO_TABLE.to_owned())
                } else {
                    refused(err)
                }
            })?
            .map(|row| row.get(0));
        session.generation = WriterGeneration::new(bumped.ok_or(LockRefused::Failed(
            "the state schema has no writer generation row".to_owned(),
        ))?);
        Ok(session)
    }

    /// The generation this holder took.
    pub(crate) fn generation(&self) -> WriterGeneration {
        self.generation
    }

    /// A trivial query on the lock's own session: an error when the session is gone, and
    /// with it the lock.
    ///
    /// # Errors
    /// [`StoreError`] when the query fails.
    pub(crate) fn check(&mut self) -> Result<(), StoreError> {
        self.conn
            .query_one("SELECT 1", &[])
            .map(drop)
            .map_err(failed)
    }
}

impl Drop for LockSession {
    fn drop(&mut self) {
        // A session that is gone has released the lock already; the close that follows
        // ends it either way.
        let _ = self.conn.query_one(UNLOCK, &[&WRITER_LOCK_KEY]);
    }
}

/// The holder of the writer lock of the database `config` names, read from `pg_locks`
/// without trying the lock: `None` when no session holds it, else `<pid>@<backend start>`
/// (the start in microseconds since the Unix epoch), or the pid alone when this role may
/// not see when the holder's backend started.
///
/// # Errors
/// The text of [`dial_failed`] or [`refusal`] when the server cannot be reached or refuses
/// the login or the query.
pub(crate) fn probe(config: &PostgresConfig) -> Result<Option<String>, String> {
    let mut conn = Conn::connect(&config.dial()).map_err(dial_failed)?;
    let row = conn
        .query_opt(HOLDER, &[&WRITER_LOCK_KEY])
        .map_err(refusal)?;
    Ok(row.map(|row| {
        let pid: i32 = row.get(0);
        let started: Option<i64> = row.get(1);
        match started {
            Some(started) => format!("{pid}@{started}"),
            None => pid.to_string(),
        }
    }))
}

/// A connection that could not be opened, in words that carry neither the URL nor the
/// password.
fn dial_failed(err: DialError) -> String {
    match err {
        DialError::Pg(err) => refusal(err),
        DialError::Tls(err) => err.to_string(),
    }
}

/// A failed call while taking the lock, as [`LockRefused::Failed`].
fn refused(err: postgres::Error) -> LockRefused {
    LockRefused::Failed(refusal(err))
}

/// A failed call: the server's answer by its SQLSTATE alone (the server's text can quote
/// the role or the database), anything else as the client says it. Neither carries the
/// URL or the password.
fn refusal(err: postgres::Error) -> String {
    match err.code() {
        Some(code) => format!("the server refused (SQLSTATE {})", code.code()),
        None => describe(&err),
    }
}
