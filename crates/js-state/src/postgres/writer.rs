//! The Postgres side of [`crate::WriterLock`]: a session-level advisory lock held on a
//! connection of its own, outside the store's pool.
//!
//! A session-level lock lives as long as the session that took it, so it is held on a
//! dedicated connection that nothing else uses: a pooled one would be handed to other
//! work, and a transaction-level lock would end with the first commit. The session ending
//! for any reason (the process exits, the connection breaks, an administrator terminates
//! the backend) releases the lock on the server, which is what lets another process take
//! it after a crash; [`LockSession::check`] is how the holder learns its session is gone.

use super::conn::Conn;
use super::{PostgresConfig, describe, failed};
use crate::StoreError;
use crate::writer::LockRefused;

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

/// The connection that holds the writer lock.
pub(crate) struct LockSession {
    conn: Conn,
}

impl LockSession {
    /// Opens a connection of its own to the database `config` names and takes the writer
    /// lock on it, once.
    ///
    /// # Errors
    /// [`LockRefused::Held`] when another session holds the lock; [`LockRefused::Failed`]
    /// when the server cannot be reached or refuses the login or the call.
    pub(crate) fn take(config: &PostgresConfig) -> Result<LockSession, LockRefused> {
        let mut conn = Conn::connect(&config.pg).map_err(refused)?;
        let taken: bool = conn
            .query_one(TRY_LOCK, &[&WRITER_LOCK_KEY])
            .map_err(refused)?
            .get(0);
        if taken {
            Ok(LockSession { conn })
        } else {
            Err(LockRefused::Held)
        }
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

/// A failed call while taking the lock, as [`LockRefused::Failed`]: the server's answer by
/// its SQLSTATE alone (the server's text can quote the role or the database), anything
/// else as the client says it. Neither carries the URL or the password.
fn refused(err: postgres::Error) -> LockRefused {
    LockRefused::Failed(match err.code() {
        Some(code) => format!("the server refused (SQLSTATE {})", code.code()),
        None => describe(&err),
    })
}
