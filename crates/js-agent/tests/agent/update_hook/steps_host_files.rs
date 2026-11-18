//! The host certificate and key a first install may find named in the host's flags file in
//! place of a join token, looked at through the hook's token port.

use std::fs;

use js_agent::update_hook::step::TokenFacts;
use js_agent::update_hook::{Class, Outcome, Reason};
use js_client::HostIdentity;

use super::step_rig::{
    HOST_CERT_PATH, HOST_KEY_PATH, RETRY_AFTER_SECS, Rig, Spy, TokenFile, euid, good_token,
};

/// A spy over `rig` whose flags file names the host certificate and key.
fn naming_the_host_files(rig: &Rig) -> Spy<'_> {
    let mut spy = Spy::new(rig);
    spy.host_identity = Some(HostIdentity::new(HOST_CERT_PATH, HOST_KEY_PATH));
    spy
}

/// The flags file of the rig as the message of a deferral names it.
fn flags(rig: &Rig) -> String {
    rig.params.flags_file.display().to_string()
}

/// A key file as hostkeeper writes it: regular, mode 0640, root's.
fn good_key() -> TokenFile {
    key(0o640, 0)
}

/// A key file with `mode` and owner `uid`.
fn key(mode: u32, uid: u32) -> TokenFile {
    TokenFile::Facts(TokenFacts {
        mode,
        uid,
        ..good_token()
    })
}

/// A certificate file: public, so its mode and owner are not looked at.
fn cert() -> TokenFile {
    TokenFile::Facts(TokenFacts {
        mode: 0o644,
        uid: 0,
        ..good_token()
    })
}

/// Asserts that a first install whose flags file names the host files, which show `cert` and
/// `key` and whose token file is good, defers with exactly `problem` after the flags file's
/// name, with the retry the params ask for, and changes nothing.
#[track_caller]
fn deferred_for_the_host_files(name: &str, cert: TokenFile, key: TokenFile, problem: &str) {
    let rig = Rig::first_install(name);
    let spy = naming_the_host_files(&rig);
    spy.identity_cert.set(cert);
    spy.identity_key.set(key);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{name}: {done:?}");
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing), "{name}");
    assert_eq!(done.report.class, Class::Environment, "{name}");
    assert_eq!(
        done.report.retry_after_secs,
        Some(RETRY_AFTER_SECS),
        "{name}"
    );
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the host certificate and key named in {}: {problem}; fix it to install the agent",
            flags(&rig)
        )),
        "{name}"
    );
    assert_eq!(rig.record_bytes(), None, "{name}: nothing is written");
    assert_eq!(rig.cordon_expiry(), None, "{name}");
}

/// Asserts that a first install whose flags file names the host files, the key one the agent
/// may use (mode 0600, 0400, 0640 or 0440, owned by root or by the hook's user), goes on with
/// no token file at all: no `join_token_missing`, and the record is written.
#[test]
fn a_first_install_with_the_host_files_in_the_flags_file_and_no_token_file_goes_on() {
    for (mode, uid) in [
        (0o600, euid()),
        (0o400, euid()),
        (0o640, 0),
        (0o440, 0),
        (0o640, euid()),
    ] {
        let rig = Rig::first_install(&format!("hook-host-files-good-{mode:o}-{uid}"));
        let spy = naming_the_host_files(&rig);
        spy.token.set(TokenFile::Missing);
        spy.identity_cert.set(cert());
        spy.identity_key.set(key(mode, uid));
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{mode:o} {uid}: {done:?}");
        assert!(rig.record().is_some(), "{mode:o} {uid}");
    }
}

/// Asserts that with host files named the token file is not looked at: a token file that is
/// wrong in every way does not defer an install whose host files are good.
#[test]
fn a_first_install_with_the_host_files_does_not_look_at_the_token_file() {
    let rig = Rig::first_install("hook-host-files-ignore-token");
    let spy = naming_the_host_files(&rig);
    spy.token.set(TokenFile::Facts(TokenFacts {
        mode: 0o666,
        uid: euid() + 1,
        regular: false,
    }));
    spy.identity_cert.set(cert());
    spy.identity_key.set(good_key());
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}

/// Asserts that a key the others can reach or its group can write (0644, 0660, 0604, 0666)
/// defers naming the key file and its mode, even when the token file is good: the agent would
/// refuse to start on it, so the token file does not stand in for it.
#[test]
fn a_first_install_with_a_key_that_others_can_read_or_its_group_can_write_defers() {
    for mode in [0o644, 0o604, 0o666] {
        deferred_for_the_host_files(
            &format!("hook-host-files-key-mode-{mode:o}"),
            cert(),
            key(mode, 0),
            &format!(
                "{HOST_KEY_PATH} must give others no access (mode 0600 or 0640), not mode {mode:04o}"
            ),
        );
    }
    deferred_for_the_host_files(
        "hook-host-files-key-mode-660",
        cert(),
        key(0o660, 0),
        &format!(
            "{HOST_KEY_PATH} must not be writable by its group (mode 0600 or 0640), not mode 0660"
        ),
    );
}

/// Asserts that a key owned by a user that is neither root nor the hook's user defers naming
/// both allowed owners, and that a key that is not a regular file defers saying so.
#[test]
fn a_first_install_with_a_key_of_another_owner_or_not_a_regular_file_defers() {
    deferred_for_the_host_files(
        "hook-host-files-key-owner",
        cert(),
        key(0o640, euid() + 1),
        &format!(
            "{HOST_KEY_PATH} must be owned by root or by uid {}, not by uid {}",
            euid(),
            euid() + 1
        ),
    );
    deferred_for_the_host_files(
        "hook-host-files-key-not-regular",
        cert(),
        TokenFile::Facts(TokenFacts {
            regular: false,
            ..good_token()
        }),
        &format!("{HOST_KEY_PATH} is not a regular file"),
    );
}

/// Asserts that a certificate or key file that is not there, or that cannot be looked at
/// (permission denied), or a certificate that is not a regular file, defers naming the file
/// and the reason; the certificate is looked at first.
#[test]
fn a_first_install_with_a_host_file_that_is_missing_or_cannot_be_read_defers() {
    deferred_for_the_host_files(
        "hook-host-files-cert-missing",
        TokenFile::Missing,
        good_key(),
        &format!("{HOST_CERT_PATH} is not there"),
    );
    deferred_for_the_host_files(
        "hook-host-files-key-missing",
        cert(),
        TokenFile::Missing,
        &format!("{HOST_KEY_PATH} is not there"),
    );
    deferred_for_the_host_files(
        "hook-host-files-cert-denied",
        TokenFile::Unreadable,
        good_key(),
        &format!("{HOST_CERT_PATH} cannot be read: permission denied"),
    );
    deferred_for_the_host_files(
        "hook-host-files-key-denied",
        cert(),
        TokenFile::Unreadable,
        &format!("{HOST_KEY_PATH} cannot be read: permission denied"),
    );
    deferred_for_the_host_files(
        "hook-host-files-cert-not-regular",
        TokenFile::Facts(TokenFacts {
            regular: false,
            ..good_token()
        }),
        good_key(),
        &format!("{HOST_CERT_PATH} is not a regular file"),
    );
    deferred_for_the_host_files(
        "hook-host-files-both-missing",
        TokenFile::Missing,
        TokenFile::Missing,
        &format!("{HOST_CERT_PATH} is not there"),
    );
}

/// Asserts that a host credential in the state root does not stand in for host files that are
/// named and wrong: the agent presents the certificate first and would stop on a bad key.
#[test]
fn host_files_that_are_wrong_defer_even_beside_a_host_credential() {
    let rig = Rig::first_install("hook-host-files-credential");
    let spy = naming_the_host_files(&rig);
    drop(rig.state_root());
    let root = rig.tmp.path().join("world/state");
    fs::write(root.join("host-credential"), "cred-0123456789abcdefghij").unwrap();
    spy.identity_cert.set(cert());
    spy.identity_key.set(key(0o644, 0));
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Deferred);
    spy.identity_key.set(good_key());
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}

/// Asserts that a flags file that names no host files leaves the first-install check as it
/// was: the join token file is looked at and its problem is told as before, and the host files
/// the fakes would show are not looked at.
#[test]
fn a_first_install_whose_flags_file_names_no_host_files_still_needs_the_token() {
    let rig = Rig::first_install("hook-host-files-none-named");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    spy.identity_cert.set(cert());
    spy.identity_key.set(good_key());
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the join token file {} is not there; place it, root-owned and mode 0600, to install the agent",
            rig.params.token_file.display()
        ))
    );
}

/// Asserts that an update never looks at the host files or the token file: its host has an
/// agent that runs.
#[test]
fn an_update_does_not_look_at_the_host_files() {
    let rig = Rig::new("hook-host-files-update");
    let spy = naming_the_host_files(&rig);
    spy.token.set(TokenFile::Missing);
    spy.identity_cert.set(TokenFile::Missing);
    spy.identity_key.set(TokenFile::Missing);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}
