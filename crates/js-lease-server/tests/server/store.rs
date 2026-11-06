//! The store setting (`--store`, `server.store`), its SQLite gate, and the writer lock the
//! server takes first, over the SQLite backend (the Postgres side is `store_postgres.rs`).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;
use std::sync::Arc;

use js_lease_server::{Cli, Config, OwnerOnlyFiles, StartupError, run};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Revision, Store, SystemClock, WriterLock, WriterTarget};
use rustix::process::{Pid, Signal, kill_process};

use crate::agents_support::agent_args;
use crate::binary::launch;
use crate::config_support::{PoolText, clock, file, kinds, quoted, server};
use crate::scaler_support::{account, records_path, start_refused, text};
use crate::store_support::{
    run_to_end, sqlite_args_over, sqlite_db, sqlite_line, store_args, store_file, taken_address,
};
use crate::support::{Harness, Scratch, base_args, config, replace, request};

/// What the program wrote to standard error and how it ended, run in this process.
fn run_captured(args: Vec<OsString>) -> (ExitCode, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(args, &mut out, &mut err);
    assert!(out.is_empty(), "nothing is printed to standard output");
    (code, String::from_utf8(err).unwrap())
}

/// The refusal of the configuration file `text`, written to `js.toml` of `scratch`.
fn file_refusal(scratch: &Scratch, text: &str) -> String {
    file_config(scratch, text).err().unwrap().to_string()
}

/// The configuration file `text`, written to `js.toml` of `scratch`, checked.
fn file_config(scratch: &Scratch, text: &str) -> Result<Config, StartupError> {
    let path = scratch.write("js.toml", text, 0o644);
    Config::from_config_file(&path, &kinds(), &OwnerOnlyFiles, clock())
}

/// A file with a static pool and `extra` lines in `[server]`.
fn static_file(scratch: &Scratch, extra: &str) -> String {
    file(
        &server(scratch, extra),
        &[&PoolText::static_pool(scratch, "hosts")],
    )
}

/// Asserts that a `sqlite:` store is refused at start with exit status 2 and a message that
/// names the flag (or the key) and says SQLite is for tests and single-node development,
/// unless the dev flag (or the key) is set, and that with it the server starts over the
/// store. Also: the flag beside `--config` is refused as every flag is, the key in
/// `[server]` is accepted, the flag (or the key set true) without a store is refused, and
/// beside a Postgres store the flag and the key are accepted and change nothing. It catches
/// a SQLite store accepted silently, an unknown key, and a gate that does nothing.
#[tokio::test]
async fn sqlite_is_refused_unless_the_dev_flag_is_set() {
    let scratch = Scratch::new("store_sqlite_gate");
    let line = sqlite_line(&scratch);
    let (code, said) = run_captured(store_args(&scratch, &line, &[]));
    assert!(code == ExitCode::from(2));
    let store = scratch.path("store").display().to_string();
    assert_eq!(
        said,
        format!(
            "js-lease-server: --store: {store} names a sqlite: store, which is accepted only with --allow-sqlite-store: SQLite is for tests and single-node development\n"
        )
    );
    assert!(
        !sqlite_db(&scratch).exists(),
        "the refused store is not made"
    );

    let gated = store_args(&scratch, &line, &["--allow-sqlite-store"]);
    let h = Harness::with(scratch, gated, None).await;
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    assert!(h.running.store().is_some(), "the server holds the store");
    assert!(sqlite_db(&h.scratch).exists());
    // A tick asks the lock first; on SQLite the store holds it, and the tick runs.
    h.ticks.step().await;
    assert_eq!(h.running.save_errors(), 0);
    h.running.shutdown().await.unwrap();

    let scratch = Scratch::new("store_sqlite_gate_file");
    let path = quoted(&store_file(&scratch, &sqlite_line(&scratch)));
    let refused = file_refusal(&scratch, &static_file(&scratch, &format!("store = {path}")));
    let store = scratch.path("store").display().to_string();
    assert_eq!(
        refused,
        format!(
            "server.store: {store} names a sqlite: store, which is accepted only with server.allow_sqlite_store = true: SQLite is for tests and single-node development"
        )
    );
    let gated = format!("store = {path}\nallow_sqlite_store = true");
    let checked = file_config(&scratch, &static_file(&scratch, &gated)).unwrap();
    let h = Harness::over(scratch, checked).await;
    assert!(h.running.store().is_some());
    h.running.shutdown().await.unwrap();

    let scratch = Scratch::new("store_sqlite_gate_refusals");
    let beside = vec![
        OsString::from("js-lease-server"),
        "--config".into(),
        scratch
            .write("js.toml", &static_file(&scratch, ""), 0o644)
            .into(),
        "--allow-sqlite-store".into(),
    ];
    let refused = Config::from_cli(Cli::try_parse_from(beside).unwrap())
        .err()
        .unwrap();
    assert_eq!(
        refused.to_string(),
        "--allow-sqlite-store: not allowed with --config; set server.allow_sqlite_store"
    );
    let mut alone = base_args(&scratch);
    alone.push("--allow-sqlite-store".into());
    let refused = config(alone).err().unwrap().to_string();
    assert_eq!(refused, "--allow-sqlite-store: requires --store");
    let refused = file_refusal(
        &scratch,
        &static_file(&scratch, "allow_sqlite_store = true"),
    );
    assert_eq!(refused, "server.allow_sqlite_store: needs server.store");
    assert!(
        file_config(
            &scratch,
            &static_file(&scratch, "allow_sqlite_store = false")
        )
        .is_ok()
    );

    let postgres = "postgres://js:pw-0123@127.0.0.1:1/js";
    assert!(config(store_args(&scratch, postgres, &["--allow-sqlite-store"])).is_ok());
    let path = quoted(&store_file(&scratch, postgres));
    let keys = format!("store = {path}\nallow_sqlite_store = true");
    assert!(file_config(&scratch, &static_file(&scratch, &keys)).is_ok());
    let refused = file_refusal(&scratch, &static_file(&scratch, "store = 7"));
    assert_eq!(refused, "server.store: must be a string");
}

/// Asserts that a store file another user could read or write is refused at start naming
/// `--store` (or `server.store`), as the token files are, and that an owner-only one (0600
/// or 0400) is read; a missing file is refused too. It catches a connection URL, and its
/// password, read from a file anyone on the host can read.
#[test]
fn the_store_file_is_refused_when_others_can_read_it() {
    let scratch = Scratch::new("store_file_mode");
    let args = store_args(&scratch, "postgres://js:pw-0123@127.0.0.1:1/js", &[]);
    let store = scratch.path("store");
    for mode in [0o600, 0o400] {
        fs::set_permissions(&store, fs::Permissions::from_mode(mode)).unwrap();
        assert!(config(args.clone()).is_ok(), "mode {mode:o}");
    }
    for mode in [0o644, 0o640, 0o604, 0o660, 0o606, 0o700] {
        fs::set_permissions(&store, fs::Permissions::from_mode(mode)).unwrap();
        let refused = config(args.clone()).err().unwrap();
        assert_eq!(refused.flag(), "--store", "mode {mode:o}");
        assert!(
            refused.reason().contains("(mode 0600 or 0400)"),
            "{refused}"
        );
        assert!(!refused.to_string().contains("pw-0123"), "{refused}");
    }
    let path = quoted(&store);
    let refused = file_refusal(&scratch, &static_file(&scratch, &format!("store = {path}")));
    assert!(refused.starts_with("server.store: "), "{refused}");
    assert!(refused.contains("(mode 0600 or 0400)"), "{refused}");
    let missing = replace(args, "--store", scratch.path("absent"));
    assert_eq!(config(missing).err().unwrap().flag(), "--store");
}

/// Asserts that a second server on a SQLite store another server holds exits 2 with the one
/// line naming the writer lock, and that it took the lock before anything else: it bound no
/// listener (its listen address is taken, which would have been refused naming `--listen`),
/// made no lease file, printed no address, and did not open the store (the store's epoch,
/// bumped by each open, says one server opened it). The first server goes on serving. It
/// catches a lock that is not taken, taken after the listener or the lease file, or not held.
#[tokio::test]
async fn a_second_process_cannot_take_the_writer_lock_on_sqlite() {
    let first = Scratch::new("store_lock_first");
    let (mut child, addr) = launch(
        &sqlite_args_over(&first, &first),
        std::process::Stdio::null(),
    );

    let second = Scratch::new("store_lock_second");
    let listen = taken_address();
    let at = listen.local_addr().unwrap().to_string();
    let args = replace(sqlite_args_over(&second, &first), "--listen", at);
    let ended = run_to_end(&args);
    assert_eq!(ended.code, Some(2), "{}", ended.stderr);
    assert!(
        ended
            .stderr
            .contains("js-lease-server: --store: another process holds the store's writer lock\n"),
        "{}",
        ended.stderr
    );
    assert_eq!(ended.stdout, "", "no address was printed");
    assert!(
        !second.state().exists(),
        "the second server made no lease file"
    );

    let health = request(addr, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200, "the first server serves on");
    kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
    assert!(child.0.wait().unwrap().success());
    let clock = Arc::new(SystemClock::default());
    let store = SqliteStore::open(&sqlite_db(&first), clock, Options::default()).unwrap();
    assert_eq!(
        store.epoch(),
        2,
        "only the first server and this test opened the store"
    );
    let changes = store.changes(Revision::ZERO, 100).unwrap();
    assert_eq!((changes.rows.len(), changes.cursor), (0, Revision::ZERO));
}

/// Asserts that a store setting beside an elastic pool is refused at start with the message
/// that store-backed server state is not available yet for it, naming the setting, before any
/// listener is bound and before the scaler writes anything, and that the writer lock the
/// refused start took is released with it. It catches a missing guard, which would start a
/// server whose scaler records are not in the store it holds.
#[tokio::test]
async fn a_store_with_an_elastic_pool_refuses_with_the_not_available_message() {
    let scratch = Scratch::new("store_elastic");
    let _account = account(&scratch);
    let path = quoted(&store_file(&scratch, &sqlite_line(&scratch)));
    let keys = format!("[server]\nstore = {path}\nallow_sqlite_store = true\n");
    let text = text(&scratch, "live", "initialise").replacen("[server]\n", &keys, 1);
    let refused = start_refused(&scratch, &text).await;
    assert_eq!(
        refused,
        "server.store: store-backed server state is not available yet for a server with an elastic pool; start this server without a store setting"
    );
    assert!(!records_path(&scratch).exists(), "the scaler wrote nothing");
    assert!(!scratch.state().exists(), "no lease file was made");
    let free = WriterLock::acquire(&WriterTarget::Sqlite(sqlite_db(&scratch)));
    assert!(free.is_ok(), "the refused start released the lock");
}

/// Asserts that agent registration from the flags, whose bindings and join tokens the store
/// keeps, starts over a SQLite store with the dev flag and serves, and that a tick writes no
/// agent state or join token file beside the lease file. It catches a guard that still
/// refuses agent registration with a store setting, and a store mode that writes the files.
#[tokio::test]
async fn agent_registration_starts_over_a_store_and_writes_no_files() {
    let scratch = Scratch::new("store_agents");
    let mut args = agent_args(&scratch, true);
    args.extend([
        "--store".into(),
        store_file(&scratch, &sqlite_line(&scratch)).into(),
    ]);
    args.push("--allow-sqlite-store".into());
    let h = Harness::with(scratch, args, None).await;
    assert!(h.running.store().is_some(), "the server holds the store");
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    h.ticks.step().await;
    assert!(!h.scratch.path("leases.toml.agents").exists());
    assert!(!h.scratch.path("leases.toml.joins").exists());
    h.running.shutdown().await.unwrap();
}

/// Asserts that a SQLite store that cannot be made owner-only (a directory in its place), or
/// that is not a state store (a text file), is refused at start naming `--store`, after the
/// lock, which the refusal releases.
#[tokio::test]
async fn a_sqlite_store_that_cannot_be_opened_is_refused() {
    let scratch = Scratch::new("store_sqlite_dir");
    fs::create_dir(sqlite_db(&scratch)).unwrap();
    let args = sqlite_args_over(&scratch, &scratch);
    let (_ticks, ticker) = js_lease_server::channel_ticker();
    let checked = config(args).unwrap();
    let refused = js_lease_server::start(checked, clock(), ticker).await;
    let refused = refused.err().unwrap();
    assert_eq!(refused.flag(), "--store");
    let db = sqlite_db(&scratch).display().to_string();
    assert!(
        refused
            .reason()
            .starts_with(&format!("the store could not be opened: {db}: ")),
        "{refused}"
    );
    let free = WriterLock::acquire(&WriterTarget::Sqlite(sqlite_db(&scratch)));
    assert!(free.is_ok(), "the refused start released the lock");
    drop(free);

    // A file that can be made owner-only but is not a state store: refused by the open.
    let scratch = Scratch::new("store_sqlite_not_a_store");
    scratch.write("state.db", "not a state store\n", 0o600);
    let args = sqlite_args_over(&scratch, &scratch);
    let (_ticks, ticker) = js_lease_server::channel_ticker();
    let refused = js_lease_server::start(config(args).unwrap(), clock(), ticker).await;
    let refused = refused.err().unwrap();
    assert_eq!(refused.flag(), "--store");
    assert!(
        refused
            .reason()
            .starts_with("the store could not be opened: "),
        "{refused}"
    );
    let free = WriterLock::acquire(&WriterTarget::Sqlite(sqlite_db(&scratch)));
    assert!(free.is_ok(), "the refused start released the lock");
}

/// Asserts that a server started without a store setting holds no store and makes no file
/// besides those it made before the setting existed (the lease file and its lock). It
/// catches a store or a lock file made, or the lock taken, without the setting.
#[tokio::test]
async fn without_a_store_setting_nothing_changes() {
    let h = Harness::new("store_none").await;
    assert!(h.running.store().is_none());
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    h.ticks.step().await;
    let names = |scratch: &Scratch| -> BTreeSet<String> {
        fs::read_dir(&scratch.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    let expected = ["leases.toml", "leases.toml.lock", "pool.toml", "token"];
    let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(names(&h.scratch), expected);
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    assert_eq!(names(&scratch), expected);
}
