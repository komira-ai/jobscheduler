//! The hook on a machine when the host's flags file names the host's certificate and key
//! (`--host-cert` and `--host-key`): its lease connection presents them and needs no token or
//! credential, and a first install needs no join token.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_agent::own_host::{Lease, LeaseError};
use js_agent::update_hook::{ConnectParams, MachineLease, Outcome, Reason, Step};
use js_client::HostIdentity;

use super::machine::Tree;
use crate::jobs::private;
use crate::support::Scratch;

/// A server that is not listening: a connection made with a valid login gets as far as the
/// refused connection, which is how these tests tell a login was built.
const NOBODY: &str = "https://127.0.0.1:1";

/// A certificate and key minted for the test, in `files`, the key with `mode`.
fn host_files(files: &Scratch, mode: u32) -> HostIdentity {
    let made = rcgen::generate_simple_self_signed(vec!["host-1".to_owned()]).unwrap();
    let cert = files.write("host.pem", &made.cert.pem(), 0o644);
    let key = files.write("host.key", &made.signing_key.serialize_pem(), mode);
    HostIdentity::new(cert, key)
}

fn connect(identity: Option<HostIdentity>, state: Option<PathBuf>) -> MachineLease {
    MachineLease::connect(
        &ConnectParams {
            server: Some(NOBODY.to_owned()),
            state_root: state,
            token_file: None,
            ca_cert: None,
            host_id: Some("host-1".to_owned()),
            host_identity: identity,
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

/// Asserts that a lease made with the host's certificate and key needs no token file and no
/// state root (it is built and reaches for the server), and that a state root beside it whose
/// credential file is not a credential is not read: the certificate is used first.
#[test]
fn the_host_certificate_is_used_first_and_needs_no_token_file_or_credential() {
    let files = Scratch::new("hook-machine-host-cert");
    let identity = host_files(&files, 0o640);
    let state = private(&files, "state");
    fs::write(state.join("host-credential"), "not a credential\n").unwrap();
    fs::set_permissions(
        state.join("host-credential"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    for state in [None, Some(state)] {
        let said = why(connect(Some(identity.clone()), state).host());
        assert!(
            said.starts_with(&format!("cannot talk to {NOBODY}")),
            "{said}"
        );
    }
}

/// Asserts that a key that others can read is refused with the client's own text, which names
/// the flag of the flags file and the file, and holds no content of the key.
#[test]
fn a_host_key_that_others_can_read_is_refused_naming_the_flag_and_the_file() {
    let files = Scratch::new("hook-machine-loose-host-key");
    let identity = host_files(&files, 0o644);
    let said = why(connect(Some(identity.clone()), None).host());
    assert_eq!(
        said,
        format!(
            "--host-key: {} must give others no access (mode 0600 or 0640), not mode 0644",
            identity.key_path().display()
        )
    );
}

/// The flags file lines of a first install that names the lease server and the host files.
fn naming_host_files(identity: &HostIdentity) -> String {
    format!(
        "server = \"https://leases.example:7300\"\nhost_cert = \"{}\"\nhost_key = \"{}\"\n",
        identity.cert_path().display(),
        identity.key_path().display()
    )
}

/// Asserts that a first install on the machine whose flags file names the host files goes on
/// with no join token file when the key is one the agent may use (read as the machine reads it,
/// through the link a renewal repoints), and that a missing token file does not matter.
#[test]
fn a_first_install_on_the_machine_goes_on_with_the_host_files_and_no_token_file() {
    let tree = Tree::new("hook-machine-first-host-files");
    let identity = host_files(&tree.files, 0o640);
    let link = tree.files.path("host-key-link");
    std::os::unix::fs::symlink(identity.key_path(), &link).unwrap();
    let named = HostIdentity::new(identity.cert_path(), &link);
    let params = tree.params(false, &naming_host_files(&named));
    fs::remove_file(&params.token_file).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(tree.record().exists());
}

/// Asserts that a first install on the machine whose flags file names a key that others can
/// read is deferred, with the reason `join_token_missing`, naming the flags file and the key, and
/// changes nothing; fixing the mode lets the next attempt go on.
#[test]
fn a_first_install_on_the_machine_defers_for_a_host_key_others_can_read() {
    let tree = Tree::new("hook-machine-first-loose-host-key");
    let identity = host_files(&tree.files, 0o644);
    let params = tree.params(false, &naming_host_files(&identity));
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the host certificate and key named in {}: {} must give others no access (mode 0600 or 0640), not mode 0644; fix it to install the agent",
            params.flags_file.display(),
            identity.key_path().display()
        ))
    );
    assert!(!tree.record().exists());
    fs::set_permissions(identity.key_path(), fs::Permissions::from_mode(0o640)).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// Asserts what a first install does when the flags file cannot be used (it is not there, is
/// open to others, or names one of the two host flags only): it keeps the join token check
/// it had, so the install goes on with a good token file and is deferred, naming the token
/// file, without one; the agent itself refuses such a flags file when it starts.
#[test]
fn a_first_install_with_a_flags_file_that_cannot_be_used_keeps_the_join_token_check() {
    let tree = Tree::new("hook-machine-first-flags-unusable");
    let identity = host_files(&tree.files, 0o640);
    let params = tree.params(false, &naming_host_files(&identity));
    fs::remove_file(&params.token_file).unwrap();
    let flags_text = fs::read_to_string(&params.flags_file).unwrap();
    let lone_cert: String = flags_text
        .lines()
        .filter(|line| !line.starts_with("--host-key"))
        .map(|line| format!("{line}\n"))
        .collect();
    let cases: [(&str, Option<&str>, u32); 3] = [
        ("missing", None, 0o600),
        ("open to others", Some(&flags_text), 0o666),
        ("one host flag only", Some(&lone_cert), 0o600),
    ];
    for (what, text, mode) in cases {
        match text {
            Some(text) => {
                fs::write(&params.flags_file, text).unwrap();
                fs::set_permissions(&params.flags_file, fs::Permissions::from_mode(mode)).unwrap();
            }
            None => fs::remove_file(&params.flags_file).unwrap(),
        }
        let done = tree.run(Step::Prepare, &params, Err("unused"));
        assert_eq!(done.outcome, Outcome::Deferred, "{what}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing), "{what}");
        assert_eq!(
            done.report.detail,
            Some(format!(
                "the join token file {} is not there; place it, root-owned and mode 0600, to install the agent",
                params.token_file.display()
            )),
            "{what}"
        );
        assert!(!tree.record().exists(), "{what}");
    }
    fs::write(&params.token_file, "tok-0123456789\n").unwrap();
    fs::set_permissions(&params.token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}
