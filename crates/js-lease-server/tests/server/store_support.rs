//! What the store tests share: the store file, the program run to its end, and, for the
//! Postgres tests, the backend that holds the writer lock.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use js_state::postgres::{PostgresConfig, PostgresStore, WRITER_LOCK_KEY};
use js_state::{Revision, Store};
use postgres::{Client, NoTls};
use rustix::process::{Pid, Signal, kill_process};

use crate::binary::Guard;
use crate::support::{Scratch, base_args};

pub const PROGRAM: &str = env!("CARGO_BIN_EXE_js-lease-server");

/// How long a test waits for the program to print its address or to end before it gives
/// up. Only a hang reaches it: a program that works prints, or ends, at once.
pub const LIMIT: Duration = Duration::from_secs(60);

/// The owner-only store file `store` of `scratch`, holding `line`.
pub fn store_file(scratch: &Scratch, line: &str) -> PathBuf {
    scratch.write("store", &format!("{line}\n"), 0o600)
}

/// The line of a SQLite store at `state.db` of `scratch`.
pub fn sqlite_line(scratch: &Scratch) -> String {
    format!("sqlite:{}", sqlite_db(scratch).display())
}

/// The SQLite store's file of `scratch`.
pub fn sqlite_db(scratch: &Scratch) -> PathBuf {
    scratch.path("state.db")
}

/// The base flags with `--store` naming a file that holds `line`, and `extra` after them.
pub fn store_args(scratch: &Scratch, line: &str, extra: &[&str]) -> Vec<OsString> {
    let mut args = base_args(scratch);
    args.push("--store".into());
    args.push(store_file(scratch, line).into_os_string());
    args.extend(extra.iter().map(OsString::from));
    args
}

/// The base flags of `scratch` over a SQLite store of `store`'s scratch directory, with the
/// dev flag: a second server's flags over the first one's store.
pub fn sqlite_args_over(scratch: &Scratch, store: &Scratch) -> Vec<OsString> {
    store_args(scratch, &sqlite_line(store), &["--allow-sqlite-store"])
}

/// What the program printed and how it ended.
pub struct Ended {
    /// The exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the program on `args` (the program name first) to its end. A program still running
/// after [`LIMIT`] is killed and the test fails: only a start that was not refused, or a
/// server that did not stop, runs that long.
pub fn run_to_end(args: &[OsString]) -> Ended {
    let child = Command::new(PROGRAM)
        .args(&args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = Pid::from_child(&child);
    let (done, ended) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let output = ended.recv_timeout(LIMIT);
    if output.is_err() {
        // The wait has not returned, so the child is not reaped and the pid is still its own.
        let _ = kill_process(pid, Signal::KILL);
    }
    let output = output.expect("the program ended").unwrap();
    Ended {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// Starts the program on `args` with ticks every `tick`, standard error piped, and returns
/// it with the address it printed.
pub fn launch_ticking(args: &[OsString], tick: &str) -> (Guard, SocketAddr) {
    let mut child = Guard(
        Command::new(PROGRAM)
            .args(&args[1..])
            .args(["--tick", tick])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (first, line) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stdout).read_line(&mut text);
        let _ = first.send(text);
    });
    let line = line.recv_timeout(LIMIT).unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap()
        .parse()
        .unwrap();
    (child, addr)
}

/// A loopback address that is taken while the listener lives: a server told to bind it
/// would be refused naming its listen flag.
pub fn taken_address() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
}

/// `url` with `userinfo` (a role and a password) in place of its own.
pub fn with_userinfo(url: &str, userinfo: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap();
    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    format!("{scheme}://{userinfo}@{host}")
}

/// The password of `url`, when its user information carries one.
pub fn password_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let (info, _) = rest.rsplit_once('@')?;
    info.split_once(':')
        .map(|(_, password)| password.to_owned())
}

/// Asserts that `printed` holds neither `url` nor its password.
pub fn assert_unquoted(url: &str, printed: &str) {
    assert!(!printed.contains(url), "the URL in {printed}");
    let password = password_of(url).unwrap_or_else(|| url.to_owned());
    assert!(!printed.contains(&password), "the password in {printed}");
}

/// The backend that holds the writer lock in the database `url` names, read from
/// `pg_locks` (a single-`bigint` advisory key is split into `classid`, its high 32 bits, and
/// `objid`, its low 32 bits, with `objsubid` 1). Call it outside an async runtime.
pub fn holder(url: &str) -> Option<i32> {
    let mut admin = Client::connect(url, NoTls).unwrap();
    admin
        .query_opt(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND objsubid = 1 \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
             AND ((classid::bigint << 32) | objid::bigint) = $1",
            &[&WRITER_LOCK_KEY],
        )
        .unwrap()
        .map(|row| row.get(0))
}

/// Ends the session that holds the writer lock in the database `url` names, and returns
/// once its backend has exited (so the lock is gone on the server). Call it outside an async
/// runtime.
pub fn end_the_holder(url: &str) {
    let pid = holder(url).expect("a session holds the writer lock");
    let mut admin = Client::connect(url, NoTls).unwrap();
    let ended: bool = admin
        .query_one("SELECT pg_terminate_backend($1, 60000)", &[&pid])
        .unwrap()
        .get(0);
    assert!(ended, "the holder's backend ended");
    assert_eq!(holder(url), None, "the lock is free");
}

/// How far the store in the database `url` has been written: the cursor after every change
/// in its feed, read on a store of the test's own (which takes no writer lock).
pub fn revision(url: &str) -> Revision {
    let store = PostgresStore::open(&PostgresConfig::parse(url).unwrap()).unwrap();
    store.changes(Revision::ZERO, 100_000).unwrap().cursor
}
