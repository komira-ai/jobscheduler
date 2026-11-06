//! The writer lock over a Postgres store: a second server refused, a lost lock that stops the
//! server, and refusals that never show the connection URL.
//!
//! Each test has a database of its own (`js_state::postgres::test_database`, the state schema
//! applied), and an advisory lock is scoped to its database, so the tests do not see each
//! other's locks. They need the server `KBF_TEST_POSTGRES_URL` names (CI's `test` and
//! `coverage` jobs have one; a superuser, to end a backend) and fail, not skip, without it.
//! The database calls block, so they are made outside the async runtime a server runs on.

use std::fs;
use std::process::Stdio;
use std::time::Duration;

use js_lease_server::LockLost;
use js_state::postgres::{TestDatabase, test_database};
use rustix::process::{Pid, Signal, kill_process};
use tokio::runtime::Runtime;

use crate::binary::launch;
use crate::store_support::{
    LIMIT, assert_unquoted, end_the_holder, holder, launch_ticking, revision, run_to_end,
    store_args, taken_address, with_userinfo,
};
use crate::support::{Harness, Scratch, TOKEN, lease_body, replace, request};

/// A role no test server has and a password for it: neither may ever be printed.
const ROLE: &str = "js_store_no_such_role";
const PASSWORD: &str = "store-secret-0123456789";

/// Asserts that a second server on a Postgres store another server holds exits 2 with the
/// one line naming the writer lock, and that it took the lock before anything else: it
/// bound no listener (its listen address is taken, which would have been refused naming
/// `--listen`), made no lease file and printed no address; the store's revision is
/// unchanged, and the first server still holds the lock and serves. It catches a lock that
/// is not taken, taken after the listener or the lease file, or released early.
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
        "the second server made no lease file"
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
/// the next tick stops the server: `lock_lost` names the loss, the tick neither ticks the
/// lease table (a lease whose time ran out is still in the lease file) nor anything else,
/// the shutdown runs no final tick (the lease file is unchanged) and answers the loss as an
/// error, and the store's revision is unchanged. A tick before the loss runs as usual. It
/// catches a server that keeps serving and writing after its session, and its lock, ended.
#[test]
fn losing_the_writer_lock_stops_the_server() {
    let database = test_database();
    let url = database.url();
    let scratch = Scratch::new("store_pg_lost");
    let args = store_args(&scratch, &url, &[]);
    let runtime = Runtime::new().unwrap();
    let h = runtime.block_on(Harness::with(scratch, args, None));
    runtime.block_on(async {
        let body = lease_body().to_string().into_bytes();
        let made = request(h.addr(), "POST", "/v1/leases", Some(TOKEN), Some(&body)).await;
        assert_eq!(made.status, 201, "{}", made.body);
        h.ticks.step().await;
    });
    let before = revision(&url);
    // Past the lease's default ttl (600 s): a tick of the table would now expire it.
    h.clock.advance(Duration::from_secs(601));
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
    let leases = fs::read_to_string(h.scratch.state()).unwrap();
    assert!(
        leases.contains("lease_count = 1"),
        "the lost tick expired nothing: {leases}"
    );

    let Harness {
        running, scratch, ..
    } = h;
    let stopped = runtime.block_on(running.shutdown());
    assert_eq!(stopped.err().map(|err| err.to_string()), Some(said));
    let after = fs::read_to_string(scratch.state()).unwrap();
    assert_eq!(after, leases, "no final tick wrote the lease file");
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

/// Asserts that a Postgres store without the state schema is refused at start naming
/// `--store`, saying the store could not be opened, after the lock was taken (the refusal
/// releases it), with no part of the URL. It catches a server that runs on a database it
/// cannot keep its state in, or that creates a schema of its own.
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
    assert_eq!(holder(&url), None, "the refusal released the lock");
}
