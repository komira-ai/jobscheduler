use postgres::config::Host;

use std::time::Duration;

use super::{
    ConfigError, Fail, MAX_ATTEMPTS, PostgresConfig, PostgresStore, Revision, Store, StoreError,
    Txn, describe,
};
use crate::Cond;

// Catches a Unix-socket URI that parses to a TCP host (or not at all): the frontend
// reaches Postgres over a socket directory, and `host=/dir` must be that directory.
#[test]
fn a_socket_uri_parses_to_a_unix_host() {
    let config = PostgresConfig::parse("postgres:///kbf?host=/run/postgresql")
        .expect("a socket directory is a host");
    assert_eq!(
        config.pg.get_hosts(),
        [Host::Unix("/run/postgresql".into())]
    );
    assert_eq!(config.pg.get_dbname(), Some("kbf"));
}

// Catches a URL form the install recipe uses parsing to the wrong socket, role, database
// or port (setup creates the role and database these accessors name, and the frontend
// connects with the same URL): the query form, the authority form, a hyphenated role,
// a port, and a URL with no role at all.
#[test]
fn the_urls_of_the_install_recipe_name_their_socket_role_and_database() {
    let parsed = |url: &str| PostgresConfig::parse(url).expect("the recipe's URL parses");
    for (url, user, db, port) in [
        (
            "postgresql:///kbf?host=/run/postgresql&user=kbf",
            "kbf",
            "kbf",
            None,
        ),
        (
            "postgres:///kbf?host=/run/postgresql&user=kbf-server",
            "kbf-server",
            "kbf",
            None,
        ),
        (
            "postgres://kbf-server@/state?host=/var/run/postgresql&port=5433",
            "kbf-server",
            "state",
            Some(5433),
        ),
    ] {
        let config = parsed(url);
        assert_eq!(config.user(), Some(user), "{url}");
        assert_eq!(config.dbname(), Some(db), "{url}");
        assert_eq!(config.port(), port, "{url}");
        assert!(
            config.socket_dir().is_some_and(|d| d.is_absolute()),
            "{url}"
        );
        assert!(!config.has_password(), "{url}");
    }
    assert_eq!(
        parsed("postgres:///kbf?host=/run/postgresql").user(),
        None,
        "a URL with no role leaves it to the process's own user"
    );
    assert_eq!(
        parsed("postgres://db.example/kbf?user=kbf").socket_dir(),
        None,
        "a host name is not a socket directory"
    );
}

// Catches a URI with no host being accepted (it would connect to a default socket the
// operator never chose).
#[test]
fn a_uri_with_no_host_is_refused() {
    assert_eq!(
        PostgresConfig::parse("postgres:///kbf").unwrap_err(),
        ConfigError::NoHost
    );
}

// Catches a URL with no application name leaving connections unnamed in
// `pg_stat_activity`, and an application name the URL set being overwritten.
#[test]
fn a_url_without_an_application_name_gets_the_crates_and_keeps_its_own() {
    let plain = PostgresConfig::parse("postgres://h/db").expect("parses");
    assert_eq!(plain.pg.get_application_name(), Some("kbf-state"));
    let named = PostgresConfig::parse("postgres://h/db?application_name=mine").expect("parses");
    assert_eq!(named.pg.get_application_name(), Some("mine"));
}

// Catches a URL refused for the wrong reason (or accepted): each way `parse` says no
// is its own error, and the answers that are yes keep the URL's own settings. All
// here, in one test binary, so the function is covered whole by one copy.
#[test]
fn each_way_a_url_is_refused_is_its_own_error() {
    let refused = |url: &str| PostgresConfig::parse(url).unwrap_err();
    assert_eq!(refused(" "), ConfigError::Missing);
    assert_eq!(refused("mysql://h/db"), ConfigError::NotPostgres);
    assert!(matches!(
        refused("postgres://h/db?sslmode=bogus"),
        ConfigError::Invalid(_)
    ));
    assert_eq!(refused("postgres:///db"), ConfigError::NoHost);
    assert_eq!(
        refused("postgres://h/db?sslmode=require"),
        ConfigError::TlsUnsupported
    );
    assert!(PostgresConfig::parse("postgresql://h/db?sslmode=prefer").is_ok());
}

// Catches a pool size of zero (a store that could never check a session out) and a
// size that is not kept.
#[test]
fn a_pool_has_at_least_one_connection() {
    let config = PostgresConfig::parse("postgres://h/db").expect("parses");
    assert_eq!(config.clone().with_max_connections(0).max_connections, 1);
    assert_eq!(config.with_max_connections(5).max_connections, 5);
}

// Catches a schema-name check that lets through a name Postgres truncates (over 63
// bytes) or one that starts with a digit.
#[test]
fn a_schema_name_is_plain_and_at_most_63_bytes() {
    let config = PostgresConfig::parse("postgres://h/db").expect("parses");
    let long = "a".repeat(64);
    assert!(config.clone().with_schema(&"a".repeat(63)).is_ok());
    assert_eq!(
        config.clone().with_schema(&long).unwrap_err(),
        ConfigError::BadSchema(long)
    );
    assert!(config.clone().with_schema("1abc").is_err());
    assert!(config.with_schema("Abc").is_err());
}

// Catches a URL that carries a password being reported as one without (the flag
// refuses a password), and one without being reported as having one.
#[test]
fn the_config_knows_whether_the_url_carries_a_password() {
    assert!(
        PostgresConfig::parse("postgres://u:pw@h/db")
            .expect("parses")
            .has_password()
    );
    assert!(
        !PostgresConfig::parse("postgres://u@h/db")
            .expect("parses")
            .has_password()
    );
}

// Catches a refusal the store made itself (not the server) being retried, and its
// text being replaced by a generic one.
#[test]
fn a_refusal_is_not_retried_and_keeps_its_words() {
    let refusal = Fail::Refused("synchronous_commit is off".to_owned());
    assert!(!refusal.retryable());
    assert_eq!(
        refusal.into_store(),
        StoreError::Failed("synchronous_commit is off".to_owned())
    );
}

fn url() -> String {
    js_testkit::database::server_url()
}

/// A store on a schema of its own, and a client on the same database.
fn store_and_client(schema: &str) -> (PostgresStore, postgres::Client) {
    let config = PostgresConfig::parse(&url())
        .and_then(|config| config.with_schema(schema))
        .expect("a usable URL and schema");
    let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .expect("a clean schema");
    installed(&config);
    (PostgresStore::open(&config).expect("opens"), client)
}

/// The schema in `config`, as the installer makes it.
fn installed(config: &PostgresConfig) {
    postgres::Client::connect(&url(), postgres::NoTls)
        .and_then(|mut client| {
            client.batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                config.schema()
            ))
        })
        .expect("a clean schema");
    super::schema::apply(config, &super::schema::embedded(), super::schema::latest())
        .expect("the installer's step");
}

/// The error the server gives for `code`.
fn raised(client: &mut postgres::Client, code: &str) -> postgres::Error {
    client
        .batch_execute(&format!(
            "DO $$ BEGIN RAISE EXCEPTION 'raised' USING ERRCODE = '{code}'; END $$"
        ))
        .expect_err("the block raises")
}

// Catches a serialization failure or a deadlock not being retried (the store would
// fail a transaction the server only asked it to run again), a failure that is
// neither being retried, and a retry loop with no end. The server's words name the
// state.
#[test]
fn only_serialization_failures_and_deadlocks_are_run_again_and_not_for_ever() {
    let (store, mut client) = store_and_client("kbf_unit_retry");
    // One result type throughout: the coverage of a generic function is that of its
    // best single instantiation.
    let tries = std::cell::Cell::new(0);
    let mut run = |codes: &[&str]| -> Result<(), StoreError> {
        tries.set(0);
        store.retrying(|| {
            tries.set(tries.get() + 1);
            match codes.get(tries.get() - 1) {
                Some(code) => Err(Fail::Pg(raised(&mut client, code))),
                None => Ok(()),
            }
        })
    };
    for code in ["40001", "40P01"] {
        assert_eq!(
            run(&[code, code]),
            Ok(()),
            "{code}: run again until it works"
        );
    }
    assert_eq!(tries.get(), 3, "two failures, then the run that worked");
    let refused = run(&["23505", "40001"]);
    assert_eq!(tries.get(), 1, "a unique violation is not run again");
    let said = format!("{refused:?}");
    assert!(said.contains("SQLSTATE 23505"), "{said}");
    let endless = run(&["40001"; MAX_ATTEMPTS + 1]);
    assert_eq!(tries.get(), MAX_ATTEMPTS);
    assert_eq!(
        endless,
        Err(StoreError::Failed(format!(
            "the transaction failed to serialize {MAX_ATTEMPTS} times"
        )))
    );
}

// Catches an error that is not the server's (no connection, a closed one) being
// described as the empty "db error", or with its cause dropped.
#[test]
fn an_error_that_is_not_the_servers_is_described_with_its_cause() {
    let refused = postgres::Client::connect("postgres://nobody@127.0.0.1:1/x", postgres::NoTls)
        .err()
        .expect("nothing listens on port 1");
    assert!(refused.as_db_error().is_none());
    let said = describe(&refused);
    assert!(said.contains(": "), "{said}");
    let (_store, mut client) = store_and_client("kbf_unit_describe");
    let pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .expect("pid")
        .get(0);
    let mut killer = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
    killer
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .expect("terminates");
    let closed = (0..2000)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(5));
            client
                .batch_execute("SELECT 1")
                .err()
                .filter(|e| e.as_db_error().is_none())
        })
        .expect("the dead connection fails without a database error");
    assert!(!describe(&closed).is_empty());
    assert_eq!(
        describe(&closed),
        closed.to_string(),
        "no cause to add: {closed:?}"
    );
}

// Catches an empty batch reaching for a connection: it must answer nothing without
// one (the store's sessions are all dead here, and a batch that begins a transaction
// would fail).
#[test]
fn an_empty_batch_answers_nothing_without_a_connection() {
    let config = PostgresConfig::parse(&url())
        .and_then(|config| config.with_schema("kbf_unit_empty"))
        .expect("a usable URL and schema")
        .with_application_name("kbf_unit_empty");
    installed(&config);
    let store = PostgresStore::open(&config).expect("opens");
    let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
    let sessions = |client: &mut postgres::Client| -> i64 {
        client
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'kbf_unit_empty'",
                &[],
            )
            .expect("the activity view")
            .get(0)
    };
    client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = 'kbf_unit_empty'",
            &[],
        )
        .expect("terminates");
    let gone = (0..2000).any(|_| {
        std::thread::sleep(Duration::from_millis(5));
        sessions(&mut client) == 0
    });
    assert!(gone, "the killed sessions go");
    assert!(store.apply_batch(Vec::new()).expect("answers").is_empty());
    // The same call with something in it needs a session: the dead one fails the
    // call, and the next call runs on a new connection. (Both ends of the function
    // run in this one test binary.)
    let put = || vec![Txn::new().put("k", "v", Cond::Any)];
    assert!(store.apply_batch(put()).is_err());
    assert_eq!(store.apply_batch(put()).expect("commits").len(), 1);
}

/// Asserts that each kind of feed row maps to its event, a full page resumes after its
/// last row and a short one at the head, a store with sequence revisions reads to the
/// end of the table, and a row of a kind this code does not know is left out of the
/// page rather than failing it: a full page of only such rows is empty and its cursor
/// still passes them, and the known rows after it are read.
#[test]
fn the_feed_maps_each_kind_pages_and_skips_a_kind_it_does_not_know() {
    use super::{Change, Event, Flaw};
    let (store, mut client) = store_and_client("kbf_unit_kind");
    client
        .batch_execute(
            "INSERT INTO kbf_unit_kind.feed (rev, key, kind, value) VALUES \
             (1, 'a', 0, 'v'), (2, 'b', 1, NULL), (3, 'c', 2, NULL); \
             UPDATE kbf_unit_kind.head SET rev = 5",
        )
        .expect("one row of each kind");
    let all = store.changes(Revision::ZERO, 10).expect("reads");
    assert_eq!(
        all.rows,
        [
            Change {
                revision: Revision(1),
                key: "a".into(),
                event: Event::Put(b"v".to_vec())
            },
            Change {
                revision: Revision(2),
                key: "b".into(),
                event: Event::Delete
            },
            Change {
                revision: Revision(3),
                key: "c".into(),
                event: Event::Expire
            },
        ]
    );
    assert_eq!(
        all.cursor,
        Revision(5),
        "a short page ends at the head, not its last row"
    );
    let first = store.changes(Revision::ZERO, 1).expect("reads");
    assert_eq!((first.rows.len(), first.cursor), (1, Revision(1)));
    let flawed = store.with_flaw(Flaw::SequenceRevisions);
    assert_eq!(
        flawed
            .changes(Revision::ZERO, 10)
            .expect("reads")
            .rows
            .len(),
        3
    );
    let store = flawed.with_flaw(Flaw::None);
    client
        .batch_execute(
            "INSERT INTO kbf_unit_kind.feed (rev, key, kind, value) VALUES \
             (6, 'k', 9, NULL), (7, 'm', 0, 'w'); \
             UPDATE kbf_unit_kind.head SET rev = 7",
        )
        .expect("a row of a kind from the future, then a put");
    let after = Change {
        revision: Revision(7),
        key: "m".into(),
        event: Event::Put(b"w".to_vec()),
    };
    let page = store.changes(Revision(3), 10).expect("reads");
    assert_eq!(page.rows, std::slice::from_ref(&after));
    assert_eq!(page.cursor, Revision(7));
    let skipped = store.changes(Revision(3), 1).expect("reads");
    assert_eq!((skipped.rows.len(), skipped.cursor), (0, Revision(6)));
    let resumed = store.changes(skipped.cursor, 1).expect("reads");
    assert_eq!(resumed.rows, [after]);
    assert_eq!(resumed.cursor, Revision(7));
}

/// The backend of the session a read of `store` runs on.
fn backend_pid(store: &PostgresStore) -> i32 {
    store
        .read(|s| Ok(s.client.query_one("SELECT pg_backend_pid()", &[])?.get(0)))
        .expect("reads the backend")
}

/// A store on a schema of its own, installed fresh.
fn store_on(schema: &str) -> (PostgresConfig, PostgresStore) {
    let config = PostgresConfig::parse(&url())
        .and_then(|config| config.with_schema(schema))
        .expect("a usable URL and schema");
    installed(&config);
    let store = PostgresStore::open(&config).expect("opens");
    (config, store)
}

// Catches a read that fails on its session and still pools it: the next read must run on
// another connection. A statement the server refuses keeps the session (the same backend
// answers the next read); a parameter the client cannot encode leaves the connection in a
// state the pool does not judge, so the session is dropped; and a backend that ends itself
// in the statement (the server's FATAL) is never handed out again, over many rounds
// because the client's own closed flag lags the error it reports.
#[test]
fn a_read_that_fails_keeps_its_session_only_when_the_server_refused_a_statement() {
    let (_config, store) = store_on("kbf_unit_read_failure");
    let first = backend_pid(&store);

    store
        .read(|s| s.client.batch_execute("SELECT 1/0"))
        .expect_err("the server refuses the statement");
    assert_eq!(backend_pid(&store), first, "a refused statement keeps it");

    store
        .read(|s| s.client.query("SELECT $1::int8", &[&"not a number"]))
        .expect_err("the client cannot encode the parameter");
    let second = backend_pid(&store);
    assert_ne!(second, first, "a failure the server did not answer drops it");

    let mut before = second;
    for _ in 0..50 {
        store
            .read(|s| {
                s.client
                    .batch_execute("SELECT pg_terminate_backend(pg_backend_pid())")
            })
            .expect_err("the backend ends itself");
        let after = backend_pid(&store);
        assert_ne!(after, before, "the dead session was handed out again");
        before = after;
    }
}

// Catches a commit that loses its backend and still pools the session. A deferred trigger
// ends the backend inside COMMIT, so the server's answer to COMMIT is the FATAL (a
// database error, which is not a statement the server merely refused): the next call must
// run on another connection.
#[test]
fn a_commit_the_server_ends_the_backend_in_does_not_return_its_session() {
    let (config, store) = store_on("kbf_unit_commit_failure");
    let schema = config.schema();
    postgres::Client::connect(&url(), postgres::NoTls)
        .and_then(|mut client| {
            client.batch_execute(&format!(
                "CREATE FUNCTION {schema}.end_backend() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$; \
                 CREATE CONSTRAINT TRIGGER end_at_commit AFTER INSERT OR UPDATE ON {schema}.kv \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.end_backend()"
            ))
        })
        .expect("the trigger");
    let mut before = backend_pid(&store);
    for _ in 0..50 {
        let pending = store
            .begin(Txn::new().put("k", "v", Cond::Any))
            .expect("runs to the commit");
        pending.commit().expect_err("the backend ends in the commit");
        let after = backend_pid(&store);
        assert_ne!(after, before, "the dead session was handed out again");
        before = after;
    }
}
