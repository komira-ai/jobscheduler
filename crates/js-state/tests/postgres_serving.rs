//! `postgres::serving::probe` against a real server: who is connected under an application
//! name, read without a lock. Each test makes a database of its own.
#![cfg(feature = "postgres")]

use js_state::postgres::{PostgresConfig, PostgresStore, serving, test_database};
use js_state::{Store, WriterLock, WriterTarget};

/// A configuration for `database` that connects under the application name `name`.
fn named(database: &js_testkit::database::TestDatabase, name: &str) -> PostgresConfig {
    PostgresConfig::parse(&database.url())
        .expect("the test URL parses")
        .with_application_name(name)
}

// Asserts that a process connected under an application name is found as `<pid>@<start>`,
// by the same name and by nothing else, in its own database only, and that two processes
// under one name are not told apart by a lock (there is none): both open and write. It
// catches a probe that answers for any session of the database, or one that reads another
// database's sessions.
#[test]
fn a_connected_process_is_found_by_its_application_name() {
    let database = test_database();
    let config = named(&database, "js-serving-a");
    assert_eq!(serving::probe(&config), Ok(None), "nobody is connected yet");

    let first = PostgresStore::open(&config).expect("opens");
    first.get("k").expect("reads");
    let found = serving::probe(&config).expect("probes").expect("found");
    let (pid, started) = found.split_once('@').expect("<pid>@<start>");
    assert!(pid.parse::<u32>().is_ok(), "{found}");
    assert!(
        started.parse::<i64>().is_ok_and(|micros| micros > 0),
        "{found}"
    );

    let other = named(&database, "js-serving-b");
    assert_eq!(
        serving::probe(&other),
        Ok(None),
        "another name is not found"
    );

    let second = PostgresStore::open(&config).expect("a second process opens beside the first");
    second.get("k").expect("reads");
    assert!(serving::probe(&config).expect("probes").is_some());
    drop((first, second));
}

// Asserts that a server's refusal is reported by its SQLSTATE alone, and a server that cannot
// be reached as the client says it, neither carrying the URL. It catches a refusal that
// quotes the server's text (which can name the role or the database).
#[test]
fn a_probe_that_fails_says_why_without_the_url() {
    let database = test_database();
    let missing = PostgresConfig::parse(&database.url().replacen(
        database.name(),
        "no_such_database",
        1,
    ))
    .expect("parses");
    assert_eq!(
        serving::probe(&missing),
        Err("the server refused (SQLSTATE 3D000)".to_owned())
    );

    let nowhere = PostgresConfig::parse("postgres://nobody@127.0.0.1:1/none").expect("parses");
    let said = serving::probe(&nowhere).expect_err("nothing listens there");
    assert!(!said.contains("127.0.0.1:1/none"), "{said}");
}

// Asserts that the writer probe of a Postgres target is the serving probe: nobody is
// `Free`, and a connected process is `Held` with its text.
#[test]
fn the_writer_probe_of_a_postgres_target_reads_the_sessions() {
    let database = test_database();
    let config = named(&database, "js-serving-c");
    let target = WriterTarget::Postgres(Box::new(config.clone()));
    assert_eq!(
        WriterLock::probe(&target).expect("probes"),
        js_state::LockState::Free
    );

    let store = PostgresStore::open(&config).expect("opens");
    store.get("k").expect("reads");
    let held = WriterLock::probe(&target).expect("probes");
    assert!(matches!(held, js_state::LockState::Held { .. }), "{held:?}");
    drop(store);
}
