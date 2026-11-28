//! The host bindings kept in the state store (`--store`; SQLite with the dev flag here, and one
//! restart over Postgres in `store_postgres.rs`): what a restart reads back, that nothing is
//! written beside the state file, that a stored value that cannot be read as written refuses the
//! start, and that the rows and the file of the one-time join tokens the previous release kept
//! are left exactly as they were.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_lease_server::channel_ticker;
use js_registry::Credential;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Cond, Store, SystemClock, Txn};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::agents_support::{
    agent_args, entry, hello_as, hosts, post, restarted, session, start_agents_in,
};
use crate::config_support::clock;
use crate::store_support::sqlite_db;
use crate::support::{Harness, Kept, Scratch, T0, config, kept_bindings, request, start};

/// The file `suffix` names beside `h`'s state file.
fn beside(h: &Harness, suffix: &str) -> PathBuf {
    let mut path = h.scratch.state().into_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// The host `host`'s own reading of itself, presenting its certificate.
async fn own(h: &Harness, host: &str) -> Value {
    let read = request(h.addr(), "GET", &format!("/v1/hosts/{host}"), None, None).await;
    assert_eq!(read.status, 200, "{}", read.body);
    read.json()
}

/// Asserts that a restart over the store reads back each host's binding, taint and cordon: the
/// taint is still shown, the cordon is still in force with its end, and on the injected clock
/// it lapses at that end; each host's own hello, with its certificate, is admitted again. No
/// `.agents` or `.joins` file is written, before or after. It catches a binding, a taint or a
/// cordon that lives only in memory or only in a file, and a store mode that writes the files
/// as well.
#[tokio::test]
async fn bindings_taints_and_cordons_survive_a_restart_from_the_store() {
    let first = start_agents_in("hosts_store_bindings", Kept::Store).await;
    let tainted = session(&hello_as(&first, "burst-1").await, "burst-1");
    let mut beat = tainted.beat();
    beat["taint"] = json!({ "reason": "residue", "check": "tmp" });
    let reply = post(&first, "/v1/agent/heartbeat", None, &beat).await;
    assert_eq!(reply.json()["taint_ack"], true, "{}", reply.body);
    let admitted = hello_as(&first, "burst-2").await;
    assert_eq!(admitted.status, 200, "{}", admitted.body);
    let lease = json!({ "lease_secs": 120 });
    let cordoned = post(&first, "/v1/hosts/burst-2:cordon", None, &lease).await;
    assert_eq!(cordoned.status, 200, "{}", cordoned.body);
    let kept = kept_bindings(&first);
    assert!(kept.contains("\"taint_reason\":\"residue\""), "{kept}");
    assert!(
        kept.contains(&format!("\"cordon_until_ms\":{}", T0 + 120_000)),
        "{kept}"
    );

    let h = restarted(first).await;
    assert_eq!(Kept::of(&h), Kept::Store);
    let listing = hosts(&h).await;
    assert_eq!(entry(&listing, "burst-1")["taint"]["reason"], "residue");
    let read = own(&h, "burst-2").await;
    assert_eq!(read["cordon"]["expires_at_ms"], T0 + 120_000);
    h.clock.advance(Duration::from_secs(119));
    assert!(own(&h, "burst-2").await.get("cordon").is_some());
    h.clock.advance(Duration::from_secs(1));
    assert!(own(&h, "burst-2").await.get("cordon").is_none());
    let back = hello_as(&h, "burst-2").await;
    assert_eq!(back.status, 200, "{}", back.body);
    let again = hello_as(&h, "burst-1").await;
    assert_eq!(again.status, 200, "{}", again.body);
    for suffix in [".agents", ".joins"] {
        assert!(!beside(&h, suffix).exists(), "{suffix}");
    }
}

/// Asserts that a start whose store holds a binding that cannot be read as written is refused
/// naming `--store`, with the key and what is wrong, as a damaged agent state file is refused
/// naming `--state-file`. It catches a start that reads a damaged store as fewer bindings.
#[tokio::test]
async fn a_stored_host_that_cannot_be_read_refuses_the_start() {
    let scratch = Scratch::new("hosts_store_damaged_binding");
    let key = "hosts/binding/burst-1";
    {
        let clock = Arc::new(SystemClock::default());
        let db = sqlite_db(&scratch);
        let store = SqliteStore::open(&db, clock, Options::default()).unwrap();
        let damaged = Txn::new().put(key, "{}", Cond::Any);
        store.txn(damaged).unwrap().unwrap();
    }
    let args = Kept::Store.args(&scratch, agent_args(&scratch));
    let (_ticks, ticker) = channel_ticker();
    let started = start(config(args).unwrap(), clock(), ticker).await;
    let refused = started.err().unwrap();
    assert_eq!(refused.flag(), "--store");
    let reason = format!("{key} does not hold this key's fields");
    assert_eq!(refused.reason(), reason);
}

/// The one-time join tokens and host credentials the previous release kept under
/// `hosts/join/<digest>`, as that release read them: a frozen copy of its row, and of the
/// checks it made of a row, which this release does not read.
#[derive(Debug, Deserialize)]
struct PreviousJoinRow {
    version: u32,
    provider: String,
    instance_id: String,
    expires_at_ms: u64,
    #[serde(default)]
    used_by: Option<String>,
    #[serde(default)]
    credential_expires_at_ms: Option<u64>,
    #[serde(default)]
    replaces: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// The previous release's reading of the row at `hosts/join/<digest>` holding `bytes`: what it
/// refused the start for.
fn previous_release_reads_join_row(digest: &str, bytes: &[u8]) -> Result<(), String> {
    let key = format!("hosts/join/{digest}");
    let row: PreviousJoinRow = serde_json::from_slice(bytes)
        .map_err(|_| format!("{key} does not hold this key's fields"))?;
    let hex = |text: &str| Credential::from_hex(text).is_some();
    if !hex(digest) {
        return Err(format!(
            "{key}: a token hash is not 64 lower-case hex digits"
        ));
    }
    if row.used_by.is_some() != row.credential_expires_at_ms.is_some() {
        return Err(format!(
            "{key}: a token has only one of used_by and credential_expires_at_ms"
        ));
    }
    if row
        .replaces
        .as_deref()
        .is_some_and(|replaced| !hex(replaced))
    {
        return Err(format!(
            "{key}: a replaced token hash is not 64 lower-case hex digits"
        ));
    }
    if row.replaces.is_some() && row.used_by.is_none() {
        return Err(format!("{key}: a host credential has no host"));
    }
    let _ = (
        row.version,
        row.provider,
        row.instance_id,
        row.expires_at_ms,
        row.extra,
    );
    Ok(())
}

/// A binding row as the previous release read it: a frozen copy of its row.
#[derive(Debug, Deserialize)]
struct PreviousBinding {
    version: u32,
    credential: String,
    #[serde(default)]
    instance_provider: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    taint_reason: Option<String>,
    #[serde(default)]
    taint_run_id: Option<String>,
    #[serde(default)]
    taint_check: Option<String>,
    #[serde(default)]
    cordon_until_ms: Option<u64>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// The previous release's reading of the row at `hosts/binding/<host>` holding `bytes`.
fn previous_release_reads_binding(host: &str, bytes: &[u8]) -> Result<(), String> {
    let key = format!("hosts/binding/{host}");
    let row: PreviousBinding = serde_json::from_slice(bytes)
        .map_err(|_| format!("{key} does not hold this key's fields"))?;
    let _ = (
        row.version,
        row.credential,
        row.instance_provider,
        row.instance_id,
        row.taint_reason,
        row.taint_run_id,
        row.taint_check,
        row.cordon_until_ms,
        row.extra,
    );
    Ok(())
}

/// Every row under `prefix`: key, bytes and version (as text).
fn rows_under(store: &dyn Store, prefix: &str) -> Vec<(String, Vec<u8>, String)> {
    let rows = store.scan_prefix(prefix).unwrap();
    let each = rows.into_iter();
    each.map(|(key, row)| (key, row.value, format!("{:?}", row.version)))
        .collect()
}

/// The digest of a one-time token or host credential the previous release minted.
fn digest(token: &str) -> String {
    Credential::of_token(token).to_hex()
}

/// This is the test that the release before serves a store the new release ran on (the design's
/// compatibility test, section 7). The store holds what the previous release wrote: rows under
/// `hosts/join/` of a one-time token unused, one used by a host, a host credential that replaced
/// a token, and one carrying a field a later build wrote, and a host binding bound to a token's
/// credential. The new release starts over it, takes a host's certificate hello, a heartbeat
/// and a tick, and stops. Every row under `hosts/join/` is still there, byte for byte and at
/// the version it had (it neither reads nor writes the prefix), and every row under `hosts/join/`
/// and `hosts/binding/` is read by a frozen copy of the previous release's reader without a
/// refusal, so that the previous release started over the same store would serve it. It catches
/// a release that rewrites, drops or reshapes the rows of the tokens, or one that writes a
/// binding the previous release cannot read.
#[tokio::test]
async fn the_release_before_serves_a_store_the_new_release_ran_on() {
    let scratch = Scratch::new("hosts_store_previous_release");
    let hex = |digit: char| digit.to_string().repeat(64);
    let rows = [
        (
            digest("a-one-time-token-never-used"),
            json!({
                "version": 1, "provider": "lambdalabs", "instance_id": "i-1",
                "expires_at_ms": T0 + 1_800_000,
            }),
        ),
        (
            digest("a-one-time-token-used-by-burst-3"),
            json!({
                "version": 1, "provider": "lambdalabs", "instance_id": "i-2",
                "expires_at_ms": T0 + 1_800_000, "used_by": "burst-3",
                "credential_expires_at_ms": T0 + 86_400_000,
            }),
        ),
        (
            hex('c'),
            json!({
                "version": 1, "provider": "lambdalabs", "instance_id": "i-3",
                "expires_at_ms": T0 + 1_800_000, "used_by": "burst-4",
                "credential_expires_at_ms": T0 + 86_400_000, "replaces": hex('d'),
            }),
        ),
        (
            hex('e'),
            json!({
                "version": 3, "provider": "lambdalabs", "instance_id": "i-4",
                "expires_at_ms": T0 + 1_800_000, "written_by_a_later_build": [1, 2],
            }),
        ),
    ];
    let bound_to_a_token = json!({
        "version": 1, "credential": digest("the-pool-token"),
        "instance_provider": "lambdalabs", "instance_id": "i-9",
    });
    {
        let clock = Arc::new(SystemClock::default());
        let store = SqliteStore::open(&sqlite_db(&scratch), clock, Options::default()).unwrap();
        let mut seeded = Txn::new();
        for (key, value) in &rows {
            seeded = seeded.put(format!("hosts/join/{key}"), value.to_string(), Cond::Any);
        }
        seeded = seeded.put(
            "hosts/binding/burst-9",
            bound_to_a_token.to_string(),
            Cond::Any,
        );
        store.txn(seeded).unwrap().unwrap();
    }
    let args = Kept::Store.args(&scratch, agent_args(&scratch));
    let h = Harness::with(scratch, args).await;
    let store = h.running.store().expect("a server over a store");
    let before = rows_under(&*store, "hosts/join/");
    assert_eq!(before.len(), rows.len());

    let welcome = session(&hello_as(&h, "burst-1").await, "burst-1");
    assert_eq!(
        post(&h, "/v1/agent/heartbeat", None, &welcome.beat())
            .await
            .status,
        200
    );
    h.ticks.step().await;
    assert_eq!(
        post(&h, "/v1/agent/heartbeat", None, &welcome.beat())
            .await
            .status,
        200
    );
    h.ticks.step().await;
    h.running.shutdown().await.unwrap();

    assert_eq!(rows_under(&*store, "hosts/join/"), before);
    for (key, bytes, _) in rows_under(&*store, "hosts/join/") {
        let digest = key.trim_start_matches("hosts/join/");
        let read = previous_release_reads_join_row(digest, &bytes);
        assert_eq!(read, Ok(()), "{key}");
    }
    let bindings = rows_under(&*store, "hosts/binding/");
    assert!(bindings.len() >= 2, "{bindings:?}");
    for (key, bytes, _) in bindings {
        let host = key.trim_start_matches("hosts/binding/");
        assert_eq!(
            previous_release_reads_binding(host, &bytes),
            Ok(()),
            "{key}"
        );
    }
}

/// Asserts that a start over a store whose `hosts/join/` row is damaged (a value the previous
/// release refused the start for) starts, says hello for a host with a certificate, and leaves
/// the row as it was: the prefix is not read. It catches a start that still reads the rows of
/// the tokens, and one that deletes what it cannot read.
#[tokio::test]
async fn a_damaged_join_row_no_longer_refuses_the_start_and_is_left_alone() {
    let scratch = Scratch::new("hosts_store_damaged_join");
    let key = "hosts/join/k";
    {
        let clock = Arc::new(SystemClock::default());
        let store = SqliteStore::open(&sqlite_db(&scratch), clock, Options::default()).unwrap();
        store
            .txn(Txn::new().put(key, "{}", Cond::Any))
            .unwrap()
            .unwrap();
    }
    let args = Kept::Store.args(&scratch, agent_args(&scratch));
    let h = Harness::with(scratch, args).await;
    let store = h.running.store().expect("a server over a store");
    let before = rows_under(&*store, "hosts/join/");
    assert_eq!(hello_as(&h, "burst-1").await.status, 200);
    h.ticks.step().await;
    h.running.shutdown().await.unwrap();
    assert_eq!(rows_under(&*store, "hosts/join/"), before);
    assert_eq!(before.len(), 1);
}

/// Asserts that, in file mode, a `<state file>.joins` the previous release left beside the state
/// file is neither read nor changed nor refused at start: a file of loose mode and no valid
/// content (which the previous release refused the start for) is there after a start, a hello, a
/// heartbeat and a tick, byte for byte and with its mode, and the host is welcomed. It catches a
/// start that still opens the file, and one that rewrites it.
#[tokio::test]
async fn a_stale_joins_file_is_neither_read_nor_changed_nor_refused() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("hosts_files_stale_joins");
    let args = agent_args(&scratch);
    let mut stale = scratch.state().into_os_string();
    stale.push(".joins");
    let stale = PathBuf::from(stale);
    std::fs::write(&stale, b"not a version of the file {[").unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
    let h = Harness::with(scratch, args).await;
    let me = session(&hello_as(&h, "burst-1").await, "burst-1");
    assert_eq!(
        post(&h, "/v1/agent/heartbeat", None, &me.beat())
            .await
            .status,
        200
    );
    h.ticks.step().await;
    h.running.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read(&stale).unwrap(),
        b"not a version of the file {["
    );
    let mode = std::fs::metadata(&stale).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

/// Asserts that the first start over an empty store, which imports the files beside the state
/// file, does not import a `<state file>.joins` there: the store holds no row under
/// `hosts/join/` after the start and a hello, and the file is as it was. It catches an import
/// that still reads the file of the tokens.
#[tokio::test]
async fn a_joins_file_is_not_imported_into_a_new_store() {
    let scratch = Scratch::new("hosts_store_joins_import");
    let text = format!(
        "version = 1\n[[join]]\nhash = \"{}\"\nprovider = \"lambdalabs\"\ninstance_id = \"i-1\"\nexpires_at_ms = {}\n",
        digest("a-token-of-the-previous-release"),
        T0 + 1_800_000
    );
    let stale = scratch.write("leases.toml.joins", &text, 0o600);
    let args = Kept::Store.args(&scratch, agent_args(&scratch));
    let h = Harness::with(scratch, args).await;
    assert_eq!(hello_as(&h, "burst-1").await.status, 200);
    let store = h.running.store().expect("a server over a store");
    assert!(rows_under(&*store, "hosts/join/").is_empty());
    h.running.shutdown().await.unwrap();
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), text);
}
