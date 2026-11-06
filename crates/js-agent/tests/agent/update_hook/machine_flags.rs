//! The host's flags file and join token file as the hook on a machine looks at them: the flags
//! file is where the server, the certificate and the host id come from, and the token file is
//! looked at through links with its mode and owner.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_agent::update_hook::{Class, Machine, Outcome, Reason, Step};

use super::machine::Tree;
use crate::jobs::private;
use crate::support::Scratch;

/// The flags file is the one place the server, the certificate and the host id come from. One
/// that is missing, open to others, holds a flag the agent does not have or holds another flags
/// file is told as the lease's error, by the params key `flags_file`; the step that needs the
/// lease ends `lease_unreachable`, class environment, and nothing else is changed.
///
/// Catches a flags file read without the agent's rules (it names the server, and an open one
/// could be written by anyone), and a failure that is blamed on the release.
#[test]
fn a_flags_file_that_cannot_be_used_ends_an_update_as_a_lease_failure_naming_it() {
    let tree = Tree::new("hook-machine-bad-flags");
    let params = tree.params(true, &tree.state_line());
    let flags = tree.flags();
    let cases: [(&str, Option<&str>, u32, &str); 6] = [
        (
            "missing",
            None,
            0o600,
            "cannot be read: No such file or directory",
        ),
        (
            "open to others",
            Some("--server=https://leases.example:7300\n--pool=farm\n"),
            0o666,
            "writable by group or others (mode 0666)",
        ),
        (
            "a flag the agent does not have",
            Some("--server=https://leases.example:7300\n--pool=farm\n--bogus=1\n"),
            0o600,
            "--bogus",
        ),
        (
            "another flags file",
            Some("--server=https://leases.example:7300\n--pool=farm\n--flags-file=/etc/other\n"),
            0o600,
            "line 3: --flags-file may not appear in a flags file",
        ),
        (
            "not a flag",
            Some("server=x\n"),
            0o600,
            "line 1: not a flag",
        ),
        (
            "no server",
            Some("--pool=farm\n"),
            0o600,
            "required arguments were not provided",
        ),
    ];
    for (what, text, mode, said) in cases {
        match text {
            Some(text) => {
                fs::write(&flags, text).unwrap();
                fs::set_permissions(&flags, fs::Permissions::from_mode(mode)).unwrap();
            }
            None => fs::remove_file(&flags).unwrap(),
        }
        let done = tree.run(Step::Prepare, &params, Err("unused"));
        assert_eq!(done.outcome, Outcome::Failure, "{what}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable), "{what}");
        assert_eq!(done.report.class, Class::Environment, "{what}");
        let detail = done.report.detail.unwrap();
        assert!(detail.starts_with("flags_file: "), "{what}: {detail}");
        assert!(detail.contains(said), "{what}: {detail}");
        assert!(!tree.record().exists(), "{what}");
    }
}

/// A first install asks the lease server nothing, so it does not read the flags file: a host
/// whose flags the operator has not placed yet installs, and its agent waits for them.
///
/// Catches a first install held up by a file the agent itself would wait for.
#[test]
fn a_first_install_does_not_read_the_flags_file() {
    let tree = Tree::new("hook-machine-first-no-flags");
    let params = tree.params(false, "");
    fs::remove_file(tree.flags()).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

/// The machine looks at the join token file as the system has it: through a link (secret
/// mounts are links), with its permission bits and owner, and a directory is not a regular file.
///
/// Catches a look at the link and not at the file, which would take a good token for a link.
#[test]
fn the_machine_looks_at_the_token_file_through_links_with_its_mode_and_owner() {
    let files = Scratch::new("hook-machine-token-facts");
    let token = files.write("token", "tok-0123456789\n", 0o640);
    let link = files.path("token-link");
    std::os::unix::fs::symlink(&token, &link).unwrap();
    let dir = private(&files, "a-directory");
    let machine = Machine::system();
    for path in [&token, &link] {
        let facts = (machine.token)(path).unwrap();
        assert_eq!(
            (facts.regular, facts.mode, facts.uid),
            (true, 0o640, machine.euid)
        );
    }
    assert!(!(machine.token)(&dir).unwrap().regular);
    let missing = (machine.token)(&files.path("absent")).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

/// The machine's user is the one running the hook, and its flags file is read as the agent reads
/// one.
///
/// Catches a machine that trusts files of the wrong user or reads the flags file another way.
#[test]
fn the_machine_runs_as_its_user_and_reads_the_flags_file_as_the_agent_does() {
    let machine = Machine::system();
    assert_eq!(machine.euid, rustix::process::geteuid().as_raw());
    let files = Scratch::new("hook-machine-flags-facts");
    let file = files.write("flags", "--pool=farm\n", 0o600);
    let facts = (machine.flags)(&file).unwrap();
    assert_eq!(
        (facts.regular, facts.mode, facts.uid, facts.bytes.as_slice()),
        (true, 0o600, machine.euid, "--pool=farm\n".as_bytes())
    );
}

/// On the machine a first install finds no join token file in place, and the agent's state root
/// holds the host credential the agent sends instead: the install goes on. Without the
/// credential the same install is deferred for the token. The state root is looked up as the
/// agent's own is, so one that is not private is refused before any of this (machine_root.rs).
///
/// Catches a machine whose state root cannot say it holds a credential, which would hold up a
/// host that has joined for a token it no longer needs.
#[test]
fn a_first_install_on_the_machine_goes_on_with_a_host_credential_and_no_token_file() {
    let tree = Tree::new("hook-machine-first-credential");
    let state = private(&tree.files, "agent-state");
    let params = tree.params(false, &format!("state_root = \"{}\"\n", state.display()));
    fs::remove_file(&params.token_file).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing));
    assert!(!tree.record().exists());
    let credential = state.join("host-credential");
    fs::write(&credential, "cred-0123456789abcdefghij").unwrap();
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(tree.record().exists());
}
