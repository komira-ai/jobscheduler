//! The account module's privileged macOS calls, run for real: no fake host and no fake
//! runner. Each test makes the call through the code the pool uses ([`MacHost`],
//! [`AsAccount`]) and reads the outcome back from the system.
//!
//! They need root on macOS, so the ordinary test run skips them by name (each is `#[ignore]`d,
//! with the reason) and the macOS job of `beta.yml` runs exactly them under `sudo`:
//!
//! ```text
//! sudo -E env "PATH=$PATH" "HOME=$HOME" cargo test -p js-agent --lib --locked -- --ignored --test-threads=1 mac_accounts::macos::root_tests
//! ```
//!
//! What they touch is theirs alone: a uid of [`TEST_UIDS`] that no record and no process has,
//! and a record name no account of a pool would take; each test deletes what it made.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use js_mac_session::disclaim::Program;
use js_testkit::dir::TestDir;

use super::super::cycle::Runner;
use super::super::host::{AccountHost, NewUser};
use super::super::{Account, STAFF, generated_uid};
use super::{AsAccount, MacHost};

/// The uids a test may take: high in the range a pool may be given, so that no pool and no
/// person has them on a runner.
const TEST_UIDS: std::ops::RangeInclusive<u32> = 59_900..=59_999;

/// Fails the test unless it runs as root: these are never run as anyone else.
fn as_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "the account module's root tests run as root (beta.yml runs them under sudo)"
    );
}

/// A uid of [`TEST_UIDS`] that no record has and no process runs as.
fn free_uid(host: &MacHost) -> u32 {
    TEST_UIDS
        .clone()
        .find(|uid| {
            host.uid_taken(*uid).is_ok_and(|taken| !taken)
                && host.processes(*uid).is_ok_and(|found| found.is_empty())
        })
        .expect("a uid of 59900-59999 that nothing uses")
}

/// The record name `n` of this test run's own: the pool's prefix with a number no pool
/// reaches.
fn fresh_name(n: u32) -> String {
    format!("kbf-acct-{}{n}", 9_000_000 + std::process::id())
}

/// What `dscl` reads of the record `name`'s uid and `GeneratedUID`: `None` when it finds no
/// such record.
fn dscl_read(host: &MacHost, name: &str) -> io::Result<Option<String>> {
    let path = format!("/Users/{name}");
    let read = host.command(
        "/usr/bin/dscl",
        &[".", "-read", &path, "UniqueID", "GeneratedUID"],
    )?;
    Ok(read.success.then_some(read.stdout))
}

/// Asserts that a record made through `MacHost` is in the directory with its uid and the
/// slot's `GeneratedUID`, as the directory itself and `dscl` read it, and that its deletion
/// through `MacHost` leaves no record by its name or uid, as both read it again.
#[test]
#[ignore = "needs root on macOS: beta.yml's macOS job runs it under sudo"]
fn a_record_is_made_in_the_directory_and_deleted_from_it() {
    as_root();
    let host = MacHost;
    let uid = free_uid(&host);
    let name = fresh_name(1);
    let user = NewUser {
        name: name.clone(),
        uid,
        gid: STAFF,
        home: PathBuf::from("/var/empty"),
        generated_uid: generated_uid(uid),
    };
    host.create_account(&user).unwrap();
    let made = (
        host.in_directory(&name).unwrap(),
        host.uid_in_directory(uid).unwrap(),
        host.record_guid(&name).unwrap(),
        dscl_read(&host, &name).unwrap(),
    );
    // Deleted before anything is asserted, so a failing assertion leaves no record behind.
    let deleted = host.delete_account(&name).unwrap();
    let after = (
        host.in_directory(&name).unwrap(),
        host.uid_in_directory(uid).unwrap(),
        dscl_read(&host, &name).unwrap(),
    );
    let again = host.delete_account(&name).unwrap();

    let (named, by_uid, guid, read) = made;
    assert!(named);
    assert!(by_uid);
    assert_eq!(guid, Some(generated_uid(uid)));
    let read = read.unwrap_or_default();
    assert!(read.contains(&format!("UniqueID: {uid}")));
    assert!(read.contains(&format!("GeneratedUID: {}", generated_uid(uid))));
    assert!(deleted);
    assert_eq!(after, (false, false, None));
    assert!(!again, "a second deletion finds no record");
}

/// Asserts that `kill_all` ends every process of a uid: three processes started as a free uid
/// are listed by `MacHost::processes`, each ends by `SIGKILL` once the uid is killed, and no
/// process of the uid is left.
#[test]
#[ignore = "needs root on macOS: beta.yml's macOS job runs it under sudo"]
fn every_process_of_a_uid_is_ended_by_kill_all() {
    as_root();
    let host = MacHost;
    let uid = free_uid(&host);
    let mut children: Vec<std::process::Child> = (0..3)
        .map(|_| {
            Program::new("/bin/sleep", ["600"])
                .uid(uid)
                .gid(STAFF)
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut started: Vec<i32> = children
        .iter()
        .map(|child| i32::try_from(child.id()).unwrap())
        .collect();
    started.sort_unstable();
    let mut listed: Vec<i32> = host
        .processes(uid)
        .unwrap()
        .iter()
        .map(|process| process.pid)
        .collect();
    listed.sort_unstable();
    host.kill_all(uid).unwrap();
    let signals: Vec<Option<i32>> = children
        .iter_mut()
        .map(|child| child.wait().unwrap().signal())
        .collect();
    let left = host.processes(uid).unwrap();

    assert_eq!(listed, started);
    assert_eq!(signals, [Some(libc::SIGKILL); 3]);
    assert!(left.is_empty());
}

/// A folder the account may enter, for its working directory and `HOME`.
fn open_folder(name: &str) -> TestDir {
    let dir = TestDir::under(Path::new("/private/tmp"), name);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Asserts that `AsAccount` runs a program as the account and nothing more: the program reads
/// back the account's uid as its real and effective uid, the account's group as its group,
/// and that group as the only one it holds; it exits 0.
#[test]
#[ignore = "needs root on macOS: beta.yml's macOS job runs it under sudo"]
fn a_program_runs_as_the_account_with_its_ids_and_no_other_group() {
    as_root();
    let host = MacHost;
    let uid = free_uid(&host);
    let home = open_folder("kbf-mac-accounts-as-account");
    let account = Account {
        name: fresh_name(2),
        uid,
        gid: STAFF,
        generated_uid: generated_uid(uid),
    };
    let ran = AsAccount
        .run(
            &account,
            home.path(),
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "/usr/bin/id -u; /usr/bin/id -ru; /usr/bin/id -g; /usr/bin/id -G".to_owned(),
            ],
        )
        .unwrap();
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.stdout, format!("{uid}\n{uid}\n{STAFF}\n{STAFF}\n"));
}
