//! The store family: the server's state in the store `--store` names, across a restart, a
//! stop in a burst of hellos, a second server on the same store, and a crash of the
//! scaler's launch (`cell/store.rs`). Each scenario runs over SQLite in every build, over
//! sixteen seeds, and with the crate's `postgres` feature over Postgres too, over eight
//! seeds, a database of its own for each run, under every invariant of `Invariant::ALL`.
//!
//! With the feature, the same seeds must give the same trace over both stores: SQLite is a
//! stand-in for Postgres in tests, and a difference is a finding about the stand-in.
//! `the_sqlite_and_postgres_stores_give_the_same_trace_hash` compares a fixed list of
//! scenarios and seeds, and the restart's Postgres runs against the golden lines of family
//! `store` (`tests/golden/trace-hashes.txt`, pinned over SQLite by `tests/replay.rs`). The
//! Postgres tests need the server `KBF_TEST_POSTGRES_URL` names (the `state-postgres` CI
//! job sets it) and fail, never skip, without it.
//!
//! Not run: a failed commit that changes nothing the client can see (design section 8,
//! row 27). The server opens its store from `--store` and takes no store from its caller,
//! so the cell has no way to fail one of its commits without a seam the server does not
//! have (`docs/simulation.md`, "What the cell does not model").

#[path = "cell/store.rs"]
#[cfg_attr(
    not(feature = "postgres"),
    allow(
        dead_code,
        reason = "the golden names are read only by the agreement test of the postgres feature"
    )
)]
mod world;

use js_sim_cell::{Scenario, Store};

/// What makes one of the family's scenarios over a store.
type Of = fn(Store) -> Scenario;

/// The seeds of each scenario over SQLite.
const SQLITE_SEEDS: u64 = 16;

/// The seeds of each scenario over Postgres, a database of its own for each.
#[cfg(feature = "postgres")]
const POSTGRES_SEEDS: u64 = 8;

/// Runs the scenario `of` makes over SQLite for [`SQLITE_SEEDS`], and with the feature
/// `postgres` over Postgres for `POSTGRES_SEEDS`; fails at the first seed that fails.
fn over_every_store(of: Of) {
    of(Store::Sqlite).run_seeds(SQLITE_SEEDS);
    #[cfg(feature = "postgres")]
    of(Store::Postgres).run_seeds(POSTGRES_SEEDS);
}

/// The restart over the store; see [`world::restart`].
#[test]
fn a_restart_over_the_store_keeps_every_host_binding_and_lease() {
    over_every_store(world::restart);
}

/// A stop of the server in a burst of hellos that consume join tokens; see
/// [`world::hellos`].
#[test]
fn a_hello_that_consumes_a_join_token_commits_the_binding_with_it() {
    over_every_store(world::hellos);
}

/// A second server on the store of a server that runs; see [`world::second`].
#[test]
fn a_second_server_over_the_same_store_is_refused_while_the_first_holds_the_lock() {
    over_every_store(world::second);
}

/// A crash of the server at the scaler's acquire; see [`world::scaler`].
#[test]
fn the_scaler_records_survive_a_restart_and_launch_nothing_twice() {
    over_every_store(world::scaler);
}

/// Asserts that each scenario and seed of a fixed list gives the same trace hash over
/// SQLite and over Postgres, and that the restart's runs over Postgres match the golden
/// lines of family `store`, which `tests/replay.rs` pins over SQLite: one file pins both
/// stores. A difference names the scenario, the seed and both hashes. Catches a trace line
/// that holds something a store generates (a connection string, a database name, a row id
/// or a revision that differs between the backends), and a behaviour of the server that
/// depends on the store (a commit that is not atomic on one, a read of a stale row).
#[cfg(feature = "postgres")]
#[test]
fn the_sqlite_and_postgres_stores_give_the_same_trace_hash() {
    use js_sim::golden::GoldenFile;

    let pairs: [(&str, Of, u64); 5] = [
        ("the restart", world::restart, 0),
        ("the restart", world::restart, 1),
        ("the burst of hellos", world::hellos, 0),
        ("the second server", world::second, 0),
        ("the scaler's crash", world::scaler, 0),
    ];
    let hash = |of: Of, store, seed: u64| of(store).run_seed_range(seed..seed + 1).runs[0].hash;
    let differ = pairs.iter().filter_map(|&(name, of, seed)| {
        let (sqlite, postgres) = (
            hash(of, Store::Sqlite, seed),
            hash(of, Store::Postgres, seed),
        );
        let differs = sqlite != postgres;
        differs.then(|| format!("{name}, seed {seed}: sqlite {sqlite}, postgres {postgres}"))
    });
    let differ: Vec<String> = differ.collect();
    let golden = GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        world::FAMILY,
        "cargo test -p js-sim-cell --test replay -- --ignored regenerate_golden_hashes",
    );
    let runs = world::restart(Store::Postgres)
        .run_seeds(world::GOLDEN_SEEDS)
        .runs;
    let pinned = golden.check(&world::golden_names(&runs)).err();
    let found: Vec<String> = differ.into_iter().chain(pinned).collect();
    if !found.is_empty() {
        panic!("the stores disagree:\n{}", found.join("\n"));
    }
}
