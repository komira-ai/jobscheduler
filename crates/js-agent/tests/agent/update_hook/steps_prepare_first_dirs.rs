//! `prepare` of a first install on a Mac that never had the agent: the state directory the
//! params name (`/Library/Application Support/js-agent/update`) and the directories above it
//! are not there, and its flags file names hostkeeper's host certificate and key.

use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent::update_hook::step::{self, TokenFacts};
use js_agent::update_hook::{DirProblem, DirStat, Outcome, Reason};
use js_client::HostIdentity;

use super::step_rig::{HOST_CERT_PATH, HOST_KEY_PATH, Rig, Spy, TokenFile, as_root, good_file};

/// What the staged Mac agent prints on `--version`.
const MAC_VERSION: &str = "js-agent 1.4.0\nruntimes: native\n";

/// A first install on a Mac whose state directory is `state_dir` under the rig's scratch tree,
/// none of whose directories below the tree is there.
fn mac_first_install(name: &str) -> (Rig, PathBuf) {
    let mut rig = Rig::mac_first_install_with_helper(name);
    let state_dir = rig
        .tmp
        .path()
        .join("Library/Application Support/js-agent/update");
    rig.params.settings.state_dir = state_dir.clone();
    (rig, state_dir)
}

/// A spy over `rig` for a Mac whose flags file names the host certificate and key as
/// hostkeeper keeps them (the key root's, mode 0640).
fn joining_by_host_certificate(rig: &Rig) -> Spy<'_> {
    let mut spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(MAC_VERSION.to_owned());
    spy.host_identity = Some(HostIdentity::new(HOST_CERT_PATH, HOST_KEY_PATH));
    spy.identity_cert.set(TokenFile::Facts(TokenFacts {
        mode: 0o644,
        uid: 0,
        ..good_file()
    }));
    spy.identity_key.set(TokenFile::Facts(TokenFacts {
        mode: 0o640,
        uid: 0,
        ..good_file()
    }));
    spy
}

/// The permission bits of the directory at `path`.
fn mode(path: &Path) -> u32 {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir(), "{}", path.display());
    meta.permissions().mode() & 0o7777
}

/// Asserts that the first `prepare` on a Mac where no directory of the state directory's path
/// is there, with the host certificate named, succeeds: it makes
/// `js-agent` and `js-agent/update` (0755 and 0700) and writes its record in the one it made.
/// Catches the first install that halted `state_invalid` on `/Library/Application
/// Support/js-agent: No such file or directory`.
#[test]
fn a_first_install_into_an_empty_root_makes_the_state_directory_and_succeeds() {
    let (rig, state_dir) = mac_first_install("hook-prepare-first-dirs");
    assert!(!rig.tmp.path().join("Library").exists());
    let spy = joining_by_host_certificate(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    let parent = state_dir.parent().unwrap();
    assert_eq!(mode(parent), 0o755);
    assert_eq!(mode(&state_dir), 0o700);
    assert!(state_dir.join("record.json").is_file());
    // The rerun hostkeeper may make finds what the first one made and goes on.
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}

/// Asserts that a state directory that is there and open to others is still refused
/// (`state_invalid`, naming it and its mode), not made over: nothing is written in it.
#[test]
fn a_first_install_still_refuses_a_state_directory_that_is_there_and_not_private() {
    let (rig, state_dir) = mac_first_install("hook-prepare-first-dirs-open");
    fs::create_dir_all(&state_dir).unwrap();
    let open = |path: &Path, fd: BorrowedFd<'_>| -> io::Result<DirStat> {
        if path == state_dir {
            Ok(DirStat {
                mode: 0o755,
                ..as_root(path, fd)?
            })
        } else {
            as_root(path, fd)
        }
    };
    let spy = joining_by_host_certificate(&rig);
    let done = step::prepare(&spy.env(&rig, &open), &rig.params);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "state directory {}: {}",
            state_dir.display(),
            DirProblem::Mode { mode: 0o755 }
        ))
    );
    assert_eq!(fs::read_dir(&state_dir).unwrap().count(), 0);
}
