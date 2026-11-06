//! The writer lock on Postgres: a session-level advisory lock on a connection of its own,
//! taken once (no waiting), released when the holder is dropped or its session ends.
//!
//! Each test has a database of its own (`test_database`), and advisory locks are scoped
//! to a database, so the tests do not see each other's locks. The tests need the server
//! `KBF_TEST_POSTGRES_URL` names (a superuser, to terminate a backend; Postgres 14 or later,
//! for `pg_terminate_backend` with a timeout) and fail, not skip, without one.
#![cfg(feature = "postgres")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use js_state::postgres::{PostgresConfig, TestDatabase, WRITER_LOCK_KEY, test_database};
use js_state::sqlite::{Options, SqliteError, SqliteStore};
use js_state::{Clock, LockRefused, ManualClock, WriterLock, WriterTarget};
use js_testkit::dir::TestDir;
use postgres::{Client, NoTls};

fn target(database: &TestDatabase) -> WriterTarget {
    WriterTarget::Postgres(Box::new(
        PostgresConfig::parse(&database.url()).expect("the URL parses"),
    ))
}

/// The backend that holds the writer lock in the database `admin` is connected to, read
/// from `pg_locks` (a single-`bigint` advisory key is split into `classid`, its high 32
/// bits, and `objid`, its low 32 bits, with `objsubid` 1).
fn holder(admin: &mut Client) -> Option<i32> {
    admin
        .query_opt(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND objsubid = 1 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
             AND ((classid::bigint << 32) | objid::bigint) = $1",
            &[&WRITER_LOCK_KEY],
        )
        .expect("the lock view")
        .map(|row| row.get(0))
}

// Catches a lock whose connection leaks past the drop (the server keeps the session, and
// with it the lock, so the next start is refused as held), and a release left to the
// backend's exit after the close, which a new acquire can overtake.
#[test]
fn the_writer_lock_is_released_when_the_holder_ends() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    let first = WriterLock::acquire(&target(&database)).expect("the first acquire");
    assert!(
        holder(&mut admin).is_some(),
        "the lock is held on the server"
    );
    drop(first);
    assert_eq!(holder(&mut admin), None, "the drop released the lock");
    let again = WriterLock::acquire(&target(&database)).expect("acquired again after the drop");
    drop(again);
}

// Catches a lock that is not taken (a second process would become a second writer), a
// lock taken on a pooled or transaction scope that is gone before the second acquire,
// and a key that is not scoped to the database (a store in another database refused).
#[test]
fn a_second_acquire_on_the_same_store_is_refused_as_held() {
    let database = test_database();
    let held = WriterLock::acquire(&target(&database)).expect("the first acquire");
    let second = WriterLock::acquire(&target(&database));
    assert_eq!(second.err(), Some(LockRefused::Held));
    let other = test_database();
    let elsewhere = WriterLock::acquire(&target(&other)).expect("another database's own lock");
    drop(elsewhere);
    drop(held);
}

// Catches a writer lock on the migration lock's key: an installer applying a migration
// would then wait on (or be taken for) the running writer.
#[test]
fn the_writer_lock_key_differs_from_the_migration_lock_key() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    let migration: i64 = admin
        .query_one("SELECT hashtext('kbf-state-schema')::bigint", &[])
        .expect("the migration key")
        .get(0);
    assert_ne!(WRITER_LOCK_KEY, migration);
    assert!(
        i32::try_from(WRITER_LOCK_KEY).is_err(),
        "outside the range of any hashtext key"
    );
}

// Catches a check that does not touch the lock's own session (a pooled connection, or no
// query at all): the holder would keep writing after losing the lock to another process.
#[test]
fn check_fails_after_the_holder_backend_is_terminated() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    let mut lock = WriterLock::acquire(&target(&database)).expect("acquired");
    assert_eq!(lock.check(), Ok(()));
    let pid = holder(&mut admin).expect("the holder's backend");
    // The second argument waits (up to a minute) for the backend to have exited, so the
    // lock is gone on the server when this returns.
    let ended: bool = admin
        .query_one("SELECT pg_terminate_backend($1, 60000)", &[&pid])
        .expect("terminates")
        .get(0);
    assert!(ended, "the holder's backend ended");
    assert!(lock.check().is_err(), "check saw the session gone");
    let next = WriterLock::acquire(&target(&database)).expect("the lock is free again");
    drop(next);
    drop(lock);
}

/// A server that asks for a password, reads it, and refuses the login as Postgres does
/// for a wrong one (SQLSTATE 28P01, with a message that quotes the role). Returns the port
/// and the thread, which ends once the refusal is sent.
fn refusing_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0; 1024];
        let _ = stream.read(&mut buf).expect("the startup message");
        // AuthenticationCleartextPassword: 'R', length 8, code 3.
        stream
            .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 3])
            .expect("ask for the password");
        let _ = stream.read(&mut buf).expect("the password message");
        let mut fields = Vec::new();
        for (tag, value) in [
            (b'S', "FATAL"),
            (b'V', "FATAL"),
            (b'C', "28P01"),
            (b'M', "password authentication failed for user \"writer\""),
        ] {
            fields.push(tag);
            fields.extend_from_slice(value.as_bytes());
            fields.push(0);
        }
        fields.push(0);
        let length = u32::try_from(fields.len() + 4).expect("a short message");
        let mut message = vec![b'E'];
        message.extend_from_slice(&length.to_be_bytes());
        message.extend_from_slice(&fields);
        stream.write_all(&message).expect("refuse the login");
    });
    (port, server)
}

// Catches a refusal that quotes the URL, the password or the server's own text (which
// names the role): the error is logged at start, and the URL is read from a secret file.
#[test]
fn lock_errors_never_contain_the_url_or_password() {
    let password = "wrong-password-7f3a";
    let (port, server) = refusing_server();
    let url = format!("postgres://writer:{password}@127.0.0.1:{port}/state");
    let config = PostgresConfig::parse(&url).expect("the URL parses");
    let refused = WriterLock::acquire(&WriterTarget::Postgres(Box::new(config)))
        .expect_err("the login is refused");
    server.join().expect("the server thread");
    assert_eq!(
        refused.to_string(),
        "the store's writer lock could not be taken: the server refused (SQLSTATE 28P01)"
    );
    let debug = format!("{refused:?}");
    assert!(!debug.contains(password), "{debug}");
    assert!(!debug.contains(&url), "{debug}");

    // No server at all: the client's own words, with no address in them.
    let dir = TestDir::under(std::env::temp_dir(), "js-state-writer-lock-no-server");
    let socket = dir.path().join("absent");
    let url = format!(
        "postgres:///state?host={}&user=writer&password={password}",
        socket.display()
    );
    let config = PostgresConfig::parse(&url).expect("the URL parses");
    let refused = WriterLock::acquire(&WriterTarget::Postgres(Box::new(config)))
        .expect_err("nothing answers");
    assert_eq!(
        refused.to_string(),
        "the store's writer lock could not be taken: \
         error connecting to server: No such file or directory (os error 2)"
    );
}

// Catches an `open_locked` that takes a Postgres writer lock as the state file's (the file
// would run unlocked), and a refused lock kept held. Also shows the lock's debug output
// names the backend and no part of the URL.
#[test]
fn open_locked_refuses_a_postgres_writer_lock() {
    let database = test_database();
    let lock = WriterLock::acquire(&target(&database)).expect("acquired");
    assert_eq!(
        format!("{lock:?}"),
        "WriterLock { backend: \"postgres\", .. }"
    );
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-state-writer-lock-pg-file");
    let db = dir.path().join("state.db");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
    let refused = SqliteStore::open_locked(&db, lock, clock, Options::default())
        .expect_err("a Postgres lock is not the file's");
    assert!(
        matches!(refused, SqliteError::NotThisFilesLock { .. }),
        "{refused:?}"
    );
    let again = WriterLock::acquire(&target(&database)).expect("the refused lock was released");
    drop(again);
}
