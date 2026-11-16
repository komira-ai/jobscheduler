//! A server over a Postgres store: a second server serves beside the first, refusals that
//! never show the connection URL, and the state a restart reads back from it, the first
//! start's import of the files beside the state file included.
//!
//! Each test has a database of its own (`js_state::postgres::test_database`, the state schema
//! applied). They need the server `KBF_TEST_POSTGRES_URL` names (CI's `test` and
//! `coverage` jobs have one; a superuser, to end a backend) and fail, not skip, without it.
//! The database calls block, so they are made outside the async runtime a server runs on.

use std::collections::BTreeSet;
use std::fs;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::ScriptedIds;
use js_lease_server::{RecordsFile, UnremovedFile};
use js_scaler::Records;
use js_state::postgres::{TestDatabase, test_database};
use js_state::{ManualClock, Store};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::agents_support::{JOIN, JOIN_B, agent_args, hello_with, job_host_args, post, session};
use crate::binary::{address_in, launch, spawn};
use crate::jobs_support::{job_agent, job_of, per_job_hello, submit};
use crate::joins::{code, hello, minted};
use crate::ready_listen::with_ready;
use crate::store_support::{
    assert_unquoted, revision, run_to_end, store_args, store_file, unflushed, with_userinfo,
};
use crate::support::{Harness, Scratch, TOKEN, config, replace, request};

/// A role no test server has and a password for it: neither may ever be printed.
const ROLE: &str = "js_store_no_such_role";
const PASSWORD: &str = "store-secret-0123456789";

/// Asserts that a second server on a Postgres store another server serves starts at once and
/// serves beside it: both print their address, both answer `/healthz` 200, neither waits for
/// a lock, and when the first is stopped with SIGTERM the second keeps serving. It catches a
/// server that takes a lock of the store, or that stops serving when another opens it.
#[test]
fn a_second_process_serves_beside_the_first() {
    let database = test_database();
    let url = database.url();
    let first = Scratch::new("store_pg_pair_first");
    let (mut first_child, first_addr) = launch(&store_args(&first, &url, &[]), Stdio::null());
    let second = Scratch::new("store_pg_pair_second");
    let (mut second_child, second_addr) = launch(&store_args(&second, &url, &[]), Stdio::null());
    assert_ne!(first_addr, second_addr);
    let runtime = Runtime::new().unwrap();
    for addr in [first_addr, second_addr] {
        let health = runtime.block_on(request(addr, "GET", "/healthz", None, None));
        assert_eq!(health.status, 200, "{addr}");
    }

    kill_process(Pid::from_child(&first_child.0), Signal::TERM).unwrap();
    assert!(first_child.0.wait().unwrap().success());
    let health = runtime.block_on(request(second_addr, "GET", "/healthz", None, None));
    assert_eq!(health.status, 200, "the second server serves on");
    kill_process(Pid::from_child(&second_child.0), Signal::TERM).unwrap();
    assert!(second_child.0.wait().unwrap().success());
}

/// Asserts that a second server with the readiness route on, over a Postgres store another
/// server serves, answers `/readyz` 200 `ready` once it serves, and was never `waiting`: a
/// Postgres store has no lock to wait for. It catches a route that still says it waits.
#[test]
fn a_second_process_with_a_route_does_not_wait() {
    let database = test_database();
    let url = database.url();
    let first = Scratch::new("store_pg_route_first");
    let (mut first_child, _) = launch(&store_args(&first, &url, &[]), Stdio::null());
    let second = Scratch::new("store_pg_route_second");
    let args = with_ready(store_args(&second, &url, &[]), "127.0.0.1:0");
    let (mut serving, lines) = spawn(&args, Stdio::null());
    let ready = address_in(&lines.next(), "ready on ");
    let _listen = address_in(&lines.next(), "listening on ");
    let runtime = Runtime::new().unwrap();
    let reply = runtime.block_on(request(ready, "GET", "/readyz", None, None));
    assert_eq!((reply.status, reply.body.as_str()), (200, "ready"));
    for child in [&mut first_child, &mut serving] {
        kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
        assert!(child.0.wait().unwrap().success());
    }
}

/// Asserts that a refused login (a role the server does not have, with a password) is
/// refused at start with exit status 2, naming `--store` and that the server refused the login, and that
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
            "js-lease-server: --store: the store could not be opened: the server refused the login\n"
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
/// status 2, naming `--store` and that the store could not be opened, that the refusal made
/// no lease file, and that no part of the URL is printed. It catches a server that runs on a
/// database it cannot keep its state in, or that creates a schema of its own.
#[test]
fn a_store_without_the_schema_is_refused_at_start() {
    let database = TestDatabase::empty();
    let url = database.url();
    let scratch = Scratch::new("store_pg_no_schema");
    let ended = run_to_end(&store_args(&scratch, &url, &[]));
    assert_eq!(ended.code, Some(2), "{}", ended.stderr);
    assert!(
        ended
            .stderr
            .contains("js-lease-server: --store: the store could not be opened: "),
        "{}",
        ended.stderr
    );
    assert_unquoted(&url, &ended.stderr);
    assert!(!scratch.state().exists(), "no lease file was made");
}

/// Asserts that a store that does not open (its database does not flush commits:
/// `synchronous_commit` is off) is refused at start with exit status 2, naming `--store`,
/// saying the store could not be opened and why; that the refusal made no lease file; and
/// that no part of the URL is printed. It catches a server that goes on to serve a store
/// that can lose an acknowledged write.
#[test]
fn a_store_that_does_not_open_is_refused() {
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
}

/// Asserts that the host bindings and the join tokens survive a restart over a Postgres store
/// as they do over SQLite (`hosts_store.rs`): after the restart a tainted host's binding
/// refuses another credential (403 `host-bound`) and the host still shows its taint, a used
/// one-time token is refused for another host as used and goes on serving its own, and the
/// store holds the token's digest and never its text, with no `.agents` or `.joins` file
/// beside the state file. It catches a Postgres store that loses what SQLite keeps.
#[test]
fn bindings_and_join_tokens_survive_a_restart_on_postgres() {
    let database = test_database();
    let url = database.url();
    let flags = |scratch: &Scratch| {
        let mut args = agent_args(scratch);
        args.extend(["--store".into(), store_file(scratch, &url).into_os_string()]);
        args
    };
    let scratch = Scratch::new("store_pg_hosts");
    let args = flags(&scratch);
    let runtime = Runtime::new().unwrap();
    let (h, token) = runtime.block_on(async {
        let first = Harness::with(scratch, args).await;
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
        (Harness::with(scratch, resumed).await, token)
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
/// file of hosts to remove beside the state file, the first start leaves the store holding
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
        let mut args = agent_args(scratch);
        args.extend(["--store".into(), store_file(scratch, &url).into_os_string()]);
        args
    };
    let args = flags(&scratch);
    let runtime = Runtime::new().unwrap();
    let first = runtime.block_on(Harness::with(scratch, args));
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
    let h = runtime.block_on(Harness::with(scratch, resumed));
    assert_eq!(revision(&url), before, "the restart wrote nothing");
    let store = h.running.store().expect("the server holds the store");
    assert!(store.get("hosts/binding/burst-9").unwrap().is_some());
    drop(store);
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}

/// Asserts that with a Postgres store the job store lives in it: a job submitted to the first
/// server is rows `job/` of the store and is read back by the server of the restart, and
/// neither a `.jobs` file nor a `usage.db` is made beside the state file. It catches a job
/// store that is still a local SQLite file when the server has a shared store, which a second
/// instance could not see.
#[test]
fn the_job_store_is_in_postgres_and_survives_a_restart() {
    let database = test_database();
    let url = database.url();
    let flags = |scratch: &Scratch| {
        let mut args = job_host_args(scratch);
        args.extend(["--store".into(), store_file(scratch, &url).into_os_string()]);
        args
    };
    let scratch = Scratch::new("store_pg_jobs");
    let args = flags(&scratch);
    let runtime = Runtime::new().unwrap();
    let h = runtime.block_on(async {
        let first = Harness::with(scratch, args).await;
        job_agent(&first, &per_job_hello("burst-1")).await;
        let made = submit(&first, "kept-1").await;
        assert_eq!(made.status, 201, "{}", made.body);
        let Harness {
            running, scratch, ..
        } = first;
        running.shutdown().await.unwrap();
        let resumed = replace(flags(&scratch), "--boot", "resume");
        Harness::with(scratch, resumed).await
    });
    let read = runtime.block_on(job_of(&h, "kept-1"));
    assert_eq!(read.status, 200, "{}", read.body);
    let store = h.running.store().expect("the server holds the store");
    let keys: Vec<String> = store
        .scan_prefix("job/")
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, ["job/kept-1"]);
    for file in ["leases.toml.jobs", "usage.db"] {
        assert!(!h.scratch.path(file).exists(), "{file}");
    }
    drop(store);
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}

/// Asserts that a server over a Postgres store whose job tables are given a clock (as a
/// simulation does) stamps the rows of the job store in Postgres with that clock: a job
/// submitted with the clock at 42 s is reported queued at 42000 ms, and is the row `job/` of
/// the store. It catches a Postgres store that stamps with the database's clock whatever the
/// server was given, which a simulation could not drive.
#[test]
fn the_job_store_in_postgres_is_stamped_with_the_clock_it_is_given() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_jobs_clock");
    let mut args = job_host_args(&scratch);
    args.extend([
        "--store".into(),
        store_file(&scratch, &url).into_os_string(),
    ]);
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(42));
    let ids = Arc::new(ScriptedIds::new([7; 16], []));
    let config = config(args).unwrap().with_job_sources(clock, ids);
    let runtime = Runtime::new().unwrap();
    let h = runtime.block_on(Harness::over(scratch, config));
    runtime.block_on(job_agent(&h, &per_job_hello("burst-1")));
    let made = runtime.block_on(submit(&h, "stamped-1"));
    assert_eq!(made.status, 201, "{}", made.body);
    assert_eq!(made.json()["job"]["times"]["queuedAtMs"], "42000");
    let Harness { running, .. } = h;
    let _ = runtime.block_on(running.shutdown());
}
