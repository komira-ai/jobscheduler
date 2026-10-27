//! The identity of the state store's first migration, which no server is needed to check: the
//! database records a migration by its number, name and kind, and the file's bytes are what an
//! existing database ran. A change to any of them is a change to what a deployed database has,
//! so it is refused here, not found out on a production schema.
#![cfg(feature = "postgres")]

use js_state::postgres::schema::{self, Kind};
use sha2::{Digest, Sha256};

/// The bytes of `migrations/0001_state.sql` as a database that already applied it ran them.
const SQL: &str = include_str!("../src/postgres/migrations/0001_state.sql");

/// The SHA-256 of `migrations/0001_state.sql`, written out so a changed byte fails here.
const SHA256: &str = "be91e62432579bb5436d97c3e2a8caeedea18a3eca5377a0771f768111b50f79";

/// Asserts that the migration is number 1, named `state`, an expand step, and that its file is
/// byte for byte the one a database that already applied it ran. Catches: an edit to the file
/// (even whitespace), a renumbering and a rename.
#[test]
fn the_migration_is_what_an_existing_database_recorded() {
    let hex: String = Sha256::digest(SQL.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(hex, SHA256, "the migration file changed");
    let set = schema::embedded();
    let migration = &set[0];
    assert_eq!(migration.version(), 1);
    assert_eq!(migration.name(), "state");
    assert_eq!(migration.kind(), Kind::Expand);
}

/// Asserts that the embedded set is exactly the one migration, numbered 1, so a second one
/// added to it is a deliberate change to this test.
#[test]
fn the_embedded_set_is_exactly_migration_one() {
    let set = schema::embedded();
    let named: Vec<(u32, &str)> = set.iter().map(|m| (m.version(), m.name())).collect();
    assert_eq!(named, [(1, "state")]);
    assert_eq!(schema::latest(), 1);
}
