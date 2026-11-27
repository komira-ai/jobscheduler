//! Notices between the processes of one database against a real server: what
//! `Store::notify` sends after a commit, and what a `postgres::Listener` on a connection of its
//! own hears, loses and hears again. Each test makes a database of its own.
//!
//! No test waits on a clock: a wait for a notice is long (`LONG`), so a notice that is not sent
//! shows as a `Quiet` where the notice was due, never as a test that got slower.
#![cfg(feature = "postgres")]

use std::time::Duration;

use js_state::postgres::{
    ExtensionPool, Heard, Listener, PostgresConfig, PostgresStore, test_database,
};
use js_state::{Cond, Notice, Store, StoreError, Txn};

/// Longer than any notice takes to arrive: the wait for one that is due.
const LONG: Duration = Duration::from_secs(60);

/// The two channels the tests listen on.
const CHANNELS: [&str; 2] = ["js_test_queue", "js test \"host\""];

/// The application name the listener connects under, which finds its server process.
const LISTENING: &str = "js-notify-listener";

fn notice(channel: &str, payload: &str) -> Notice {
    Notice {
        channel: channel.to_owned(),
        payload: payload.to_owned(),
    }
}

fn config(database: &js_testkit::database::TestDatabase) -> PostgresConfig {
    PostgresConfig::parse(&database.url()).expect("the test URL parses")
}

/// A listener of `config`'s database on [`CHANNELS`], connected and listening.
fn listening(config: &PostgresConfig) -> Listener {
    let mut listener = Listener::new(&config.clone().with_application_name(LISTENING), &CHANNELS);
    assert_eq!(listener.next(LONG), Heard::Listening);
    listener
}

/// Ends the listener's connection from the server's side, as a restart or an administrator
/// would, and says how many connections it ended.
fn end_listener(pool: &ExtensionPool) -> i64 {
    let mut loan = pool.checkout().expect("a connection");
    let row = loan
        .conn()
        .query_one(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE datname = current_database() AND application_name = $1",
            &[&LISTENING],
        )
        .expect("the listener's connection is ended");
    row.get(0)
}

// Asserts that the notices one store sends after its commit reach a listener on a connection of
// its own, each with its channel and payload, in the order sent, and that a notice of a channel
// it does not listen on is not heard: the next thing heard after it is the listened one. It
// catches a store that sends nothing, a payload or channel lost or swapped, and a listener that
// hears every channel.
#[test]
fn the_notices_a_store_sends_reach_a_listener_of_the_database() {
    let database = test_database();
    let config = config(&database);
    let store = PostgresStore::open(&config).expect("opens");
    let mut listener = listening(&config);
    store
        .txn(Txn::new().put("job/a", b"1".to_vec(), Cond::Absent))
        .expect("the store")
        .expect("commits");
    store.notify(&[
        notice("js_test_elsewhere", "not listened"),
        notice(CHANNELS[0], ""),
        notice(CHANNELS[1], "host-a"),
    ]);
    assert_eq!(listener.next(LONG), Heard::Notice(notice(CHANNELS[0], "")));
    assert_eq!(
        listener.next(LONG),
        Heard::Notice(notice(CHANNELS[1], "host-a"))
    );
}

// Asserts that a listener with nothing to hear answers quiet at the end of its wait, and still
// hears the next notice. It catches a quiet wait taken for a lost connection (the listener would
// connect again, and say every waiter is to look again, at every quiet wait).
#[test]
fn a_listener_with_nothing_to_hear_is_quiet_and_keeps_its_connection() {
    let database = test_database();
    let config = config(&database);
    let store = PostgresStore::open(&config).expect("opens");
    let mut listener = listening(&config);
    assert_eq!(listener.next(Duration::from_millis(1)), Heard::Quiet);
    store.notify(&[notice(CHANNELS[0], "after the quiet")]);
    assert_eq!(
        listener.next(LONG),
        Heard::Notice(notice(CHANNELS[0], "after the quiet"))
    );
}

// Asserts that a listener whose connection the server ends says it is lost, that its next call
// makes the connection again and says so (Listening), that a notice sent while it had no
// connection is not heard after it, which is why Listening means "look again", and that it
// hears the notices sent after. It catches a listener that never connects again, one that
// connects again without saying so (its caller's waiters would miss what was sent meanwhile),
// and one that listens on the new connection on nothing.
#[test]
fn a_listener_whose_connection_ends_is_lost_then_listens_again() {
    let database = test_database();
    let config = config(&database);
    let store = PostgresStore::open(&config).expect("opens");
    let pool = ExtensionPool::open(&config).expect("a pool");
    let mut listener = listening(&config);
    assert_eq!(end_listener(&pool), 1, "one listening connection");
    assert!(
        matches!(
            listener.next(LONG),
            Heard::Lost(StoreError::Disconnected(_))
        ),
        "an ended connection is lost"
    );
    store.notify(&[notice(CHANNELS[0], "while lost")]);
    assert_eq!(listener.next(LONG), Heard::Listening);
    assert_eq!(listener.next(Duration::from_millis(1)), Heard::Quiet);
    store.notify(&[notice(CHANNELS[1], "after")]);
    assert_eq!(
        listener.next(LONG),
        Heard::Notice(notice(CHANNELS[1], "after"))
    );
}

// Asserts that a listener of a database it cannot reach says it is lost at each call and does
// not panic or block. It catches a connect failure that is raised, or taken for a connection.
#[test]
fn a_listener_that_cannot_connect_is_lost_at_each_call() {
    let database = test_database();
    let config = config(&database);
    drop(database);
    let mut listener = Listener::new(&config, &CHANNELS);
    assert!(matches!(listener.next(LONG), Heard::Lost(_)));
    assert!(matches!(listener.next(LONG), Heard::Lost(_)));
}

// Asserts that a notice the server refuses (a payload over its 8000 bytes) fails nothing: the
// call returns, the store reads and writes on, and the notices sent after it are heard. It
// catches a refused notice that is raised (a write already committed would be reported failed)
// or that leaves the session broken.
#[test]
fn a_notice_the_server_refuses_is_dropped_and_the_store_goes_on() {
    let database = test_database();
    let config = config(&database);
    let store = PostgresStore::open(&config).expect("opens");
    let mut listener = listening(&config);
    store.notify(&[notice(CHANNELS[0], &"x".repeat(8001))]);
    store
        .txn(Txn::new().put("job/b", b"2".to_vec(), Cond::Absent))
        .expect("the store")
        .expect("commits");
    assert!(store.get("job/b").expect("reads").is_some());
    store.notify(&[notice(CHANNELS[0], "next")]);
    assert_eq!(
        listener.next(LONG),
        Heard::Notice(notice(CHANNELS[0], "next"))
    );
}
