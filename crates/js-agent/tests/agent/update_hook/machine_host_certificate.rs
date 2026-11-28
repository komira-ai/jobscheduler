//! The hook on a machine and the host's certificate and key (`--host-cert` and `--host-key`)
//! its flags file names: its lease connection presents them, a first install checks them, and
//! a flags file that names none defers the install.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_agent::own_host::{Lease, LeaseError};
use js_agent::update_hook::{ConnectParams, MachineLease, Outcome, Reason, Step};
use js_client::HostIdentity;

use super::machine::Tree;
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

fn connect(identity: Option<HostIdentity>) -> MachineLease {
    MachineLease::connect(
        &ConnectParams {
            server: Some(NOBODY.to_owned()),
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

/// Asserts that a lease made with the host's certificate and key is built and reaches for the
/// server, and that one made with none says why it was not made, naming the flags file and both
/// flags. It catches a lease made with some other identity, and a missing certificate taken for
/// a connection.
#[test]
fn the_lease_presents_the_host_certificate_and_one_without_it_says_why_it_is_not_made() {
    let files = Scratch::new("hook-machine-host-cert");
    let identity = host_files(&files, 0o640);
    let said = why(connect(Some(identity)).host());
    assert!(
        said.starts_with(&format!("cannot talk to {NOBODY}")),
        "{said}"
    );
    let said = why(connect(None).host());
    assert_eq!(
        said,
        "flags_file: the flags file names no --host-cert and --host-key: a host is identified by \
         its certificate"
    );
}

/// Asserts that a key that others can read is refused with the client's own text, which names
/// the flag of the flags file and the file, and holds no content of the key.
#[test]
fn a_host_key_that_others_can_read_is_refused_naming_the_flag_and_the_file() {
    let files = Scratch::new("hook-machine-loose-host-key");
    let identity = host_files(&files, 0o644);
    let said = why(connect(Some(identity.clone())).host());
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
/// when the key is one the agent may use (read as the machine reads it, through the link a
/// renewal repoints).
#[test]
fn a_first_install_on_the_machine_goes_on_with_the_host_files() {
    let tree = Tree::new("hook-machine-first-host-files");
    let identity = host_files(&tree.files, 0o640);
    let link = tree.files.path("host-key-link");
    std::os::unix::fs::symlink(identity.key_path(), &link).unwrap();
    let named = HostIdentity::new(identity.cert_path(), &link);
    let params = tree.params(false, &naming_host_files(&named));
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
/// open to others, or names one of the two host flags only): it defers, with the reason
/// `join_token_missing` and the same detail naming the flags file and the two flags, since a
/// host that has no usable flags file has no identity to install with; the agent itself refuses
/// such a flags file when it starts. With the flags file made right the next attempt goes on.
#[test]
fn a_first_install_with_a_flags_file_that_cannot_be_used_defers_naming_the_flags() {
    let tree = Tree::new("hook-machine-first-flags-unusable");
    let identity = host_files(&tree.files, 0o640);
    let params = tree.params(false, &naming_host_files(&identity));
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
                "the flags file {} names no --host-cert and --host-key: a host is identified by \
                 its certificate; name them to install the agent",
                params.flags_file.display()
            )),
            "{what}"
        );
        assert!(!tree.record().exists(), "{what}");
    }
    fs::write(&params.flags_file, &flags_text).unwrap();
    fs::set_permissions(&params.flags_file, fs::Permissions::from_mode(0o600)).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// Asserts that a first install whose flags file names the host files and takes the host id
/// from a facts file that is not there still checks the host files: it goes on when the key is
/// one the agent may use, and defers naming the key when its mode is loose.
#[test]
fn a_first_install_checks_the_host_files_when_the_host_id_cannot_be_resolved() {
    let tree = Tree::new("hook-machine-first-host-files-no-facts");
    let identity = host_files(&tree.files, 0o644);
    let facts = tree.files.path("no-such-facts.json");
    let named = format!(
        "{}host_id_from_facts = \"{}\"\n",
        naming_host_files(&identity),
        facts.display()
    );
    let params = tree.params(false, &named);
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
