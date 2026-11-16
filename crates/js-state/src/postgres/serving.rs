//! Whether a server process is connected to the database, for the update hook: there is no
//! lock to ask about, so the answer is read from the sessions themselves.
//!
//! A server process keeps at least one session open to the database under the application
//! name its configuration gives it (`PostgresConfig::with_application_name`) for as long as it
//! runs, and none once it has exited, so the oldest such session says who is serving and
//! since when. The probe reads `pg_stat_activity` on a connection of its own, takes no lock
//! and writes nothing. It is for an install that runs one instance, which is how a unit is
//! stopped and started by the hook; with several instances on the database the oldest of them
//! is the one reported, and nothing is decided from it.

use super::conn::{Conn, DialError};
use super::{PostgresConfig, describe};

/// The oldest session of the connection's database under the application name `$1`, other
/// than the probe's own, and when its backend started, in microseconds since the Unix
/// epoch. `backend_start` is NULL when the probing role may not see the session's details
/// (another role, without `pg_read_all_stats`); such a session does not show its
/// application name either, so it is not found.
const OLDEST: &str = "SELECT pid, (extract(epoch FROM backend_start) * 1000000)::bigint \
                      FROM pg_stat_activity \
                      WHERE datname = current_database() AND application_name = $1 \
                      AND pid <> pg_backend_pid() \
                      ORDER BY backend_start, pid LIMIT 1";

/// The oldest session of the database `config` names that is under `config`'s application
/// name, read without taking a lock: `None` when no process is connected under that name, else
/// `<pid>@<backend start>` (the start in microseconds since the Unix epoch).
///
/// # Errors
/// Text that carries neither the URL nor the password, when the server cannot be reached or
/// refuses the login or the query.
pub fn probe(config: &PostgresConfig) -> Result<Option<String>, String> {
    let application = config.pg.get_application_name().unwrap_or_default();
    let mut conn = Conn::connect(&config.dial()).map_err(dial_failed)?;
    let row = conn.query_opt(OLDEST, &[&application]).map_err(refusal)?;
    Ok(row.map(|row| {
        let pid: i32 = row.get(0);
        let started: Option<i64> = row.get(1);
        format!("{pid}@{}", started.unwrap_or_default())
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

/// A failed call: the server's answer by its SQLSTATE alone (the server's text can quote the
/// role or the database), anything else as the client says it.
fn refusal(err: postgres::Error) -> String {
    match err.code() {
        Some(code) => format!("the server refused (SQLSTATE {})", code.code()),
        None => describe(&err),
    }
}
