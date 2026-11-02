//! The agent's state root as the hook on a machine finds it, before any step runs: an update
//! with none, a state root that is there and not usable on a first install and on an update,
//! the record's commit kept in the refusal, a refusal that needs no lock and takes none, and the
//! lease's credential from an unusable state root. No socket is opened: the params name a lease
//! server ([`NO_SERVER`]) the client refuses to be made for, by its text alone, so a step that
//! asked the lease anything would end with that refusal, which the exact details here rule out
//! (`the_lease_server_of_these_tests_is_refused_before_any_socket` shows it). The owner and
//! permission-denied cases need other users, and are in `tests/privilege/hook_root.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::own_host::RequestLimit;
use js_agent::update_hook::{
    Class, ConnectParams, Done, Held, Outcome, Reason, Record, Step, UpdateInfo,
};

use super::lease_port::{all_unreachable, lease, reach};
use super::machine::Tree;
use super::step_rig::as_root;
use crate::jobs::private;
use crate::support::Scratch;

/// What the agent's own check says of a directory that others may enter.
const OPEN: &str = "group and others must have no permission";
/// What it says of a link or a file.
const NOT_DIR: &str = "not a directory (a link is not accepted)";
/// What it says of a path it cannot look at.
const UNREAD: &str = "cannot read the directory";

/// The hook's refusal of `path` with the agent's words `why`.
fn unusable(path: &Path, why: &str) -> String {
    format!("state_root: {}: {why}", path.display())
}

/// A state root that others can read, with a credential in it: not usable.
pub(super) fn open_state_root(files: &Scratch) -> PathBuf {
    let open = files.path("open-state");
    fs::create_dir_all(&open).unwrap();
    fs::write(
        open.join(js_agent::CREDENTIAL_FILE),
        "cred-0123456789abcdefghij",
    )
    .unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    open
}

/// Every shape of a state root that is there and not usable, with the hook's exact refusal of
/// it: open to others (with a credential in it), a link to a private directory, a link to
/// nothing, a file, and a path below a file (which cannot be looked at).
fn unusable_roots(files: &Scratch) -> Vec<(PathBuf, String)> {
    let open = open_state_root(files);
    let target = private(files, "private-state");
    let link = files.path("linked-state");
    symlink(&target, &link).unwrap();
    let dangling = files.path("dangling-state");
    symlink(files.path("nothing-here"), &dangling).unwrap();
    let file = files.write("file-state", "x", 0o600);
    let below_file = file.join("state");
    [
        (open, OPEN),
        (link, NOT_DIR),
        (dangling, UNREAD),
        (file, NOT_DIR),
        (below_file, UNREAD),
    ]
    .into_iter()
    .map(|(path, why)| {
        let said = unusable(&path, why);
        (path, said)
    })
    .collect()
}

/// A lease server the client is never made for: plain http to a host off the loopback, which
/// it refuses from the text of the address before it opens a socket or resolves a name (the
/// `.invalid` name is reserved and never resolves).
const NO_SERVER: &str = "http://lease.invalid";

/// What every lease call answers when the params name [`NO_SERVER`].
const NO_SERVER_SAID: &str = "server: \"http://lease.invalid\" is plain http to a host off the \
                              loopback: a bearer token never crosses a network in clear, use https://";

/// The params lines that name [`NO_SERVER`] as `mac-1`, with a token file.
fn lease_lines(files: &Scratch) -> String {
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    format!(
        "server = \"{NO_SERVER}\"\ntoken_file = \"{}\"\nhost_id = \"mac-1\"\n",
        token.display()
    )
}

/// Every file of the hook's state directory, by name, with its bytes.
fn state_files(tree: &Tree) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(tree.root.join("state"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            (name, fs::read(entry.path()).unwrap())
        })
        .collect()
}

/// Asserts that `done` is the refusal `said`: exit 1, `state_invalid`, class release, with
/// `reversible` as given.
#[track_caller]
fn assert_refused(done: &Done, said: &str, reversible: bool, what: &str) {
    assert_eq!(done.outcome, Outcome::Failure, "{what}: {done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{what}");
    assert_eq!(done.report.detail.as_deref(), Some(said), "{what}");
    assert_eq!(done.report.reversible, reversible, "{what}");
    assert_eq!(done.report.class, Class::Release, "{what}");
}

/// Asserts that an update whose params name no state root, or name a path where nothing is,
/// fails every step `state_invalid` in exact words that say which of the two it is, and
/// reversibly (no record says anything was done); that nothing is made in the hook's state
/// directory, not even the lock file; and that the lease is not asked anything. It catches
/// an absent root read as "no taint, nothing owed" on an update, and a check that comes after
/// the lock or the lease.
#[test]
fn an_update_with_no_state_root_or_one_that_is_not_there_fails_every_step_before_the_lock() {
    let tree = Tree::new("hook-root-update-none");
    let server = lease_lines(&tree.files);
    let absent = tree.root.join("no-such-state");
    for (extra, said) in [
        (
            server.clone(),
            "state_root: an update needs the state root of the agent that runs, and the params \
             name none"
                .to_owned(),
        ),
        (
            format!("{server}state_root = \"{}\"\n", absent.display()),
            format!(
                "state_root: {} is not there, and an update needs the state root of the agent \
                 that runs",
                absent.display()
            ),
        ),
    ] {
        let params = tree.params(true, &extra);
        for step in Step::ALL {
            let done = tree.run(step, &params, Err("unused"));
            assert_refused(&done, &said, true, &format!("{step}"));
        }
    }
    assert!(state_files(&tree).is_empty());
}

/// Asserts that a first install, whose params name a lease server and a token file and no
/// state root, or a state root where nothing is, runs `prepare` to success and writes its
/// record, and that it asks the lease nothing. It catches the update's rule
/// applied to a first install.
#[test]
fn a_first_install_with_a_lease_server_and_no_state_root_still_runs() {
    let tree = Tree::new("hook-root-first-none");
    let server = lease_lines(&tree.files);
    let absent = tree.root.join("no-such-state");
    for extra in [
        server.clone(),
        format!("{server}state_root = \"{}\"\n", absent.display()),
    ] {
        let params = tree.params(false, &extra);
        let done = tree.run(Step::Prepare, &params, Err("unused"));
        assert_eq!(done.outcome, Outcome::Success, "{extra}: {done:?}");
        assert!(tree.record().exists());
        fs::remove_file(tree.record()).unwrap();
    }
}

/// Asserts that every shape of a state root that is there and not usable fails every step of a
/// first install and of an update `state_invalid`, in the exact words of the hook's param and
/// the agent's check (never the agent's flag `--state-root`), reversibly, with nothing made in
/// the hook's state directory and the lease not asked. It catches a first install that ignores an
/// unusable root, a dangling link taken for no root, and a step that skips the check.
#[test]
fn a_state_root_that_is_there_and_unusable_fails_every_step_of_a_first_install_or_an_update() {
    let tree = Tree::new("hook-root-unusable");
    let server = lease_lines(&tree.files);
    for (root, said) in unusable_roots(&tree.files) {
        for previous in [false, true] {
            let line = format!("{server}state_root = \"{}\"\n", root.display());
            let params = tree.params(previous, &line);
            for step in Step::ALL {
                let done = tree.run(step, &params, Err("unused"));
                let what = format!("{step}, update {previous}, {}", root.display());
                assert_refused(&done, &said, true, &what);
            }
        }
    }
    assert!(state_files(&tree).is_empty());
}

/// The record of an update from 1.3.0 in `tree`, with `marks` set to true.
fn record_with(tree: &Tree, first_install: bool, marks: &[&str]) -> Record {
    let mut record = Record::new(UpdateInfo {
        target: "1.4.0".to_owned(),
        previous: (!first_install).then(|| "1.3.0".to_owned()),
        staged: tree.staged(),
        previous_target: (!first_install).then(|| tree.root.join("releases/1.3.0")),
        unit: "js-agent-crun.service".to_owned(),
        artifact_digest: format!("sha256:{}", "0123456789abcdef".repeat(4)),
    });
    let mut value = serde_json::to_value(&record).unwrap();
    for mark in marks {
        value[*mark] = serde_json::Value::Bool(true);
    }
    record = serde_json::from_value(value).unwrap();
    record
}

/// Asserts that a step refused for its state root keeps what the record says of the commit: a
/// `cutover` or `rollback` rerun whose record has `stop_begun` (an update) or `link_moved` (a
/// first install) says `reversible: false`, as its own body would; `prepare`, `verify` and
/// `cleanup` on the same record, and every step on a record with no such mark, say `true`; a
/// record that cannot be read, or one of another update that the step's body would refuse,
/// says nothing of a commit (`true`). Each for an open state root,
/// and for an update with none named. The hook's state directory is left byte for byte as it
/// was, and the lease is not asked. It catches a refusal that tells the updater a committed update
/// can still be undone.
#[test]
fn a_refused_step_whose_record_is_past_its_commit_says_it_cannot_be_undone() {
    let tree = Tree::new("hook-root-committed");
    let server = lease_lines(&tree.files);
    let open = open_state_root(&tree.files);
    let open_said = unusable(&open, OPEN);
    let none_said = "state_root: an update needs the state root of the agent that runs, and the \
                     params name none";
    let records = [
        (false, &["stop_begun"][..], true),
        (true, &["link_moved"][..], true),
        (false, &["link_moved"][..], false),
        (true, &[][..], false),
        (false, &[][..], false),
    ];
    for (first_install, marks, committed) in records {
        let record = record_with(&tree, first_install, marks);
        Held::open(&tree.root.join("state"), &as_root)
            .unwrap()
            .save(&record)
            .unwrap();
        let before = state_files(&tree);
        let mut cases = vec![(
            format!("{server}state_root = \"{}\"\n", open.display()),
            open_said.clone(),
        )];
        if !first_install {
            cases.push((server.clone(), none_said.to_owned()));
        }
        for (extra, said) in cases {
            let params = tree.params(!first_install, &extra);
            for step in Step::ALL {
                let past = committed && matches!(step, Step::Cutover | Step::Rollback);
                let what = format!("{step}, first install {first_install}, {marks:?}");
                assert_refused(&tree.run(step, &params, Err("unused")), &said, !past, &what);
                assert_eq!(state_files(&tree), before, "{what}");
            }
        }
    }
    // A record of another update (another target), past its commit, is one the step's body
    // refuses before it reads the commit: it says nothing of this update's commit either.
    let mut stale = record_with(&tree, false, &["stop_begun"]);
    stale.update.target = "9.9.9".to_owned();
    Held::open(&tree.root.join("state"), &as_root)
        .unwrap()
        .save(&stale)
        .unwrap();
    let params = tree.params(
        true,
        &format!("{server}state_root = \"{}\"\n", open.display()),
    );
    for (content, what) in [
        (None, "a record of another update"),
        (Some("not a record"), "a record that cannot be read"),
    ] {
        if let Some(content) = content {
            fs::write(tree.record(), content).unwrap();
        }
        let before = state_files(&tree);
        for step in [Step::Cutover, Step::Rollback] {
            assert_refused(
                &tree.run(step, &params, Err("unused")),
                &open_said,
                true,
                &format!("{step}, {what}"),
            );
        }
        assert_eq!(state_files(&tree), before, "{what}");
    }
}

/// Asserts that the refusal of a state root needs no lock: while another holder has the hook's
/// lock, every step with an unusable state root, or of an update with none, still ends with
/// the state root's refusal, the record and the state directory stay byte for byte as they
/// were, and the lease is not asked; and, as the control, the same step with a usable state root
/// ends `lock_busy` (the lock was held). It catches a check made only after the lock, and one
/// that makes or changes anything before refusing.
#[test]
fn a_refusal_of_the_state_root_needs_no_lock_and_changes_nothing() {
    let tree = Tree::new("hook-root-locked");
    let server = lease_lines(&tree.files);
    let held = Held::open(&tree.root.join("state"), &as_root).unwrap();
    held.save(&record_with(&tree, false, &["stop_begun"]))
        .unwrap();
    let before = state_files(&tree);
    let open = open_state_root(&tree.files);
    for (extra, said) in [
        (
            format!("{server}state_root = \"{}\"\n", open.display()),
            unusable(&open, OPEN),
        ),
        (
            server.clone(),
            "state_root: an update needs the state root of the agent that runs, and the params \
             name none"
                .to_owned(),
        ),
    ] {
        let params = tree.params(true, &extra);
        for step in Step::ALL {
            let past = matches!(step, Step::Cutover | Step::Rollback);
            let done = tree.run(step, &params, Err("unused"));
            assert_refused(&done, &said, !past, &format!("{step}"));
            assert_eq!(state_files(&tree), before, "{step}");
        }
    }
    let usable = private(&tree.files, "usable-state");
    let params = tree.params(
        true,
        &format!("{server}state_root = \"{}\"\n", usable.display()),
    );
    for step in Step::ALL {
        let done = tree.run(step, &params, Err("unused"));
        assert_eq!(done.report.reason, Some(Reason::Locked), "{step}: {done:?}");
    }
    drop(held);
    assert_eq!(state_files(&tree), before);
}

/// Asserts that a lease made from params whose state root is there and not usable answers
/// `host`, `cordon` and `uncordon` with exactly the hook's refusal of that state root, for
/// every shape of one, with a token file and without one: never a demand for `token_file`,
/// never the server's refusal of a join token, never the refusal of the server's address. It catches the
/// join token used in place of an unusable state root, which would reach the server under the
/// pool's identity instead of the host's.
#[test]
fn an_unusable_state_root_is_the_leases_error_with_or_without_a_token_and_nothing_is_sent() {
    let files = Scratch::new("hook-root-lease");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    for (root, said) in unusable_roots(&files) {
        for token in [Some(token.clone()), None] {
            let params: ConnectParams = reach(NO_SERVER, Some(root.clone()), token.clone());
            for text in all_unreachable(&lease(&params, RequestLimit::DEFAULT)) {
                assert_eq!(text, said, "{}, token {token:?}", root.display());
            }
        }
    }
}

/// Asserts the control of every test above: with a usable state root, an update's `prepare`
/// does ask the lease, and ends `lease_unreachable` with exactly the client's refusal of
/// [`NO_SERVER`], made from its text alone. So a step that asked the lease would show that
/// detail, not the state root's refusal, and no test here can open a socket.
#[test]
fn the_lease_server_of_these_tests_is_refused_before_any_socket() {
    let tree = Tree::new("hook-root-no-server");
    let line = format!("{}{}", lease_lines(&tree.files), tree.state_line());
    let done = tree.run(Step::Prepare, &tree.params(true, &line), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.detail.as_deref(), Some(NO_SERVER_SAID));
    let params = reach(
        NO_SERVER,
        None,
        Some(tree.files.write("t", "token-0123456789abcdef", 0o600)),
    );
    for text in all_unreachable(&lease(&params, RequestLimit::DEFAULT)) {
        assert_eq!(text, NO_SERVER_SAID);
    }
}
