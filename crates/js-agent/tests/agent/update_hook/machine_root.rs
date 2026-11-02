//! The agent's state root as the hook on a machine finds it, before any step runs: an update
//! with none, a state root that is there and not usable on a first install and on an update,
//! the record's commit kept in the refusal, a refusal that needs no lock and takes none, and the
//! lease's credential from an unusable state root. The lease server is a counting stub: every
//! case here is refused before a request, and the stub shows that none was sent. The owner and
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
use super::lease_stub::{Reply, Stub};
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

/// The params lines that would reach `stub` as `mac-1` with a token file.
fn reaching_stub(stub: &Stub, files: &Scratch) -> String {
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    format!(
        "server = \"{}\"\ntoken_file = \"{}\"\nhost_id = \"mac-1\"\n",
        stub.url,
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
/// directory, not even the lock file; and that no request reaches the lease server. It catches
/// an absent root read as "no taint, nothing owed" on an update, and a check that comes after
/// the lock or the lease.
#[test]
fn an_update_with_no_state_root_or_one_that_is_not_there_fails_every_step_before_the_lock() {
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let tree = Tree::new("hook-root-update-none");
    let server = reaching_stub(&stub, &tree.files);
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
    assert_eq!(stub.stop(), 0, "no request reached the lease server");
}

/// Asserts that a first install, whose params name a lease server and a token file and no
/// state root, or a state root where nothing is, runs `prepare` to success and writes its
/// record, and that it sends no request to the lease server. It catches the update's rule
/// applied to a first install.
#[test]
fn a_first_install_with_a_lease_server_and_no_state_root_still_runs() {
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let tree = Tree::new("hook-root-first-none");
    let server = reaching_stub(&stub, &tree.files);
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
    assert_eq!(stub.stop(), 0, "a first install asks no lease server");
}

/// Asserts that every shape of a state root that is there and not usable fails every step of a
/// first install and of an update `state_invalid`, in the exact words of the hook's param and
/// the agent's check (never the agent's flag `--state-root`), reversibly, with nothing made in
/// the hook's state directory and no request sent. It catches a first install that ignores an
/// unusable root, a dangling link taken for no root, and a step that skips the check.
#[test]
fn a_state_root_that_is_there_and_unusable_fails_every_step_of_a_first_install_or_an_update() {
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let tree = Tree::new("hook-root-unusable");
    let server = reaching_stub(&stub, &tree.files);
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
    assert_eq!(stub.stop(), 0, "no request reached the lease server");
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
/// record that cannot be read says nothing of a commit (`true`). Each for an open state root,
/// and for an update with none named. The hook's state directory is left byte for byte as it
/// was, and no request is sent. It catches a refusal that tells the updater a committed update
/// can still be undone.
#[test]
fn a_refused_step_whose_record_is_past_its_commit_says_it_cannot_be_undone() {
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let tree = Tree::new("hook-root-committed");
    let server = reaching_stub(&stub, &tree.files);
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
    fs::write(tree.record(), "not a record").unwrap();
    let before = state_files(&tree);
    let params = tree.params(
        true,
        &format!("{server}state_root = \"{}\"\n", open.display()),
    );
    for step in [Step::Cutover, Step::Rollback] {
        assert_refused(
            &tree.run(step, &params, Err("unused")),
            &open_said,
            true,
            &format!("{step}, a record that cannot be read"),
        );
    }
    assert_eq!(state_files(&tree), before);
    assert_eq!(stub.stop(), 0, "no request reached the lease server");
}

/// Asserts that the refusal of a state root needs no lock: while another holder has the hook's
/// lock, every step with an unusable state root, or of an update with none, still ends with
/// the state root's refusal, the record and the state directory stay byte for byte as they
/// were, and no request is sent; and, as the control, the same step with a usable state root
/// ends `lock_busy` (the lock was held). It catches a check made only after the lock, and one
/// that makes or changes anything before refusing.
#[test]
fn a_refusal_of_the_state_root_needs_no_lock_and_changes_nothing() {
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let tree = Tree::new("hook-root-locked");
    let server = reaching_stub(&stub, &tree.files);
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
    assert_eq!(stub.stop(), 0, "no request reached the lease server");
}

/// Asserts that a lease made from params whose state root is there and not usable answers
/// `host`, `cordon` and `uncordon` with exactly the hook's refusal of that state root, for
/// every shape of one, with a token file and without one: never a demand for `token_file`,
/// never the server's refusal of a join token; and that no request is sent. It catches the
/// join token used in place of an unusable state root, which would reach the server under the
/// pool's identity instead of the host's.
#[test]
fn an_unusable_state_root_is_the_leases_error_with_or_without_a_token_and_nothing_is_sent() {
    let files = Scratch::new("hook-root-lease");
    let stub = Stub::start(&[Reply::Answer(200, "{}")]);
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    for (root, said) in unusable_roots(&files) {
        for token in [Some(token.clone()), None] {
            let params: ConnectParams = reach(&stub.url, Some(root.clone()), token.clone());
            for text in all_unreachable(&lease(&params, RequestLimit::DEFAULT)) {
                assert_eq!(text, said, "{}, token {token:?}", root.display());
            }
        }
    }
    assert_eq!(stub.stop(), 0, "no request reached the lease server");
}
