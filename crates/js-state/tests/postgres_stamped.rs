//! The transactions of a real Postgres server in fewer statements: one that only creates keys,
//! as a submit's does, loads nothing (the statement that reserves its keys says none holds a
//! row), so it is seven round trips (`BEGIN`, the clock, the reservation, `LOCK TABLE head`, the
//! head's move, the writes, `COMMIT`) where it was eight, stamped ([`Store::txn_stamped`]) or
//! not; it writes exactly the rows and feed rows the eight wrote (the goldens below were taken
//! from them); and a stamped one queues behind a holder of the commit head as every writer
//! does.
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

/// Two stores on fresh schemas of `backend` that read `clock`: one written through
/// [`Store::txn`], one stamped.
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

// Catches a transaction that creates keys taking more round trips than its seven (a load of
// keys the reservation already found absent), and one whose rows differ from those the eight
// round trips wrote: a version that is not its put's revision, a feed row out of order or of
// another kind, a left-over placeholder, an expiry where there is none. The golden is the rows
// the eight wrote.
#[test]
fn a_transaction_that_creates_keys_is_seven_round_trips_and_writes_the_rows_it_wrote_in_eight() {
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
    let seven = "BEGIN, the clock, the reservation, LOCK TABLE head, the move, the writes, COMMIT";
    assert_eq!(today.statements() - before, 7, "{seven}");
    let before = stamped.statements();
    let fast = stamped
        .txn_stamped(&mut |_| Some(submit_shaped("record")))
        .expect("reachable");
    assert_eq!(stamped.statements() - before, 7, "stamped: {seven}");
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
    assert_eq!(rows(&today_config), golden, "through txn");
    assert_eq!(rows(&stamped_config), golden, "stamped");
}

// Catches a transaction over a key that holds a row judged otherwise than the eight round trips
// judged it: a live row is the conflict of its operation, with nothing written; a row the clock
// expired is expired (its own feed row) and then put, at the revisions the eight took. The
// golden is the rows the eight wrote.
#[test]
fn a_transaction_over_a_key_that_holds_a_row_writes_what_it_wrote_in_eight() {
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
    let golden: (Vec<KvRow>, Vec<FeedRow>) = (
        vec![
            ("idx/job/active/a".to_owned(), Vec::new(), 5, None),
            ("idx/job/queued/a".to_owned(), Vec::new(), 6, None),
            ("job/a".to_owned(), b"record".to_vec(), 4, None),
            ("job/live".to_owned(), b"kept".to_vec(), 1, None),
        ],
        vec![
            (1, "job/live".to_owned(), 0, Some(b"kept".to_vec())),
            (2, "job/a".to_owned(), 0, Some(b"old".to_vec())),
            (3, "job/a".to_owned(), 2, None),
            (4, "job/a".to_owned(), 0, Some(b"record".to_vec())),
            (5, "idx/job/active/a".to_owned(), 0, Some(Vec::new())),
            (6, "idx/job/queued/a".to_owned(), 0, Some(Vec::new())),
        ],
    );
    assert_eq!(rows(&today_config), golden, "through txn");
    assert_eq!(rows(&stamped_config), golden, "stamped");
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
