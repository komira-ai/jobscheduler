//! The first start over a store: the files a server without the setting keeps beside the
//! state file, imported once.
//!
//! Right after the store is opened, and before anything else is read from the store, a
//! start looks for the marker `meta/server-state-imported`. Without it, and with nothing in
//! the store under `hosts/` or `acquires/`, it reads `<state file>.agents`, `.joins`, `.scaler`
//! and `.scaler.unremoved` with the readers a start without the setting uses (a missing file is
//! empty, as it is there, and a file those readers refuse, of a bad version, damaged, or one
//! others can use, stops the start), and writes every binding, every join-token row, the
//! records file's text, the hosts to remove and the marker in ONE transaction. A file that is
//! refused, or a commit that fails, writes nothing: the start is refused and the next start
//! imports again. Several servers starting at once over one store race to the import; the
//! commit of each is `Cond::Absent` on every key, so one wins. A loser's commit meets a key
//! the winner wrote: it reads the marker again and, finding it, starts over the winner's rows
//! as a start that found the marker does (the files are named stale); it is refused only
//! when the marker is still absent (a key written by something other than an import). The
//! marker is written even when there are no files (a fresh install), so a file that appears
//! later is never read. A binding's cordon end, which the file holds on the
//! server's clock, is written on the store's ([`crate::agent_state`]), by the store's clock
//! less the server's read once before the files: a store that cannot read its clock imports
//! nothing.
//!
//! With the marker present the files are not read: each one still there is named once at
//! info level and ignored. A store that already holds server state without the marker (one a
//! build that kept host bindings in the store but had no import wrote) is not imported over:
//! only the marker is written. A lease file of an earlier release is not read.
//!
//! The server never deletes or changes the files. They hold what the server held at the
//! import, and go stale from its first write after it: a rollback to a build without the
//! store setting finds them as they were at the cutover.

use std::path::Path;

use js_state::{Cond, Store, StoreError, Txn};
use tracing::info;

use crate::agent_state::Skew;
use crate::agents::state_path;
use crate::clock::SharedClock;
use crate::error::StartupError;
use crate::host_keys;
use crate::scaler::kept;
use crate::scaler::{RecordsFile, UnremovedFile};
use crate::{agent_state, join_tokens};

/// The key that says the files were imported.
const IMPORTED: &str = "meta/server-state-imported";

/// What the marker holds.
const MARKER: &[u8] = b"{\"version\":1}";

/// The prefixes of the server state the import writes: a store with a key under one of them
/// already holds that state.
const HELD: [&str; 2] = ["hosts/", "acquires/"];

/// The files beside the state file the import reads.
const FILES: [&str; 4] = [".agents", ".joins", ".scaler", ".scaler.unremoved"];

/// Imports the files beside the state file at `state_file` into `store` once (see the module
/// docs), the bindings' cordon ends moved from `clock` (the server's) onto the store's.
/// `records_needed`: a live elastic pool not at its first start, whose missing records file is
/// refused. `refuse` makes a refusal naming the store's setting.
///
/// # Errors
///
/// A file's refusal, naming the file; the store could not be read, or the import could not be
/// committed, by `refuse`. Nothing was written.
pub(super) fn run(
    store: &dyn Store,
    (state_file, clock): (&Path, &SharedClock),
    records_needed: bool,
    refuse: &dyn Fn(String) -> StartupError,
) -> Result<(), StartupError> {
    let unread = |err: StoreError| refuse(format!("the store could not be read: {err}"));
    if store.get(IMPORTED).map_err(unread)?.is_some() {
        stale(state_file);
        return Ok(());
    }
    let held: Vec<_> = HELD
        .iter()
        .map(|prefix| store.scan_prefix(prefix))
        .collect();
    let held = held
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(unread)?;
    let writes = match held.iter().all(Vec::is_empty) {
        true => {
            let skew = Skew::read(store, clock);
            let skew = skew.map_err(|err| refuse(format!("the store could not be read: {err}")))?;
            files(state_file, records_needed, skew)?
        }
        false => {
            info!(
                "the store already holds server state without the import marker: the files beside the state file are not imported"
            );
            Vec::new()
        }
    };
    let keys = writes.len();
    let txn = writes.into_iter().fold(Txn::new(), |txn, (key, value)| {
        txn.put(key, value, Cond::Absent)
    });
    let txn = txn.put(IMPORTED, MARKER, Cond::Absent);
    let failed = |err: String| refuse(format!("the import of the state files failed: {err}"));
    let answer = store.txn(txn);
    if matches!(answer, Ok(Err(_))) {
        // Another writer wrote a key of this import since the reads: another server's import
        // won the race. With its marker set this start goes on over what it wrote; with the
        // marker still absent the refusal below names the key.
        if store
            .get(IMPORTED)
            .map_err(|err| failed(err.to_string()))?
            .is_some()
        {
            info!(
                "another server imported the files beside the state file first: this start reads the store"
            );
            stale(state_file);
            return Ok(());
        }
    }
    host_keys::committed(answer).map_err(|err| failed(err.to_string()))?;
    info!(
        keys,
        "the import marker is set: the files beside the state file are not read again; the store holds the server's state"
    );
    Ok(())
}

/// The keys of the four files beside `state_file`, read with their own readers.
fn files(
    state_file: &Path,
    records_needed: bool,
    skew: Skew,
) -> Result<Vec<(String, Vec<u8>)>, StartupError> {
    let at = |suffix: &str| state_path(state_file, suffix);
    let mut writes = agent_state::import(&at(".agents"), skew)?;
    writes.extend(join_tokens::import(&at(".joins"))?);
    let records = RecordsFile::new(at(".scaler"));
    let unremoved = UnremovedFile::new(at(".scaler.unremoved"));
    writes.extend(kept::import((&records, &unremoved), records_needed)?);
    Ok(writes)
}

/// Names, once, each file beside `state_file` that the import made stale.
fn stale(state_file: &Path) {
    let present = FILES.iter().map(|suffix| state_path(state_file, suffix));
    for file in present.filter(|file| file.exists()) {
        info!(
            file = %file.display(),
            "this file is not read: the store holds the server's state since the import"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    use js_jobs::interfere::{INJECTED, Interfere};
    use js_state::ManualClock;
    use js_state::fake::{Fake, Flaw};
    use js_testkit::dir::TestDir;

    use super::*;
    use crate::config::StoreNames;
    use crate::store::ServerStore;

    fn store() -> Interfere<Fake> {
        let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Interfere::new(fake)
    }

    /// A process-wide subscriber that takes every line, so the info lines of the import run.
    fn quiet_logs() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(std::io::sink)
                .try_init();
        });
        tracing::callsite::rebuild_interest_cache();
    }

    fn refuse(reason: String) -> StartupError {
        StartupError::new("--store", reason)
    }

    /// A state file's path in a scratch directory of the test's own.
    fn state(name: &str) -> (TestDir, PathBuf) {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let path = dir.path().join("state");
        (dir, path)
    }

    /// Writes `text` owner-only at the file `suffix` beside `state`.
    fn beside(state: &Path, suffix: &str, text: &str) {
        let path = state_path(state, suffix);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Asserts that a store that cannot be read refuses the start through `refuse`, with the
    /// store's reason, at the marker's read and at the scan of what the store holds, and writes
    /// nothing. It catches a failed read taken for an empty store, which would import stale
    /// files over the state the store holds.
    #[test]
    fn a_store_that_cannot_be_read_refuses_the_start() {
        let (_dir, path) = state("import_unread");
        for reads in [0, 1] {
            let store = store();
            store.fail_read_after(reads);
            let refused = run(
                &store,
                (&path, &crate::agent_state::epoch_clock()),
                false,
                &refuse,
            )
            .unwrap_err()
            .to_string();
            assert!(
                refused.starts_with("--store: the store could not be read: "),
                "{refused}"
            );
            assert!(refused.contains(INJECTED), "{refused}");
            assert_eq!(store.commit_stats().txns, 0);
        }
    }

    /// Asserts that an import whose commit fails refuses the start with the store's reason and
    /// leaves no marker, and that the next start imports: the marker and the binding are there.
    /// It catches a marker written apart from the keys, which a crash between them would leave
    /// as an import that never happened.
    #[test]
    fn a_failed_commit_leaves_no_marker_and_the_next_start_imports() {
        let (_dir, path) = state("import_commit");
        let credential = "0123456789abcdef".repeat(4);
        beside(
            &path,
            ".agents",
            &format!("version = 1\n[[host]]\nid = \"a\"\ncredential = \"{credential}\"\n"),
        );
        let store = store();
        store.fail_next();
        let refused = run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            false,
            &refuse,
        )
        .unwrap_err()
        .to_string();
        assert!(
            refused.starts_with("--store: the import of the state files failed: "),
            "{refused}"
        );
        assert!(refused.contains(INJECTED), "{refused}");
        assert!(store.get(IMPORTED).unwrap().is_none());
        run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            false,
            &refuse,
        )
        .unwrap();
        assert_eq!(store.get(IMPORTED).unwrap().unwrap().value, MARKER);
        assert!(store.get("hosts/binding/a").unwrap().is_some());
    }

    /// Asserts that a binding's cordon end, which the file holds on the server's clock, is
    /// imported on the store's: with the server's clock at 5 000 s and the store's at its
    /// origin, a cordon a minute from the server's now is stored a minute from the store's. A
    /// store that cannot read its clock refuses the start through `refuse` and writes nothing.
    /// It catches an import that copies the server's time into the store, where every server
    /// reads it as the store's.
    #[test]
    fn a_cordon_end_is_imported_on_the_store_clock() {
        let (_dir, path) = state("import_cordon_clock");
        let credential = "0123456789abcdef".repeat(4);
        let text = format!(
            "version = 1\n[[host]]\nid = \"a\"\ncredential = \"{credential}\"\ncordon_until_ms = 5060000\n"
        );
        beside(&path, ".agents", &text);
        let server = js_types::ManualClock::new(js_types::Timestamp::from_millis(5_000_000));
        let server = SharedClock::new(server);
        let store = store();
        store.fail_clock(true);
        let refused = run(&store, (&path, &server), false, &refuse).unwrap_err();
        let refused = refused.to_string();
        assert!(
            refused.starts_with("--store: the store could not be read: "),
            "{refused}"
        );
        assert_eq!(store.commit_stats().txns, 0);

        store.fail_clock(false);
        run(&store, (&path, &server), false, &refuse).unwrap();
        let row = store.get("hosts/binding/a").unwrap().unwrap().value;
        let row: serde_json::Value = serde_json::from_slice(&row).unwrap();
        assert_eq!(row["cordon_until_ms"], 60_000);
    }

    /// The four files beside `state`: one binding, one join token, the records and a host to
    /// remove.
    fn the_four_files(state: &Path) {
        let credential = "0123456789abcdef".repeat(4);
        beside(
            state,
            ".agents",
            &format!("version = 1\n[[host]]\nid = \"a\"\ncredential = \"{credential}\"\n"),
        );
        beside(
            state,
            ".joins",
            &format!(
                "version = 1\n[[join]]\nhash = \"{credential}\"\nprovider = \"p\"\ninstance_id = \"i\"\nexpires_at_ms = 1\n"
            ),
        );
        let records = js_scaler::Records::default().encode().unwrap();
        beside(state, ".scaler", &records);
        beside(state, ".scaler.unremoved", r#"{"version":1,"hosts":["h"]}"#);
    }

    /// Every key of `store`.
    fn keys(store: &dyn Store) -> Vec<String> {
        let rows = store.scan_prefix("").unwrap().into_iter();
        rows.map(|(key, _)| key).collect()
    }

    /// Asserts each way a start meets the import: an empty store imports the four files with
    /// the marker in one commit; a start that finds the marker reads no file (a file edited
    /// since is not read) and commits nothing; a store that holds server state without the
    /// marker gets the marker only. It catches an import that runs twice, one that misses a
    /// file, and one that runs over a store that is not empty.
    #[test]
    fn the_files_are_imported_once_and_never_over_held_state() {
        quiet_logs();
        let (_dir, path) = state("import_once");
        the_four_files(&path);
        let store = store();
        run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            true,
            &refuse,
        )
        .unwrap();
        assert_eq!(store.commit_stats().txns, 1);
        let join = format!("hosts/join/{}", "0123456789abcdef".repeat(4));
        let all = [
            "acquires/scaler/records",
            "acquires/scaler/unremoved",
            "hosts/binding/a",
            join.as_str(),
            IMPORTED,
        ];
        assert_eq!(keys(&store), all);
        beside(&path, ".agents", "not toml");
        run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            true,
            &refuse,
        )
        .unwrap();
        assert_eq!(
            store.commit_stats().txns,
            1,
            "the second start wrote nothing"
        );

        let held = self::store();
        let txn = Txn::new().put("acquires/scaler/unremoved", "{}", Cond::Any);
        held.txn(txn).unwrap().unwrap();
        run(
            &held,
            (&path, &crate::agent_state::epoch_clock()),
            true,
            &refuse,
        )
        .unwrap();
        assert_eq!(keys(&held), ["acquires/scaler/unremoved", IMPORTED]);
    }

    /// Asserts that with no files the marker alone is written, and that a live pool not at
    /// its first start is refused with no records file, naming the file, before anything is
    /// written. It catches an import that waits for a file before it sets the marker, and one
    /// that sets it for a pool whose records are lost.
    #[test]
    fn no_files_set_the_marker_and_missing_records_refuse_a_live_pool() {
        let (_dir, path) = state("import_none");
        let store = store();
        let refused = run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            true,
            &refuse,
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("is missing"), "{refused}");
        assert_eq!(keys(&store), Vec::<String>::new());
        run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            false,
            &refuse,
        )
        .unwrap();
        assert_eq!(keys(&store), [IMPORTED]);
    }

    /// Asserts that a damaged bindings, join tokens or records file stops the import with the
    /// file's own refusal, naming the file, and writes nothing. It catches an import that
    /// reads a damaged file as empty, or writes the files it could read and not the rest.
    #[test]
    fn a_damaged_file_refuses_the_import_and_writes_nothing() {
        let cases = [
            (".agents", "is not a valid agent state file"),
            (".joins", "is not a valid join token file"),
            (".scaler", "is not a valid scaler records file"),
        ];
        for (n, (suffix, says)) in cases.into_iter().enumerate() {
            let (_dir, path) = state(&format!("import_damaged_{n}"));
            the_four_files(&path);
            beside(&path, suffix, "{}");
            let store = store();
            let refused = run(
                &store,
                (&path, &crate::agent_state::epoch_clock()),
                false,
                &refuse,
            )
            .unwrap_err()
            .to_string();
            assert!(refused.starts_with("--state-file: "), "{refused}");
            assert!(refused.contains(says), "{refused}");
            assert_eq!(keys(&store), Vec::<String>::new());
        }
    }

    /// Asserts that of two servers racing the first start over one store, the one whose commit
    /// meets the other's import starts: the other's commit lands between this one's reads and
    /// its commit, and this start reads the marker, writes nothing and answers `Ok`, leaving
    /// the winner's rows as they were. It catches a loser whose conflict refuses its start
    /// (exit 2) though the store holds the import.
    #[test]
    fn the_server_that_loses_the_import_race_starts() {
        quiet_logs();
        let (_dir, path) = state("import_race");
        the_four_files(&path);
        let store = store();
        let other = store.inner();
        let winner = path.clone();
        store.before_next(move || {
            run(
                &*other,
                (&winner, &crate::agent_state::epoch_clock()),
                true,
                &refuse,
            )
            .unwrap();
        });
        run(
            &store,
            (&path, &crate::agent_state::epoch_clock()),
            true,
            &refuse,
        )
        .unwrap();
        let join = format!("hosts/join/{}", "0123456789abcdef".repeat(4));
        let all = [
            "acquires/scaler/records",
            "acquires/scaler/unremoved",
            "hosts/binding/a",
            join.as_str(),
            IMPORTED,
        ];
        assert_eq!(keys(&store), all);
        assert_eq!(store.get(IMPORTED).unwrap().unwrap().value, MARKER);
        assert_eq!(
            store.commit_stats().txns,
            1,
            "only the winner's import committed"
        );
    }

    /// Asserts that an import whose commit meets a key written by something other than an
    /// import (the marker is still absent) refuses the start, naming the key, and that one
    /// whose read of the marker then fails refuses it with the store's reason. It catches a
    /// loser that starts without the marker, over a store no import filled, and a failed read
    /// taken for an absent marker or a present one.
    #[test]
    fn a_conflict_without_the_marker_refuses_the_start() {
        let (_dir, path) = state("import_conflict");
        the_four_files(&path);
        for fail_read in [false, true] {
            let store = store();
            let other = store.inner();
            let also = Arc::clone(&other);
            store.before_next(move || {
                let txn = Txn::new().put("hosts/binding/a", "{}", Cond::Any);
                also.txn(txn).unwrap().unwrap();
            });
            if fail_read {
                // The marker's read and the two scans pass (the clock's read is not counted);
                // the read of the marker after the conflict fails.
                store.fail_read_after(3);
            }
            let refused = run(
                &store,
                (&path, &crate::agent_state::epoch_clock()),
                true,
                &refuse,
            )
            .unwrap_err()
            .to_string();
            assert!(
                refused.starts_with("--store: the import of the state files failed: "),
                "{refused}"
            );
            match fail_read {
                false => assert!(refused.contains("hosts/binding/a"), "{refused}"),
                true => assert!(refused.contains(INJECTED), "{refused}"),
            }
            assert!(other.get(IMPORTED).unwrap().is_none());
        }
    }

    /// Asserts that the server's own import names its store setting when the store fails:
    /// a read that fails and a commit that fails are refusals of `--store`. It catches a
    /// refusal that names no setting, or the state file's flag.
    #[test]
    fn the_server_store_names_its_setting_when_the_import_fails() {
        let (_dir, path) = state("import_named");
        let store = Arc::new(self::store());
        let server = ServerStore {
            store: store.clone(),
            names: StoreNames::Flags,
            locked: false,
        };
        store.fail_read_after(0);
        let refused = server
            .import((&path, &crate::agent_state::epoch_clock()), false)
            .unwrap_err()
            .to_string();
        assert!(
            refused.starts_with("--store: the store could not be read: "),
            "{refused}"
        );
        store.fail_next();
        let refused = server
            .import((&path, &crate::agent_state::epoch_clock()), false)
            .unwrap_err()
            .to_string();
        assert!(
            refused.starts_with("--store: the import of the state files failed: "),
            "{refused}"
        );
    }
}
