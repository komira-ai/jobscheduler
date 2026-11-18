//! The service identity a first install may find in the state root in place of a join token:
//! the files hostkeeper writes there, looked at through the hook's token port.

use std::fs;

use js_agent::update_hook::step::TokenFacts;
use js_agent::update_hook::{Class, Outcome, Reason};

use super::step_rig::{RETRY_AFTER_SECS, Rig, Spy, TokenFile, euid, good_token};

/// The state root of the rig as the message of a deferral names it.
fn root(rig: &Rig) -> String {
    rig.params.state_root.display().to_string()
}

/// Asserts that a first install whose identity files show `cert` and `key`, and whose token
/// file is good, defers with exactly `detail` and changes nothing.
#[track_caller]
fn deferred_for_the_identity(
    name: &str,
    cert: TokenFile,
    key: TokenFile,
    detail: &dyn Fn(&Rig) -> String,
) {
    let rig = Rig::first_install(name);
    let spy = Spy::new(&rig);
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
    assert_eq!(done.report.detail, Some(detail(&rig)), "{name}");
    assert_eq!(rig.record_bytes(), None, "{name}: nothing is written");
    assert_eq!(rig.cordon_expiry(), None, "{name}");
}

/// A key file as hostkeeper writes it: regular, mode 0600, the hook's user's.
fn good_key() -> TokenFile {
    TokenFile::Facts(good_token())
}

/// A certificate file: public, so its mode and owner are not looked at.
fn cert() -> TokenFile {
    TokenFile::Facts(TokenFacts {
        mode: 0o644,
        ..good_token()
    })
}

/// Asserts that a first install whose state root holds both identity files, the key private
/// (mode 0600 or 0400, the hook's user's), goes on with no token file at all: no
/// `join_token_missing`, and the record is written.
#[test]
fn a_first_install_with_the_service_identity_and_no_token_file_goes_on() {
    for mode in [0o600, 0o400] {
        let rig = Rig::first_install(&format!("hook-identity-good-{mode:o}"));
        let spy = Spy::new(&rig);
        spy.token.set(TokenFile::Missing);
        spy.identity_cert.set(cert());
        spy.identity_key.set(TokenFile::Facts(TokenFacts {
            mode,
            ..good_token()
        }));
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "mode {mode:o}: {done:?}");
        assert!(rig.record().is_some(), "mode {mode:o}");
    }
}

/// Asserts that with neither identity file and no token file the deferral names both ways: the
/// token file and the service identity, with the files' names and the state root.
#[test]
fn a_first_install_with_no_identity_and_no_token_defers_naming_both_ways() {
    let rig = Rig::first_install("hook-identity-neither");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the join token file {} is not there; place it, root-owned and mode 0600, or have hostkeeper write the service identity (service-identity.pem and service-identity.key in {}), to install the agent",
            rig.params.token_file.display(),
            root(&rig)
        ))
    );
}

/// Asserts that a state root with one identity file only defers naming the file that is missing,
/// in both directions, even when the token file is good: the agent would refuse to start on half
/// an identity, so the token file does not stand in for it.
#[test]
fn a_first_install_with_one_identity_file_defers_naming_the_missing_one() {
    deferred_for_the_identity("hook-identity-no-key", cert(), TokenFile::Missing, &|rig| {
        format!(
            "the service identity is half there: {}/service-identity.key is missing; fix it or remove it",
            root(rig)
        )
    });
    deferred_for_the_identity(
        "hook-identity-no-cert",
        TokenFile::Missing,
        good_key(),
        &|rig| {
            format!(
                "the service identity is half there: {}/service-identity.pem is missing; fix it or remove it",
                root(rig)
            )
        },
    );
}

/// Asserts that a key that is not private defers, whatever the token file is, saying what is
/// wrong with it: a loose mode (several), another owner, not a regular file.
#[test]
fn a_first_install_with_a_key_that_is_not_private_defers() {
    for mode in [0o644, 0o640, 0o660, 0o700, 0o000] {
        deferred_for_the_identity(
            &format!("hook-identity-key-mode-{mode:o}"),
            cert(),
            TokenFile::Facts(TokenFacts {
                mode,
                ..good_token()
            }),
            &|rig| {
                format!(
                    "the service identity key {}/service-identity.key must be mode 0600 or 0400, not {mode:04o}; fix it or remove it",
                    root(rig)
                )
            },
        );
    }
    deferred_for_the_identity(
        "hook-identity-key-owner",
        cert(),
        TokenFile::Facts(TokenFacts {
            uid: euid() + 1,
            ..good_token()
        }),
        &|rig| {
            format!(
                "the service identity key {}/service-identity.key must be owned by uid {}, not {}; fix it or remove it",
                root(rig),
                euid(),
                euid() + 1
            )
        },
    );
    deferred_for_the_identity(
        "hook-identity-key-not-regular",
        cert(),
        TokenFile::Facts(TokenFacts {
            regular: false,
            ..good_token()
        }),
        &|rig| {
            format!(
                "the service identity key {}/service-identity.key is not a regular file; fix it or remove it",
                root(rig)
            )
        },
    );
}

/// Asserts that an identity file that cannot be looked at (permission denied), the certificate
/// or the key, defers naming the file and the reason.
#[test]
fn a_first_install_with_an_identity_file_that_cannot_be_read_defers() {
    deferred_for_the_identity(
        "hook-identity-cert-denied",
        TokenFile::Unreadable,
        good_key(),
        &|rig| {
            format!(
                "the service identity file {}/service-identity.pem cannot be read: permission denied; fix it or remove it",
                root(rig)
            )
        },
    );
    deferred_for_the_identity(
        "hook-identity-key-denied",
        cert(),
        TokenFile::Unreadable,
        &|rig| {
            format!(
                "the service identity file {}/service-identity.key cannot be read: permission denied; fix it or remove it",
                root(rig)
            )
        },
    );
}

/// Asserts that the agent's own order is followed: the identity comes before a host credential,
/// so a state root with a host credential and half an identity defers (the agent would stop on
/// it), and a state root with a host credential and no identity goes on as it did.
#[test]
fn half_an_identity_defers_even_beside_a_host_credential() {
    let rig = Rig::first_install("hook-identity-credential");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    let state = rig.state_root();
    drop(state);
    let root = rig.tmp.path().join("world/state");
    fs::write(root.join("host-credential"), "cred-0123456789abcdefghij").unwrap();
    spy.identity_cert.set(cert());
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Deferred);
    spy.identity_cert.set(TokenFile::Missing);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// Asserts that an update never looks at the identity files or the token file: its host has an
/// agent that runs.
#[test]
fn an_update_does_not_look_at_the_identity_files() {
    let rig = Rig::new("hook-identity-update");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    spy.identity_cert.set(cert());
    spy.identity_key.set(TokenFile::Missing);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}
