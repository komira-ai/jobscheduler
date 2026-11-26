//! A stamped transaction ([`Store::txn_stamped`]) on a real Postgres server: one that only
//! creates keys is three round trips (the `BEGIN` that takes the commit head and reads the
//! clock, one statement, the `COMMIT`) where today's path takes eight, it writes exactly the
//! rows and feed rows today's path writes, and it queues behind a holder of the commit head as
//! every writer does.
//!
//! The tests need a server (`KBF_TEST_POSTGRES_URL` names it) and fail without one. Waits are on
//! conditions (a connection queued on a lock); no assertion depends on elapsed time.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use js_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore};
use js_state::{Cond, Conflict, ManualClock, Store, Txn};
use postgres::{Client, NoTls};

fn url() -> String {
    js_testkit::database::server_url()
}

/// A row of `kv`: key, value, version, expiry.
type KvRow = (String, Vec<u8>, i64, Option<i64>);
/// A row of `feed`: revision, key, kind, value.
type FeedRow = (i64, String, i16, Option<Vec<u8>>);

/// Every row of the schema of `config`, of `kv` in key order and of `feed` in revision order, as
/// stored.
fn rows(config: &PostgresConfig) -> (Vec<KvRow>, Vec<FeedRow>) {
    let mut client = Client::connect(&url(), NoTls).expect("a reading connection");
    let schema = config.schema();
    let kv = client
        .query(
            &format!("SELECT key, value, version, expires_us FROM {schema}.kv ORDER BY key"),
            &[],
        )
        .expect("kv reads");
    let feed = client
        .query(
            &format!("SELECT rev, key, kind, value FROM {schema}.feed ORDER BY rev"),
            &[],
        )
        .expect("feed reads");
    let kv = kv
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    let feed = feed
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    (kv, feed)
}

/// Two stores on fresh schemas of `backend` that read `clock`: one written today's way, one
/// stamped.
fn pair(
    backend: &PostgresBackend,
    clock: &Arc<ManualClock>,
) -> [(PostgresConfig, PostgresStore); 2] {
    [(), ()].map(|()| {
        let config = backend.fresh_config();
        let store = PostgresStore::open(&config)
            .expect("the test database opens")
            .with_clock(Arc::clone(clock) as Arc<dyn js_state::Clock>);
        (config, store)
    })
}

/// The transaction a submit commits, in shape: the job's row on `Absent` and its two index
/// entries on `Any`.
fn submit_shaped(value: &str) -> Txn {
    Txn::new()
        .put("job/a", value.as_bytes().to_vec(), Cond::Absent)
        .put("idx/job/active/a", Vec::new(), Cond::Any)
        .put("idx/job/queued/a", Vec::new(), Cond::Any)
}

// Catches a stamped insert that takes more round trips than its three, and one whose rows
// differ from today's: a version that is not its put's revision, a feed row out of order or
// of another kind, a left-over placeholder, an expiry where there is none. The golden is the
// rows today's path (`Store::txn`) writes, which the stamped path must equal row for row.
#[test]
fn a_transaction_that_creates_keys_is_three_round_trips_and_writes_todays_rows() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let clock = Arc::new(ManualClock::default());
    let [(today_config, today), (stamped_config, stamped)] = pair(&backend, &clock);
    for store in [&today, &stamped] {
        store
            .txn(Txn::new().put("seed", b"s".to_vec(), Cond::Any))
            .expect("the store is reachable")
            .expect("the seed commits");
    }

    let before = today.statements();
    let made = today.txn(submit_shaped("record")).expect("reachable");
    assert_eq!(today.statements() - before, 8, "today's path");
    let before = stamped.statements();
    let fast = stamped
        .txn_stamped(&mut |_| Some(submit_shaped("record")))
        .expect("reachable");
    assert_eq!(
        stamped.statements() - before,
        3,
        "BEGIN with the head and the clock, the statement, COMMIT"
    );
    assert_eq!(fast, Some(made.clone()), "the commit's revision");
    assert_eq!(made.map(|commit| commit.revision.0), Ok(4));

    let golden: (Vec<KvRow>, Vec<FeedRow>) = (
        vec![
            ("idx/job/active/a".to_owned(), Vec::new(), 3, None),
            ("idx/job/queued/a".to_owned(), Vec::new(), 4, None),
            ("job/a".to_owned(), b"record".to_vec(), 2, None),
            ("seed".to_owned(), b"s".to_vec(), 1, None),
        ],
        vec![
            (1, "seed".to_owned(), 0, Some(b"s".to_vec())),
            (2, "job/a".to_owned(), 0, Some(b"record".to_vec())),
            (3, "idx/job/active/a".to_owned(), 0, Some(Vec::new())),
            (4, "idx/job/queued/a".to_owned(), 0, Some(Vec::new())),
        ],
    );
    assert_eq!(rows(&today_config), golden, "today's path");
    assert_eq!(rows(&stamped_config), golden, "the stamped path");
}

// Catches a stamped insert that writes over a key that holds a row instead of judging it as
// today's path does: a live row is the same conflict, with nothing written; a row the clock
// expired is expired (its own feed row) and then put, at the same revisions as today's.
#[test]
fn a_transaction_over_a_key_that_holds_a_row_writes_what_todays_path_writes() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let clock = Arc::new(ManualClock::default());
    let [(today_config, today), (stamped_config, stamped)] = pair(&backend, &clock);
    for store in [&today, &stamped] {
        let seed = Txn::new()
            .put("job/live", b"kept".to_vec(), Cond::Any)
            .put_ttl("job/a", b"old".to_vec(), Duration::from_secs(1), Cond::Any);
        store
            .txn(seed)
            .expect("reachable")
            .expect("the seed commits");
    }
    let live = || {
        Txn::new().put("job/new", b"n".to_vec(), Cond::Absent).put(
            "job/live",
            b"x".to_vec(),
            Cond::Absent,
        )
    };
    let conflict = Conflict {
        index: 1,
        key: "job/live".to_owned(),
    };
    assert_eq!(today.txn(live()), Ok(Err(conflict.clone())));
    let stamped_live = stamped
        .txn_stamped(&mut |_| Some(live()))
        .expect("reachable");
    assert_eq!(stamped_live, Some(Err(conflict)));

    clock.advance(Duration::from_secs(1));
    let made = today.txn(submit_shaped("record")).expect("reachable");
    let fast = stamped
        .txn_stamped(&mut |_| Some(submit_shaped("record")))
        .expect("reachable");
    assert_eq!(fast, Some(made));
    let (today_rows, stamped_rows) = (rows(&today_config), rows(&stamped_config));
    assert_eq!(stamped_rows, today_rows);
    let kinds: Vec<(String, i16)> = today_rows
        .1
        .iter()
        .skip(2)
        .map(|(_, key, kind, _)| (key.clone(), *kind))
        .collect();
    let expected = [
        ("job/a", 2),
        ("job/a", 0),
        ("idx/job/active/a", 0),
        ("idx/job/queued/a", 0),
    ];
    assert_eq!(kinds, expected.map(|(key, kind)| (key.to_owned(), kind)));
}

// Catches a stamped insert that does not take the commit head the way every writer does
// (`LOCK TABLE head`, which a claimed decision holds while it reads and an older release's
// tail takes): with another connection holding that lock, the insert waits, is not visible,
// and commits once the lock is let go.
#[test]
fn a_stamped_insert_waits_for_a_holder_of_the_commit_head() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = backend.fresh_config();
    let app = config.schema().to_owned();
    let store = Arc::new(PostgresStore::open(&config).expect("the test database opens"));
    let mut holder = Client::connect(&url(), NoTls).expect("a holding connection");
    holder
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {app}.head IN SHARE UPDATE EXCLUSIVE MODE"
        ))
        .expect("the head is held");
    let mut monitor = Client::connect(&url(), NoTls).expect("a monitoring connection");
    let writer = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            store.txn_stamped(&mut |_| Some(Txn::new().put("w/a", b"1".to_vec(), Cond::Absent)))
        })
    };
    let mut polls = 0_u32;
    loop {
        let waiting: i64 = monitor
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE application_name = $1 AND wait_event_type = 'Lock'",
                &[&app],
            )
            .expect("the activity view is readable")
            .get(0);
        if waiting == 1 {
            break;
        }
        polls += 1;
        assert!(
            polls < 2_000_000,
            "the stamped insert never queued on the head"
        );
        thread::yield_now();
    }
    assert_eq!(
        store.get("w/a").expect("reachable"),
        None,
        "it committed past the head"
    );
    holder.batch_execute("COMMIT").expect("the head is let go");
    let commit = writer.join().expect("no panic").expect("reachable");
    assert!(matches!(commit, Some(Ok(_))), "{commit:?}");
    assert!(store.get("w/a").expect("reachable").is_some());
}
