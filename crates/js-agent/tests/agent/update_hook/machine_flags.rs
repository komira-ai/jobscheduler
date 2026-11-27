//! The host's flags file and the files it names as the hook on a machine looks at them: the flags
//! file is where the server, the certificate and the host id come from, and the certificate and
//! key files are looked at through links with their mode and owner.

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

/// A first install asks the lease server nothing, and reads the flags file to find the host's
/// certificate and key: a flags file that is not there names none, so the install defers
/// (`join_token_missing`, a shipped reason name) until the operator has placed it, naming the
/// flags file and the two flags.
///
/// Catches a first install that goes on for a host whose agent would not start for want of an
/// identity.
#[test]
fn a_first_install_defers_when_the_flags_file_is_not_placed_yet() {
    let tree = Tree::new("hook-machine-first-no-flags");
    let params = tree.params(false, "");
    fs::remove_file(tree.flags()).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the flags file {} names no --host-cert and --host-key: a host is identified by \
             its certificate; name them to install the agent",
            params.flags_file.display()
        ))
    );
    assert!(!tree.record().exists());
}

/// The machine looks at the host's key file as the system has it: through a link (secret
/// mounts are links), with its permission bits and owner, and a directory is not a regular file.
///
/// Catches a look at the link and not at the file, which would take a good key for a link.
#[test]
fn the_machine_looks_at_a_host_file_through_links_with_its_mode_and_owner() {
    let files = Scratch::new("hook-machine-key-facts");
    let key = files.write("host.key", "a key\n", 0o640);
    let link = files.path("key-link");
    std::os::unix::fs::symlink(&key, &link).unwrap();
    let dir = private(&files, "a-directory");
    let machine = Machine::system();
    for path in [&key, &link] {
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
