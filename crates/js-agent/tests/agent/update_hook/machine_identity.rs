//! The hook's own lease connection when the agent's state root holds the service identity
//! hostkeeper writes: it connects with the identity first, and needs no token file.

use std::path::PathBuf;

use js_agent::own_host::{Lease, LeaseError};
use js_agent::update_hook::{ConnectParams, MachineLease};
use js_client::{SERVICE_CERT_FILE, SERVICE_KEY_FILE};

use crate::jobs::private;
use crate::support::Scratch;

/// A server that is not listening: a connection made with a valid login gets as far as the
/// refused connection, which is how these tests tell a login was built.
const NOBODY: &str = "https://127.0.0.1:1";

/// A state root holding a pair minted for the test, the key with `mode`.
fn state_with_identity(files: &Scratch, mode: u32) -> PathBuf {
    let state = private(files, "state");
    let made = rcgen::generate_simple_self_signed(vec!["host-1".to_owned()]).unwrap();
    files.write(
        &format!("state/{SERVICE_CERT_FILE}"),
        &made.cert.pem(),
        0o644,
    );
    files.write(
        &format!("state/{SERVICE_KEY_FILE}"),
        &made.signing_key.serialize_pem(),
        mode,
    );
    state
}

fn connect(state: PathBuf, token_file: Option<PathBuf>) -> MachineLease {
    MachineLease::connect(
        &ConnectParams {
            server: Some(NOBODY.to_owned()),
            state_root: Some(state),
            token_file,
            ca_cert: None,
            host_id: Some("host-1".to_owned()),
        },
        &|| Err("unused".to_owned()),
        js_agent::own_host::RequestLimit::DEFAULT,
    )
}

fn why(result: Result<impl std::fmt::Debug, LeaseError>) -> String {
    match result {
        Err(LeaseError::Unreachable(text)) => text,
        other => panic!("not an unreachable lease: {other:?}"),
    }
}

/// Asserts that a lease made from a state root holding the service identity needs no token file
/// (it is built and reaches for the server), and that a token file named beside it, which is not
/// there, is not looked at: the identity is used first.
#[test]
fn the_service_identity_of_the_state_root_is_used_first_and_needs_no_token_file() {
    let files = Scratch::new("hook-machine-identity");
    let state = state_with_identity(&files, 0o600);
    for token_file in [None, Some(files.path("no-such-token"))] {
        let said = why(connect(state.clone(), token_file).host());
        assert!(
            said.starts_with(&format!("cannot talk to {NOBODY}")),
            "{said}"
        );
    }
}

/// Asserts that a state root with one identity file only is refused by the params key
/// `state_root`, naming the missing file, and that the token file is not used in its place.
#[test]
fn half_a_service_identity_is_refused_naming_the_state_root_and_the_missing_file() {
    let files = Scratch::new("hook-machine-half-identity");
    let state = state_with_identity(&files, 0o600);
    std::fs::remove_file(state.join(SERVICE_CERT_FILE)).unwrap();
    let token = files.write("token", "tok-0123456789", 0o600);
    let said = why(connect(state.clone(), Some(token)).host());
    assert_eq!(
        said,
        format!(
            "state_root: {} is missing: the service identity needs both service-identity.pem and service-identity.key",
            state.join(SERVICE_CERT_FILE).display()
        )
    );
}

/// Asserts that a key of a loose mode is refused by the params key `state_root`, naming the
/// file and its mode, and that the text holds no content of the key.
#[test]
fn a_service_identity_key_that_is_not_private_is_refused_naming_the_state_root() {
    let files = Scratch::new("hook-machine-loose-identity");
    let state = state_with_identity(&files, 0o644);
    let said = why(connect(state.clone(), None).host());
    assert!(
        said.starts_with(&format!(
            "state_root: {} must be a regular file owned by you",
            state.join(SERVICE_KEY_FILE).display()
        )),
        "{said}"
    );
    assert!(!said.contains("PRIVATE"), "{said}");
}
