//! The pool a crate that extends the schema opens (`ExtensionPool`), the loans it makes, the
//! URL parts the configuration reports, and a backend that carries extra migrations. Added with
//! the move to this repository: each is used by a crate that is not here, so none had a test
//! of its own in this crate.
#![cfg(feature = "postgres")]

use js_state::StoreError;
use js_state::postgres::schema::{self, Migration};
use js_state::postgres::{
    ExtensionPool, Flaw, PostgresBackend, PostgresConfig, store_error, test_database,
};

/// Asserts that a pool opened on a database with the schema lends a connection that runs a
/// query, and that a loan marked discarded is not handed to the next caller: the next
/// checkout is a connection that is open and answers.
#[test]
fn a_pool_lends_connections_and_drops_a_discarded_one() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    let pool = ExtensionPool::open(&config).expect("the pool opens on the schema");
    {
        let mut loan = pool.checkout().expect("a connection");
        let row = loan.conn().query_one("SELECT 7::int", &[]).expect("runs");
        assert_eq!(row.get::<_, i32>(0), 7);
        loan.discard();
    }
    let mut loan = pool.checkout().expect("a new connection after the discard");
    assert!(!loan.conn().is_closed());
    let row = loan.conn().query_one("SELECT 8::int", &[]).expect("runs");
    assert_eq!(row.get::<_, i32>(0), 8);
}

/// Asserts that a loan whose failure was reported is dropped unless the server only refused a
/// statement: a refused statement keeps the connection, and a backend that ends itself in the
/// statement is never lent again, however often it is tried.
#[test]
fn a_reported_failure_drops_the_connection_unless_the_server_only_refused() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    let pool = ExtensionPool::open(&config).expect("the pool opens");
    let pid = || -> i32 {
        let mut loan = pool.checkout().expect("a connection");
        let row = loan
            .conn()
            .query_one("SELECT pg_backend_pid()", &[])
            .expect("a live connection");
        row.get(0)
    };
    let fail = |statement: &str| {
        let mut loan = pool.checkout().expect("a connection");
        let err = loan
            .conn()
            .batch_execute(statement)
            .expect_err("the statement fails");
        loan.note_failure(&err);
    };
    let first = pid();
    fail("SELECT 1/0");
    assert_eq!(pid(), first, "a refused statement keeps the connection");
    let mut before = first;
    for _ in 0..50 {
        fail("SELECT pg_terminate_backend(pg_backend_pid())");
        let after = pid();
        assert_ne!(after, before, "the dead connection was lent again");
        before = after;
    }
}

/// Asserts that a failed client call becomes a `StoreError::Failed` that says what the server
/// said.
#[test]
fn a_failed_call_is_reported_as_a_store_error() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    let pool = ExtensionPool::open(&config).expect("the pool opens");
    let mut loan = pool.checkout().expect("a connection");
    let err = loan
        .conn()
        .batch_execute("SELECT * FROM no_such_table_here")
        .expect_err("the table does not exist");
    match store_error(err) {
        StoreError::Failed(why) => assert!(why.contains("no_such_table_here"), "{why}"),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Asserts that the counts follow the URL: one host and port for a plain URL, two each for a
/// list in the authority, and the default port counted when the URL names none.
#[test]
fn the_configuration_counts_the_hosts_and_ports_of_its_url() {
    let one = PostgresConfig::parse("postgres://u@db.example:5433/state").expect("parses");
    assert_eq!((one.host_count(), one.port_count()), (1, 1));
    let two =
        PostgresConfig::parse("postgres://u@a.example:5433,b.example:5434/state").expect("parses");
    assert_eq!((two.host_count(), two.port_count()), (2, 2));
    let bare = PostgresConfig::parse("postgres://u@a.example/state").expect("parses");
    assert_eq!((bare.host_count(), bare.port_count()), (1, 1));
}

/// Asserts that a backend given more migrations makes every schema at their version, not the
/// embedded one.
#[test]
fn a_backend_with_more_migrations_makes_schemas_at_their_version() {
    let database = test_database();
    let more = Migration::new(
        schema::latest() + 1,
        "extra",
        "-- kbf-migration: expand\nCREATE TABLE extra_rows (id bigint PRIMARY KEY);\n",
    )
    .expect("the file lints");
    let backend = PostgresBackend::new(&database.url(), Flaw::None).with_migrations(vec![more]);
    let config = backend.fresh_config();
    assert_eq!(schema::current(&config), Ok(schema::latest() + 1));
}
