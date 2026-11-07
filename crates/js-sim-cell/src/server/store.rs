//! Where the cell's server keeps its state: the store `--store` names (`server.store` for a
//! server read from a configuration file), as the server documents the setting. The file
//! holds one line, `sqlite:<absolute path>` or `postgres://...`; a SQLite store is accepted
//! with `--allow-sqlite-store` (`server.allow_sqlite_store = true`) only.
//!
//! [`Store::Sqlite`], the default for every scenario, is a database file in the server's
//! directory ([`SQLITE_FILE`]). [`Store::Postgres`], with the crate's `postgres` feature, is
//! a database of its own on the test server for each server node, made with
//! `js_state::postgres::test_database()` when the node starts (the state schema applied) and
//! dropped with the node, so with the cell of one run of a scenario.
//!
//! The server opens its store from the file and nothing else: the cell gives it no store
//! of its own. The server opens a SQLite store on the system's clock, which it takes from
//! no seam; it writes no key with a time to live, so no decision reads that clock. The cell
//! itself opens the store only to read keys while the server is down
//! ([`super::ServerNode::stored`]), on the cell's clock.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_lease::ManualClock;
#[cfg(feature = "postgres")]
use js_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use js_state::sqlite::{Options, SqliteStore};

use super::StoreClock;

/// The SQLite store's file, in the server's directory.
pub const SQLITE_FILE: &str = "state.db";

/// The file that holds the store's one line, in the server's directory (`--store`).
pub const STORE_LINE_FILE: &str = "store";

/// Which store the cell's server keeps its state in ([`super::ServerSpec::store`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Store {
    /// A SQLite database file in the server's directory, accepted with
    /// `--allow-sqlite-store`.
    #[default]
    Sqlite,
    /// A Postgres database of its own on the test server for each server node, dropped
    /// with it (feature `postgres`).
    #[cfg(feature = "postgres")]
    Postgres,
}

impl Store {
    /// A stable name: `sqlite` or `postgres`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Store::Sqlite => "sqlite",
            #[cfg(feature = "postgres")]
            Store::Postgres => "postgres",
        }
    }
}

/// The store of one server node: kept across a restart, and named by a second server
/// started over it.
#[derive(Clone)]
pub(crate) enum StoreAt {
    /// The SQLite file at this path.
    Sqlite(PathBuf),
    /// The node's own Postgres database, dropped with the last handle on it.
    #[cfg(feature = "postgres")]
    Postgres(Arc<TestDatabase>),
}

impl StoreAt {
    /// A new store of the kind `store` for a server whose files are in `dir`.
    ///
    /// # Panics
    ///
    /// For [`Store::Postgres`], when the test server is not named or refuses the database
    /// (`js_state::postgres::test_database`): a Postgres test fails, never skips.
    pub(crate) fn make(store: Store, dir: &Path) -> Self {
        match store {
            Store::Sqlite => StoreAt::Sqlite(dir.join(SQLITE_FILE)),
            #[cfg(feature = "postgres")]
            Store::Postgres => StoreAt::Postgres(Arc::new(js_state::postgres::test_database())),
        }
    }

    /// The line of the store file, as the server documents it.
    pub(crate) fn line(&self) -> String {
        match self {
            StoreAt::Sqlite(file) => format!("sqlite:{}", file.display()),
            #[cfg(feature = "postgres")]
            StoreAt::Postgres(database) => database.url(),
        }
    }

    /// Whether the store needs the SQLite gate (`--allow-sqlite-store`).
    pub(crate) fn is_sqlite(&self) -> bool {
        match self {
            StoreAt::Sqlite(_) => true,
            #[cfg(feature = "postgres")]
            StoreAt::Postgres(_) => false,
        }
    }

    /// The name of the Postgres database, for a Postgres store.
    #[cfg(feature = "postgres")]
    pub(crate) fn database(&self) -> Option<&str> {
        match self {
            StoreAt::Sqlite(_) => None,
            StoreAt::Postgres(database) => Some(database.name()),
        }
    }

    /// Every key under `prefix` with its value, read with a store opened for the read and
    /// closed before this returns; a SQLite store is opened on `clock`. Only while no
    /// server holds the store: a running server holds the SQLite file's lock.
    ///
    /// # Errors
    ///
    /// The store could not be opened or read, as text.
    pub(crate) fn scan(
        &self,
        prefix: &str,
        clock: &ManualClock,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        match self {
            StoreAt::Sqlite(file) => {
                let clock = Arc::new(StoreClock(clock.clone()));
                let opened = SqliteStore::open(file, clock, Options::default());
                let store = opened.map_err(|err| err.to_string())?;
                rows(&store, prefix)
            }
            #[cfg(feature = "postgres")]
            StoreAt::Postgres(database) => {
                let _ = clock;
                let config = PostgresConfig::parse(&database.url()).map_err(|e| e.to_string())?;
                let store = PostgresStore::open(&config).map_err(|err| err.to_string())?;
                rows(&store, prefix)
            }
        }
    }
}

/// Every key of `store` under `prefix`, in key order, with its value.
fn rows(store: &dyn js_state::Store, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let scanned = store.scan_prefix(prefix).map_err(|err| err.to_string())?;
    let rows = scanned.into_iter().map(|(key, row)| (key, row.value));
    Ok(rows.collect())
}
