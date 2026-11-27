//! A held dispatch poll woken by another server's notice (`docs/design/multi-instance.md`, G16):
//! the message another server wrote is handed out before the hold's bound, with the recheck set
//! past any test ([`NEVER`]), so only a wake can answer the poll with it. The two servers share
//! the fake store; the notices go through a real Postgres database, sent as the other server's
//! store sends them after its commit and heard by this server's listener
//! ([`crate::listening::listen`]). Nothing waits on time: the waits the poll counts mark each
//! step.

use std::time::Duration;

use js_jobs::{CHANNELS, Wake};
use js_state::postgres::{
    ExtensionPool, Heard, Listener, PostgresConfig, PostgresStore, TestDatabase, test_database,
};
use tokio::runtime::Runtime;

use super::*;
use crate::listening::{heard_by, listen};

/// The application name the listener connects under, which finds its server process.
const LISTENING: &str = "js-dispatch-listener";

/// A database for the notices, its configuration and a store that sends on it, made outside any
/// runtime (the test database's own client blocks).
fn notices() -> (TestDatabase, PostgresConfig, PostgresStore) {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).unwrap();
    let store = PostgresStore::open(&config).unwrap();
    (database, config, store)
}

/// A listener on the job store's channels of `config`'s database, for `rig`, making a lost
/// connection again at once.
fn listening(rig: &Rig, config: &PostgresConfig) -> crate::listening::Listening {
    let named = config.clone().with_application_name(LISTENING);
    let listener = Listener::new(&named, &CHANNELS);
    listen(Arc::clone(&rig.agents), listener, Duration::ZERO)
}

/// Ends the listener's connection from the server's side, as a database restart would.
fn end_listener(config: &PostgresConfig) {
    let pool = ExtensionPool::open(config).unwrap();
    let mut loan = pool.checkout().unwrap();
    let row = loan
        .conn()
        .query_one(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE datname = current_database() AND application_name = $1",
            &[&LISTENING],
        )
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1, "one listening connection");
}

/// Catches a held poll that a host notice of another server does not wake (its message would wait
/// for the recheck, or the next poll). The other server places a job on the host after the poll is
/// held; its notice for the host ends the hold with the start, the recheck never having come.
#[tokio::test]
async fn a_host_notice_ends_a_held_poll_with_the_message_another_server_wrote() {
    let a = timed(server("notice_route_a", "server-a"), LONG, NEVER);
    let b = second(&a, "notice_route_b", "server-b");
    let writing = async {
        waited(&a, 1).await;
        b.place("job-1", "host-a");
        heard_by(&a.agents, &Heard::Notice(Wake::Host(host()).notice()));
    };
    let (reply, ()) = tokio::join!(polled(&a, Vec::new()), writing);
    assert_eq!(names(&reply.unwrap()), [start("job-1")]);
    assert_eq!(waits(&a), 1, "answered at the first wake");
}

/// Catches a server that does not listen to the database, or does not act on what it hears. With
/// a poll held, the listener's connection is made, which wakes the poll once (nothing is due
/// yet); the other server places a job on the host and its notice goes out through Postgres; the
/// poll is answered with the start before its hold ends.
#[test]
fn a_notice_through_postgres_ends_a_poll_held_on_another_server() {
    let (_database, config, store) = notices();
    Runtime::new().unwrap().block_on(async {
        let a = timed(server("notice_pg_a", "server-a"), LONG, NEVER);
        let b = second(&a, "notice_pg_b", "server-b");
        let writing = async {
            waited(&a, 1).await;
            let listening = listening(&a, &config);
            waited(&a, 2).await;
            b.place("job-1", "host-a");
            store.notify(&[Wake::Host(host()).notice()]);
            listening
        };
        let (reply, listening) = tokio::join!(polled(&a, Vec::new()), writing);
        assert_eq!(names(&reply.unwrap()), [start("job-1")]);
        tokio::task::spawn_blocking(move || listening.join())
            .await
            .unwrap();
    });
}

/// Catches a listener that comes back from a lost connection without waking the waits (a notice
/// sent while it was down is missed, and the poll would hold to its bound). With a poll held and
/// the listener connected, the other server places a job on the host and its notice is lost; the
/// database ends the listening connection; the listener connects again and wakes the poll, which
/// finds the start.
#[test]
fn a_listening_connection_made_again_wakes_a_held_poll_to_what_it_missed() {
    let (_database, config, _store) = notices();
    Runtime::new().unwrap().block_on(async {
        let a = timed(server("notice_relisten_a", "server-a"), LONG, NEVER);
        let b = second(&a, "notice_relisten_b", "server-b");
        let writing = async {
            waited(&a, 1).await;
            let listening = listening(&a, &config);
            waited(&a, 2).await;
            b.place("job-1", "host-a");
            let config = config.clone();
            tokio::task::spawn_blocking(move || end_listener(&config))
                .await
                .unwrap();
            listening
        };
        let (reply, listening) = tokio::join!(polled(&a, Vec::new()), writing);
        assert_eq!(names(&reply.unwrap()), [start("job-1")]);
        tokio::task::spawn_blocking(move || listening.join())
            .await
            .unwrap();
    });
}
