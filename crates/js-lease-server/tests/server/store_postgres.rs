//! The writer lock over a Postgres store: a second server refused, a lost lock that stops the
//! server, a store fenced by the lock's generation, and refusals that never show the
//! connection URL; and the state a restart reads back from it, the first start's import of
//! the files beside the state file included.
//!
//! Each test has a database of its own (`js_state::postgres::test_database`, the state schema
//! applied), and an advisory lock is scoped to its database, so the tests do not see each
//! other's locks. They need the server `KBF_TEST_POSTGRES_URL` names (CI's `test` and
//! `coverage` jobs have one; a superuser, to end a backend) and fail, not skip, without it.
//! The database calls block, so they are made outside the async runtime a server runs on.

use std::collections::BTreeSet;
use std::fs;
use std::process::Stdio;

use js_lease_server::{LockLost, RecordsFile, UnremovedFile};
use js_scaler::Records;
use js_state::postgres::{PostgresConfig, TestDatabase, test_database};
use js_state::{Cond, Store, StoreError, Txn, WriterLock, WriterTarget};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::agents_support::{JOIN, JOIN_B, agent_args, hello_with, post, session};
use crate::binary::launch;
use crate::joins::{code, hello, minted};
use crate::store_support::{
    LIMIT, assert_unquoted, end_the_holder, holder, launch_ticking, revision, run_to_end,
    store_args, store_file, taken_address, unflushed, with_userinfo,
};
use crate::support::{Harness, Scratch, TOKEN, replace, request};

/// A role no test server has and a password for it: neither may ever be printed.
const ROLE: &str = "js_store_no_such_role";
const PASSWORD: &str = "store-secret-0123456789";

/// Asserts that a second server on a Postgres store another server holds exits 2 with the
/// one line naming the writer lock, and that it took the lock before anything else: it
/// bound no listener (its listen address is taken, which would have been refused naming
/// `--listen`), made no state file and printed no address; the store's revision is
/// unchanged, and the first server still holds the lock and serves. It catches a lock that
/// is not taken, taken after the listener or the state files, or released early.
#[test]
fn a_second_process_cannot_take_the_writer_lock() {
    let database = test_database();
    let url = database.url();
    let first = Scratch::new("store_pg_lock_first");
    let (mut child, addr) = launch(&store_args(&first, &url, &[]), Stdio::null());
    let holding = holder(&url).expect("the first server holds the writer lock");
    let before = revision(&url);

    let second = Scratch::new("store_pg_lock_second");
    let listen = taken_address();
    let at = listen.local_addr().unwrap().to_string();
    let args = replace(store_args(&second, &url, &[]), "--listen", at);
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
        "the second server made no state file"
    );
    assert_eq!(revision(&url), before, "the second server wrote nothing");
    assert_eq!(
        holder(&url),
        Some(holding),
        "the first server still holds the lock"
    );

    let runtime = Runtime::new().unwrap();
    let health = runtime.block_on(request(addr, "GET", "/healthz", None, None));
    assert_eq!(health.status, 200, "the first server serves on");
    kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
    assert!(child.0.wait().unwrap().success());
}

/// Asserts that when the session holding the writer lock is ended from another connection,
/// the next tick stops the server: `lock_lost` names the loss, the tick does nothing else,
/// the shutdown runs no final tick and answers the loss as an error, and the store's
/// revision is unchanged. A tick before the loss runs as usual. It catches a server that
/// keeps serving and writing after its session, and its lock, ended.
#[test]
fn losing_the_writer_lock_stops_the_server() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_lost");
    let args = replace(store_args(&scratch, &url, &[]), "--boot", "resume");
    let runtime = Runtime::new().unwrap();
    let h = runtime.block_on(Harness::with(scratch, args, None));
    runtime.block_on(h.ticks.step());
    let before = revision(&url);
    end_the_holder(&url);

    let lost: LockLost = runtime.block_on(async {
        h.ticks.step().await;
        h.running.lock_lost().await
    });
    let said = lost.to_string();
    assert!(
        said.starts_with("the store's writer lock was lost, so the server stopped: "),
        "{said}"
    );
    assert_unquoted(&url, &said);

    let Harness { running, .. } = h;
    let stopped = runtime.block_on(running.shutdown());
    assert_eq!(stopped.err().map(|err| err.to_string()), Some(said));
    assert_eq!(revision(&url), before, "nothing was written to the store");
}

/// Asserts that the program whose writer lock is lost ends on its own with exit status 1,
/// after one line saying the lock was lost, and that the line carries neither the URL nor
/// its password. It catches a program that keeps running, or exits 0, without its lock.
#[test]
fn losing_the_writer_lock_ends_the_program_with_status_1() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_lost_program");
    let (mut child, _addr) = launch_ticking(&store_args(&scratch, &url, &[]), "50ms");
    end_the_holder(&url);
    let pid = Pid::from_child(&child.0);
    let (done, ended) = std::sync::mpsc::channel();
    let mut stderr = child.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        let _ = done.send(text);
    });
    let stderr = ended.recv_timeout(LIMIT);
    if stderr.is_err() {
        let _ = kill_process(pid, Signal::KILL);
    }
    let stderr = stderr.expect("the program ended on its own");
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(1), "{stderr}");
    let last = stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with(
            "js-lease-server: the store's writer lock was lost, so the server stopped: "
        ),
        "{stderr}"
    );
    assert_unquoted(&url, &stderr);
}

/// Asserts that a refused login (a role the server does not have, with a password) is
/// refused at start with exit status 2, naming `--store` and the server's SQLSTATE, and that
/// neither the URL, nor the password, nor the role is in anything the program printed, its
/// log lines included. (The test server trusts every login, so a wrong password alone would
/// be accepted; a role it does not have is refused the same way, SQLSTATE 28000.) It catches
/// a refusal or a log line that quotes the connection.
#[test]
fn the_store_url_is_never_in_an_error_or_a_log_line() {
    let database = test_database();
    let url = with_userinfo(&database.url(), &format!("{ROLE}:{PASSWORD}"));
    let scratch = Scratch::new("store_pg_url");
    let ended = run_to_end(&store_args(&scratch, &url, &[]));
    assert_eq!(ended.code, Some(2), "{}", ended.stderr);
    assert!(
        ended.stderr.contains(
            "js-lease-server: --store: the store's writer lock could not be taken: the server refused (SQLSTATE 28000)\n"
        ),
        "{}",
        ended.stderr
    );
    let printed = format!("{}{}", ended.stdout, ended.stderr);
    for secret in [url.as_str(), PASSWORD, ROLE] {
        assert!(!printed.contains(secret), "{secret} in {printed}");
    }
}

/// Asserts that a Postgres database without the state schema is refused at start with exit
/// status 2 when the writer lock is taken (the lock takes the schema's writer generation),
/// naming `--store` with the line that says what to run, that the refusal releases the lock
/// and made no lease file, and that no part of the URL is printed. It catches a server that
/// runs on a database it cannot keep its state in, or that creates a schema of its own.
#[test]
fn a_store_without_the_schema_is_refused_at_start() {
    let database = TestDatabase::empty();
    let url = database.url();
    let scratch = Scratch::new("store_pg_no_schema");
    let ended = run_to_end(&store_args(&scratch, &url, &[]));
    assert_eq!(ended.code, Some(2), "{}", ended.stderr);
    assert!(
        ended.stderr.contains(
            "js-lease-server: --store: the store's writer lock could not be taken: the state \
             schema has no writer generation table (it is older than version 2): run the \
             installer to bring it forward\n"
        ),
        "{}",
        ended.stderr
    );
    assert_unquoted(&url, &ended.stderr);
    assert!(!scratch.state().exists(), "no lease file was made");
    assert_eq!(holder(&url), None, "the refusal released the lock");
}

/// Asserts that a store whose writer lock was taken but that does not open (its database
/// does not flush commits: `synchronous_commit` is off) is refused at start with exit status
/// 2, naming `--store`, saying the store could not be opened and why; that the refusal
/// released the lock and made no lease file; and that no part of the URL is printed. It
/// catches a refused open that keeps the lock, or a server that goes on to serve.
#[test]
fn a_store_that_does_not_open_is_refused_after_the_lock() {
    let database = test_database();
    let url = database.url();
    unflushed(&url);
    let scratch = Scratch::new("store_pg_unflushed");
    let ended = run_to_end(&store_args(&scratch, &url, &[]));
    assert_eq!(ended.code, Some(2), "{}", ended.stderr);
    assert!(
        ended
            .stderr
            .contains("js-lease-server: --store: the store could not be opened: "),
        "{}",
        ended.stderr
    );
    assert!(
        ended.stderr.contains("synchronous_commit"),
        "{}",
        ended.stderr
    );
    assert_unquoted(&url, &ended.stderr);
    assert!(!scratch.state().exists(), "no lease file was made");
    assert_eq!(holder(&url), None, "the refusal released the lock");
}

/// Asserts that the server's store is opened fenced by the generation its writer lock took:
/// a commit succeeds while the server holds the lock, and once its session is ended and
/// another process takes the lock, a commit of the server's store is refused `Fenced` and
/// writes nothing, with no tick run in between. It catches a store opened without the
/// fence, whose commits would land after another writer took over.
#[test]
fn a_commit_after_another_process_took_the_lock_is_refused() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_fenced");
    let args = store_args(&scratch, &url, &[]);
    let runtime = Runtime::new().unwrap();
    let h = runtime.block_on(Harness::with(scratch, args, None));
    let store = h.running.store().expect("the server has a store");
    let put = |key: &str| Txn::new().put(key, "v", Cond::Any);
    let committed = store.txn(put("before"));
    assert!(committed.is_ok(), "{committed:?}");

    end_the_holder(&url);
    let target = WriterTarget::Postgres(Box::new(PostgresConfig::parse(&url).unwrap()));
    let next = WriterLock::acquire(&target).expect("another process takes the lock");
    let before = revision(&url);
    assert_eq!(store.txn(put("after")).err(), Some(StoreError::Fenced));
    assert_eq!(revision(&url), before, "the fenced commit wrote nothing");
    drop(next);
    drop(store);
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}

/// Asserts that the host bindings and the join tokens survive a restart over a Postgres store
/// as they do over SQLite (`hosts_store.rs`): after the restart a tainted host's binding
/// refuses another credential (403 `host-bound`) and the host still shows its taint, a used
/// one-time token is refused for another host as used and goes on serving its own, and the
/// store holds the token's digest and never its text, with no `.agents` or `.joins` file
/// beside the lease file. It catches a Postgres store that loses what SQLite keeps.
#[test]
fn bindings_and_join_tokens_survive_a_restart_on_postgres() {
    let database = test_database();
    let url = database.url();
    let flags = |scratch: &Scratch| {
        let mut args = agent_args(scratch, false);
        args.extend(["--store".into(), store_file(scratch, &url).into_os_string()]);
        args
    };
    let scratch = Scratch::new("store_pg_hosts");
    let args = flags(&scratch);
    let runtime = Runtime::new().unwrap();
    let (h, token) = runtime.block_on(async {
        let first = Harness::with(scratch, args, None).await;
        let me = session(&hello_with(&first, JOIN, "burst-1").await, "burst-1");
        let mut beat = me.beat();
        beat["taint"] = json!({ "reason": "residue" });
        let reply = post(&first, "/v1/agent/heartbeat", Some(JOIN), &beat).await;
        assert_eq!(reply.json()["taint_ack"], true, "{}", reply.body);
        let token = minted(&first, "i-2").await;
        assert_eq!(hello(&first, &token, "burst-2", "i-2").await.status, 200);
        let Harness {
            running, scratch, ..
        } = first;
        running.shutdown().await.unwrap();
        let resumed = replace(flags(&scratch), "--boot", "resume");
        (Harness::with(scratch, resumed, None).await, token)
    });
    runtime.block_on(async {
        let other = hello_with(&h, JOIN_B, "burst-1").await;
        assert_eq!((other.status, code(&other)), (403, "host-bound".to_owned()));
        let listing = request(h.addr(), "GET", "/v1/hosts", Some(TOKEN), None).await;
        assert!(listing.body.contains("residue"), "{}", listing.body);
        let taken = hello(&h, &token, "burst-3", "i-2").await;
        assert_eq!(
            (taken.status, code(&taken)),
            (403, "join-token-used".to_owned())
        );
        assert_eq!(hello(&h, &token, "burst-2", "i-2").await.status, 200);
    });
    let store = h.running.store().expect("the server holds the store");
    let rows = store.scan_prefix("hosts/").unwrap();
    let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
    assert!(keys.contains(&"hosts/binding/burst-1"), "{keys:?}");
    assert!(keys.contains(&"hosts/binding/burst-2"), "{keys:?}");
    let mut values = rows
        .iter()
        .map(|(_, row)| String::from_utf8_lossy(&row.value));
    assert!(values.all(|value| !value.contains(token.as_str())));
    for suffix in [".agents", ".joins"] {
        let file = h.scratch.path(&format!("leases.toml{suffix}"));
        assert!(!file.exists(), "{suffix}");
    }
    drop(store);
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}

/// Asserts the first start's import over Postgres: with a bindings file, a records file and a
/// file of hosts to remove beside the lease file, the first start leaves the store holding
/// the binding, the records file's text, the hosts and the marker; a restart writes nothing
/// to the store (its revision is the same) and still holds them, though the bindings file
/// was removed in between. It catches an import that the Postgres backend does not commit
/// whole, and one that runs again at the next start.
#[test]
fn the_files_are_imported_once_over_postgres() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_import");
    let agents = format!(
        "version = 1\n\n[[host]]\nid = \"burst-9\"\ncredential = \"{}\"\ntaint_reason = \"residue\"\n",
        "c".repeat(64)
    );
    let agents_path = scratch.write("leases.toml.agents", &agents, 0o600);
    let records = RecordsFile::new(scratch.path("leases.toml.scaler"));
    records.save(&Records::default()).unwrap();
    let records_text = fs::read_to_string(scratch.path("leases.toml.scaler")).unwrap();
    let hosts = BTreeSet::from(["js-ghost".to_owned()]);
    UnremovedFile::new(scratch.path("leases.toml.scaler.unremoved"))
        .save(&hosts)
        .unwrap();
    let flags = |scratch: &Scratch| {
        let mut args = agent_args(scratch, false);
        args.extend(["--store".into(), store_file(scratch, &url).into_os_string()]);
        args
    };
    let args = flags(&scratch);
    let runtime = Runtime::new().unwrap();
    let first = runtime.block_on(Harness::with(scratch, args, None));
    let store = first.running.store().expect("the server holds the store");
    let keys: Vec<String> = store
        .scan_prefix("")
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        keys,
        [
            "acquires/scaler/records",
            "acquires/scaler/unremoved",
            "hosts/binding/burst-9",
            "meta/server-state-imported",
        ]
    );
    let stored = store.get("acquires/scaler/records").unwrap().unwrap().value;
    assert_eq!(String::from_utf8(stored).unwrap(), records_text);
    let stored = store
        .get("acquires/scaler/unremoved")
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(stored, br#"{"version":1,"hosts":["js-ghost"]}"#);
    drop(store);
    let Harness {
        running, scratch, ..
    } = first;
    runtime.block_on(running.shutdown()).unwrap();
    let before = revision(&url);

    fs::remove_file(&agents_path).unwrap();
    let resumed = replace(flags(&scratch), "--boot", "resume");
    let h = runtime.block_on(Harness::with(scratch, resumed, None));
    assert_eq!(revision(&url), before, "the restart wrote nothing");
    let store = h.running.store().expect("the server holds the store");
    assert!(store.get("hosts/binding/burst-9").unwrap().is_some());
    drop(store);
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}
