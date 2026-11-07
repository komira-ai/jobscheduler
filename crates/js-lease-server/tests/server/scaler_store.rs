//! The scaler's records and the hosts the registry refused to remove, kept in the state store
//! (`--store`; SQLite with the dev flag here, and Postgres in `store_postgres.rs`), and the
//! first start's import of the four files a server without the setting keeps beside the lease
//! file. Time is the test's clock and the ticks are stepped; nothing sleeps and no network is
//! touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_lease_server::{RecordsFile, UnremovedFile};
use js_scaler::Records;
use js_state::{Revision, Store};
use serde_json::{Value, json};

use crate::scaler_support::{
    RECORDS_KEY, UNREMOVED_KEY, account, open_store, peek, put_in_store, records_path, start,
    start_refused, submit, text_in, unremoved_path,
};
use crate::support::{Harness, Kept, Scratch, T0, base_args, logged_with};

/// The marker of the import.
const IMPORTED: &str = "meta/server-state-imported";

/// The host the imported files bind, taint and cordon.
const HOST: &str = "js-imported";

/// When the imported cordon lapses.
const CORDON_UNTIL: u64 = T0 + 3_600_000;

/// The file `suffix` names beside the lease file of `scratch`.
fn beside(scratch: &Scratch, suffix: &str) -> PathBuf {
    let mut path = scratch.state().into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Writes `text` owner-only at the file `suffix` beside the lease file of `scratch`.
fn write_beside(scratch: &Scratch, suffix: &str, text: &str) -> PathBuf {
    let path = beside(scratch, suffix);
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// A digest of 64 hex digits, all `digit`.
fn digest(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// The bindings file: [`HOST`] bound to a credential and an instance, tainted and cordoned
/// (the reason is `reason`).
fn agents_file(reason: &str) -> String {
    format!(
        "version = 1\n\n[[host]]\nid = \"{HOST}\"\ncredential = \"{}\"\ninstance_provider = \"burst\"\ninstance_id = \"{HOST}\"\ntaint_reason = \"{reason}\"\ntaint_check = \"tmp-files\"\ncordon_until_ms = {CORDON_UNTIL}\n",
        digest('c')
    )
}

/// The join tokens file: an unused token (digest `a`) and one used by [`HOST`] (digest `b`).
fn joins_file() -> String {
    format!(
        "version = 1\n\n[[join]]\nhash = \"{}\"\nprovider = \"burst\"\ninstance_id = \"js-unused\"\nexpires_at_ms = {}\n\n[[join]]\nhash = \"{}\"\nprovider = \"burst\"\ninstance_id = \"{HOST}\"\nexpires_at_ms = {}\nused_by = \"{HOST}\"\ncredential_expires_at_ms = {}\n",
        digest('a'),
        T0 + 600_000,
        digest('b'),
        T0 + 600_000,
        T0 + 86_400_000
    )
}

/// The four files beside the lease file of `scratch`: the bindings, the join tokens, the
/// records (none of them in flight) and one host to remove. Returns the records' text.
fn the_four_files(scratch: &Scratch) -> String {
    write_beside(scratch, ".agents", &agents_file("residue"));
    write_beside(scratch, ".joins", &joins_file());
    RecordsFile::new(records_path(scratch))
        .save(&Records::default())
        .unwrap();
    let hosts = std::collections::BTreeSet::from(["js-ghost".to_owned()]);
    UnremovedFile::new(unremoved_path(scratch))
        .save(&hosts)
        .unwrap();
    fs::read_to_string(records_path(scratch)).unwrap()
}

/// Every key `store` holds, with its value.
fn everything(store: &dyn Store) -> Vec<(String, Vec<u8>)> {
    let rows = store.scan_prefix("").unwrap().into_iter();
    rows.map(|(key, row)| (key, row.value)).collect()
}

/// The value at `key` of `store`, as JSON.
fn json_at(store: &dyn Store, key: &str) -> Value {
    let row = store.get(key).unwrap().unwrap();
    serde_json::from_slice(&row.value).unwrap()
}

/// How far `store` has been written: the cursor after every change in its feed.
fn revision(store: &dyn Store) -> Revision {
    store.changes(Revision::ZERO, 100_000).unwrap().cursor
}

/// Asserts that the scaler's records are committed to the store before the provider is
/// called: the provider reads `acquires/scaler/records` through a handle of its own at the
/// launch, and the record of the machine it is asked to launch is already there, with the
/// acquire the launch carries. No records file is written. It catches a save that moves after
/// the provider call (a crash between them would forget a machine that exists) and a store
/// mode that keeps the records in memory or in the file. (That a failed commit stops the
/// step before any call, and counts one save error, is the shell's rule for every failed
/// save: tests/scaler/duties.rs `a_failed_save_carries_out_no_later_action`, and the unit
/// test `a_commit_that_fails_is_a_failed_save` of the store's writer.)
#[tokio::test]
async fn scaler_records_are_committed_before_the_provider_is_called() {
    let scratch = Scratch::new("scaler_store_order");
    let account = account(&scratch);
    let peek = peek(&scratch);
    let source = text_in(&scratch, "live", "initialise", Kept::Store);
    let h = start(scratch, &source).await;
    peek.watch(h.running.store().unwrap());
    h.ticks.step().await;
    let queued = submit(&h, "j1").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    h.ticks.step().await;
    assert_eq!(account.launched(), 1);

    let seen = peek.seen();
    assert_eq!(seen.len(), 1, "one launch, one look");
    let at_launch = Records::decode(seen[0].as_deref().expect("records at the launch")).unwrap();
    assert_eq!(at_launch.instances.len(), 1);
    let name = at_launch.instances[0].name.clone();
    let script = account.launches()[0].user_data().unwrap().to_owned();
    assert!(script.contains(&format!("burst:{name}")), "{script}");
    assert!(at_launch.instances[0].acquire.is_some());
    assert!(!records_path(&h.scratch).exists(), "no records file");
}

/// Asserts that the first start over an empty store imports the four files once: the store
/// then holds exactly the host's binding (credential digest, instance, taint, cordon), the
/// two join-token rows (the used one with its host and credential time), the records file's
/// text, the host to remove, and the marker. A restart, with the bindings file edited since,
/// changes nothing in the store (its revision is the same) and reads the binding from the
/// store, not from the file. The files are left as they were. It catches an import that
/// misses a file, one that runs again at the next start or reads an edited file, and a
/// server that deletes or rewrites the files.
#[tokio::test]
async fn first_boot_on_an_empty_store_imports_the_four_files_once() {
    let scratch = Scratch::new("scaler_store_import");
    account(&scratch);
    let records = the_four_files(&scratch);
    let source = text_in(&scratch, "live", "initialise", Kept::Store);
    let h = start(scratch, &source).await;
    let store = h.running.store().unwrap();

    let keys: Vec<String> = everything(&*store)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let binding = format!("hosts/binding/{HOST}");
    let unused = format!("hosts/join/{}", digest('a'));
    let used = format!("hosts/join/{}", digest('b'));
    let expected = [
        RECORDS_KEY,
        UNREMOVED_KEY,
        binding.as_str(),
        unused.as_str(),
        used.as_str(),
        IMPORTED,
    ];
    let mut expected: Vec<String> = expected.iter().map(|key| key.to_string()).collect();
    expected.sort();
    assert_eq!(keys, expected);
    assert_eq!(
        json_at(&*store, &binding),
        json!({
            "version": 1,
            "credential": digest('c'),
            "instance_provider": "burst",
            "instance_id": HOST,
            "taint_reason": "residue",
            "taint_check": "tmp-files",
            "cordon_until_ms": CORDON_UNTIL,
        })
    );
    assert_eq!(
        json_at(&*store, &unused),
        json!({
            "version": 1,
            "provider": "burst",
            "instance_id": "js-unused",
            "expires_at_ms": T0 + 600_000,
        })
    );
    assert_eq!(
        json_at(&*store, &used),
        json!({
            "version": 1,
            "provider": "burst",
            "instance_id": HOST,
            "expires_at_ms": T0 + 600_000,
            "used_by": HOST,
            "credential_expires_at_ms": T0 + 86_400_000,
        })
    );
    let stored = store.get(RECORDS_KEY).unwrap().unwrap().value;
    assert_eq!(String::from_utf8(stored).unwrap(), records);
    let stored = store.get(UNREMOVED_KEY).unwrap().unwrap().value;
    assert_eq!(stored, br#"{"version":1,"hosts":["js-ghost"]}"#);
    assert_eq!(json_at(&*store, IMPORTED), json!({ "version": 1 }));
    drop(store);

    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let before = revision(&open_store(&scratch));
    write_beside(&scratch, ".agents", &agents_file("edited"));
    let again = text_in(&scratch, "live", "resume", Kept::Store);
    let h = start(scratch, &again).await;
    let store = h.running.store().unwrap();
    assert_eq!(revision(&*store), before, "the restart wrote nothing");
    assert_eq!(json_at(&*store, &binding)["taint_reason"], "residue");
    let text = fs::read_to_string(beside(&h.scratch, ".agents")).unwrap();
    assert_eq!(
        text,
        agents_file("edited"),
        "the server left the file alone"
    );
    assert_eq!(
        fs::read_to_string(records_path(&h.scratch)).unwrap(),
        records
    );
}

/// Asserts that a damaged file stops the import and the start, and that nothing is written:
/// with a good bindings file beside a file of hosts to remove of another version, the start
/// is refused naming that file, the store holds no key (no marker, no binding), and no lease
/// file is made. It catches an import that is partial, or that sets the marker over a file it
/// could not read (the next start would then never import it).
#[tokio::test]
async fn a_damaged_file_stops_the_import_and_writes_nothing() {
    let scratch = Scratch::new("scaler_store_damaged_file");
    account(&scratch);
    write_beside(&scratch, ".agents", &agents_file("residue"));
    let bad = write_beside(&scratch, ".scaler.unremoved", r#"{"version":2,"hosts":[]}"#);
    let source = text_in(&scratch, "live", "initialise", Kept::Store);
    let said = start_refused(&scratch, &source).await;
    assert!(said.starts_with("--state-file: "), "{said}");
    assert!(said.contains(&bad.display().to_string()), "{said}");
    assert!(said.contains("unremoved-hosts file"), "{said}");
    assert_eq!(everything(&open_store(&scratch)), Vec::new());
    assert!(!scratch.state().exists(), "no lease file was made");
}

/// Asserts that a live pool that is not at its first start, over an empty store with no
/// records file beside the lease file, is refused before anything is imported, as it is
/// without a store: the refusal names the records file and the remedy, and the store holds
/// no key. It catches an import that sets the marker for a pool whose records are lost, after
/// which the file could no longer be restored and imported.
#[tokio::test]
async fn a_live_pool_without_its_records_file_is_refused_before_the_import() {
    let scratch = Scratch::new("scaler_store_no_records");
    account(&scratch);
    write_beside(&scratch, ".agents", &agents_file("residue"));
    let source = text_in(&scratch, "live", "resume", Kept::Store);
    let said = start_refused(&scratch, &source).await;
    let path = records_path(&scratch).display().to_string();
    assert!(
        said.starts_with(&format!("--state-file: {path} is missing")),
        "{said}"
    );
    assert!(said.contains("--boot initialise"), "{said}");
    assert_eq!(everything(&open_store(&scratch)), Vec::new());
}

/// Asserts that a fresh install, with no files, gets the marker at its first start and
/// nothing else, and that files that appear later are not read: a restart with a bindings
/// file and a file of hosts to remove beside the lease file leaves the store as it was, and
/// names each of the two files once in an info line. It catches an import that waits for a
/// file to set the marker (a later stale file would then be imported over newer state) and a
/// stale file read or ignored without a word.
#[tokio::test]
async fn import_with_no_files_still_sets_the_marker() {
    // The info lines are read from the process-wide subscriber: install it, and make every
    // log call ask it again, before the server logs.
    crate::support::quiet_logs();
    tracing::callsite::rebuild_interest_cache();
    let scratch = Scratch::new("scaler_store_fresh");
    let args = Kept::Store.args(&scratch, base_args(&scratch));
    let h = Harness::with(scratch, args.clone(), None).await;
    let store = h.running.store().unwrap();
    let only = vec![(IMPORTED.to_owned(), br#"{"version":1}"#.to_vec())];
    assert_eq!(everything(&*store), only);
    drop(store);

    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let agents = write_beside(&scratch, ".agents", &agents_file("residue"));
    let unremoved = write_beside(
        &scratch,
        ".scaler.unremoved",
        r#"{"version":1,"hosts":["js-ghost"]}"#,
    );
    let h = Harness::with(scratch, args, None).await;
    let store = h.running.store().unwrap();
    assert_eq!(everything(&*store), only);
    for file in [agents, unremoved] {
        let named = logged_with(&file.display().to_string());
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("INFO"), "{named:?}");
        assert!(
            named[0].contains(
                "this file is not read: the store holds the server's state since the import"
            ),
            "{named:?}"
        );
    }
}

/// Asserts that a store that already holds server state without the marker (as one written
/// by a build that kept host bindings in the store but had no import) is not imported over:
/// with a bindings file beside the lease file, the start writes the marker and no binding. It
/// catches an import that runs for a store that is not empty, which would put stale files
/// over newer state.
#[tokio::test]
async fn an_import_does_not_run_over_a_store_that_holds_state() {
    let scratch = Scratch::new("scaler_store_held");
    let held = r#"{"version":1,"hosts":[]}"#;
    put_in_store(&scratch, UNREMOVED_KEY, held);
    write_beside(&scratch, ".agents", &agents_file("residue"));
    let args = Kept::Store.args(&scratch, base_args(&scratch));
    let h = Harness::with(scratch, args, None).await;
    let store = h.running.store().unwrap();
    let expected = vec![
        (UNREMOVED_KEY.to_owned(), held.as_bytes().to_vec()),
        (IMPORTED.to_owned(), br#"{"version":1}"#.to_vec()),
    ];
    assert_eq!(everything(&*store), expected);
}

/// Asserts that a start whose store holds scaler records, or hosts to remove, that cannot be
/// read as written is refused naming `server.store`, with the key and what is wrong, as a
/// damaged records or unremoved file is refused naming `--state-file`. It catches a damaged
/// key read as empty, which would launch again for machines in flight or drop the retries.
#[tokio::test]
async fn a_damaged_scaler_key_stops_the_start() {
    let cases = [
        (
            RECORDS_KEY,
            "{}",
            "server.store: acquires/scaler/records does not hold valid scaler records: ",
        ),
        (
            UNREMOVED_KEY,
            r#"{"version":1,"hosts":"#,
            "server.store: acquires/scaler/unremoved does not hold a valid list of hosts: ",
        ),
    ];
    for (n, (key, value, refusal)) in cases.into_iter().enumerate() {
        let scratch = Scratch::new(&format!("scaler_store_damaged_key_{n}"));
        account(&scratch);
        put_in_store(&scratch, IMPORTED, r#"{"version":1}"#);
        put_in_store(&scratch, key, value);
        let source = text_in(&scratch, "live", "initialise", Kept::Store);
        let said = start_refused(&scratch, &source).await;
        assert!(said.starts_with(refusal), "{said}");
    }
}
