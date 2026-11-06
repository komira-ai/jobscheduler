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

use js_state::postgres::{
    PostgresConfig, PostgresStore, TestDatabase, WRITER_LOCK_KEY, test_database,
};
use js_state::sqlite::{Options, SqliteError, SqliteStore};
use js_state::{
    Clock, Cond, LockRefused, ManualClock, Store, StoreError, Txn, WriterGeneration, WriterLock,
    WriterTarget,
};
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

fn config(database: &TestDatabase) -> PostgresConfig {
    PostgresConfig::parse(&database.url()).expect("the URL parses")
}

/// Ends the holder's lock session, as a lost network or a stalled process does: the server
/// releases the lock with it, and the old holder does not know until it asks.
fn lose_the_lock(admin: &mut Client) {
    let pid = holder(admin).expect("the holder's backend");
    let ended: bool = admin
        .query_one("SELECT pg_terminate_backend($1, 60000)", &[&pid])
        .expect("terminates")
        .get(0);
    assert!(ended, "the holder's backend ended");
}

fn put(key: &str) -> Txn {
    Txn::new().put(key, "v", Cond::Any)
}

// Catches a writer that lost the lock and still commits (it would double-place work the new
// holder places too): once a second process takes the lock, every commit path of the first
// writer's store (a batch, a claim, a reap, a held commit) returns `Fenced` and none of its
// rows is visible, while the new holder's store commits. The generation goes up by one for
// each acquire. Before this change the first writer's commits succeeded.
#[test]
fn a_new_holder_fences_the_old_holders_commits() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    let first = WriterLock::acquire(&target(&database)).expect("the first acquire");
    let first_generation = first
        .generation()
        .expect("a Postgres lock has a generation");
    let old = PostgresStore::open(&config(&database))
        .expect("opens")
        .fenced(first_generation);
    old.txn(put("before")).expect("commits").expect("applies");
    old.txn(Txn::new().put("queue/a", "v", Cond::Any))
        .expect("commits")
        .expect("applies");
    let held = old.begin(put("held")).expect("runs to the commit");

    lose_the_lock(&mut admin);
    let second = WriterLock::acquire(&target(&database)).expect("the lock is free");
    let second_generation = second.generation().expect("a generation");
    assert_eq!(
        second_generation,
        WriterGeneration::new(first_generation.get() + 1)
    );

    // The held commit first: it holds the commit head, which the others would queue on. Its
    // snapshot is older than the new generation, so its read of the row is a serialization
    // failure (a held commit is not run again); it commits nothing either way.
    let refused = held.commit();
    assert!(
        matches!(&refused, Err(StoreError::Failed(why)) if why.contains("40001")),
        "{refused:?}"
    );
    assert_eq!(old.txn(put("late")), Err(StoreError::Fenced));
    assert_eq!(
        old.apply_batch(vec![put("late1"), put("late2")]),
        Err(StoreError::Fenced)
    );
    let claimed = old.claim("queue/", 10, |key, was| {
        Some(Txn::new().delete(key, Cond::Version(was.version)))
    });
    assert_eq!(claimed, Err(StoreError::Fenced));
    assert_eq!(old.reap_expired(), Err(StoreError::Fenced));

    let new = PostgresStore::open(&config(&database))
        .expect("opens")
        .fenced(second_generation);
    for key in ["late", "late1", "late2", "held"] {
        assert_eq!(new.get(key), Ok(None), "{key} was not written");
    }
    assert!(new.get("queue/a").expect("reads").is_some(), "not claimed");
    assert!(new.get("before").expect("reads").is_some());
    new.txn(put("current")).expect("commits").expect("applies");
    assert!(new.get("current").expect("reads").is_some());
    drop(second);
    drop(first);
}

// Catches a fence that refuses the current holder (the holder could never write) or one
// that a store opened without a generation is subject to (a store of shared writers, which
// takes no lock, would be refused): the holder commits before and after another acquire of
// the lock fails as held, and an unfenced store commits whatever the row says.
#[test]
fn the_current_holder_and_an_unfenced_store_commit() {
    let database = test_database();
    let lock = WriterLock::acquire(&target(&database)).expect("acquired");
    let store = PostgresStore::open(&config(&database))
        .expect("opens")
        .fenced(lock.generation().expect("a generation"));
    store.txn(put("a")).expect("commits").expect("applies");
    assert_eq!(
        WriterLock::acquire(&target(&database)).err(),
        Some(LockRefused::Held)
    );
    store.txn(put("b")).expect("commits").expect("applies");
    let unfenced = PostgresStore::open(&config(&database)).expect("opens");
    unfenced.txn(put("c")).expect("commits").expect("applies");
}

// Catches a reopened store that forgets its generation (a restarted frontend of the old
// holder would commit after it lost the lock).
#[test]
fn a_reopened_fenced_store_stays_fenced() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    let first = WriterLock::acquire(&target(&database)).expect("acquired");
    let old = PostgresStore::open(&config(&database))
        .expect("opens")
        .fenced(first.generation().expect("a generation"));
    lose_the_lock(&mut admin);
    let _second = WriterLock::acquire(&target(&database)).expect("the lock is free");
    let reopened = old.reopen().expect("opens again");
    assert_eq!(reopened.txn(put("late")), Err(StoreError::Fenced));
    drop(first);
}

// Catches an acquire that keeps the lock without a generation (its store could not be
// fenced) or keeps it when the increment fails (no one could take it until the process
// ends): with the row gone the acquire is refused, says why, and leaves the lock free. Also
// catches a fenced store that commits when it cannot read its generation: with the row gone
// it is fenced, and with the table gone its commit fails and writes nothing.
#[test]
fn an_acquire_on_a_schema_without_a_generation_row_is_refused_and_frees_the_lock() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    admin
        .batch_execute("DELETE FROM kbf_state.writer_generation")
        .expect("removes the row");
    assert_eq!(
        WriterLock::acquire(&target(&database)).err(),
        Some(LockRefused::Failed(
            "the state schema has no writer generation row".to_owned()
        ))
    );
    assert_eq!(holder(&mut admin), None, "the lock was given back");
    let fenced = PostgresStore::open(&config(&database))
        .expect("opens")
        .fenced(WriterGeneration::new(1));
    assert_eq!(
        fenced.txn(put("k")),
        Err(StoreError::Fenced),
        "no row is no generation"
    );
    admin
        .batch_execute("DROP TABLE kbf_state.writer_generation")
        .expect("drops the table");
    assert_eq!(
        fenced.txn(put("k")),
        Err(StoreError::Failed(
            "ERROR: relation \"writer_generation\" does not exist (SQLSTATE 42P01)".to_owned()
        )),
        "a fence that cannot be read commits nothing"
    );
    assert_eq!(fenced.get("k"), Ok(None));
    assert_eq!(
        WriterLock::acquire(&target(&database)).err(),
        Some(LockRefused::Failed(
            "the state schema has no writer generation table (it is older than version 2): \
             run the installer to bring it forward"
                .to_owned()
        ))
    );
    assert_eq!(holder(&mut admin), None, "the lock was given back");
}

// Catches an increment the server refuses for another reason (here a trigger raises) taken
// for a missing table, or holding the lock: the refusal is the server's, by its SQLSTATE
// alone, and the lock is free after it.
#[test]
fn an_increment_the_server_refuses_is_its_refusal_and_frees_the_lock() {
    let database = test_database();
    let mut admin = Client::connect(&database.url(), NoTls).expect("connects");
    admin
        .batch_execute(
            "CREATE FUNCTION kbf_state.no_bump() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'no'; END $$; \
             CREATE TRIGGER no_bump BEFORE UPDATE ON kbf_state.writer_generation \
             FOR EACH ROW EXECUTE FUNCTION kbf_state.no_bump()",
        )
        .expect("a trigger that refuses the update");
    assert_eq!(
        WriterLock::acquire(&target(&database)).err(),
        Some(LockRefused::Failed(
            "the server refused (SQLSTATE P0001)".to_owned()
        ))
    );
    assert_eq!(holder(&mut admin), None, "the lock was given back");
}
