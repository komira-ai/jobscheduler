//! A run's output committed to the put ingest after it (`native/put.rs`), over the fake
//! account host and launcher: the put mount is the run's put target and never the job's,
//! the job's `<rest>/pack` is committed as it is between the copy of the outputs and the
//! retirement, once, and every case that commits nothing or fails says so as the outputs.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};

use js_agent::lifecycle::{FsPrepared, JobFs, OutputsReport, RunHandle, Runtime, Status};
use js_agent::mac_put::Node;
use js_agent::native::{MountRoot, PutConfig, PutFlags, put_of};
use js_agent_wire::{
    JobMount, MountMode, MountSource, OutputKind, OutputSpec, Propagation, refusal,
};
use js_mac_pool::seatbelt::AbsPath;
use sha2::{Digest as _, Sha256};

use super::{Rig, me, my_gid, native};

/// The node of these tests: the test's own user, which the hand-over needs no root for.
fn node() -> Node {
    Node {
        uid: me(),
        gid: my_gid(),
    }
}

/// A rig whose runtime holds the read-write root `roots/rw` (a read deny of every run, as the
/// agent's settings make every mount root) and the put ingest `roots/rw/put` below it.
fn rig(name: &str) -> (Rig, PathBuf) {
    rig_with(name, false)
}

/// [`rig`], and with `trees` also the read-only trees root `trees`.
fn rig_with(name: &str, trees: bool) -> (Rig, PathBuf) {
    let rig = Rig::in_dir(name, 1, |config, dir| {
        if trees {
            fs::create_dir_all(dir.path("trees")).unwrap();
            config.mount_roots.push(MountRoot {
                path: fs::canonicalize(dir.path("trees")).unwrap(),
                read_only: true,
                trees: true,
            });
        }
        fs::create_dir_all(dir.path("roots/rw/put")).unwrap();
        let path = fs::canonicalize(dir.path("roots/rw")).unwrap();
        config.read_denies.push(AbsPath::new(&path).unwrap());
        config.mount_roots.push(MountRoot {
            path,
            read_only: false,
            trees: false,
        });
    });
    let root = fs::canonicalize(rig.dir.path("roots/rw/put")).unwrap();
    let put = PutConfig {
        root: root.clone(),
        node: node(),
    };
    let rt = rig.rt.with_put(Some(put));
    (Rig { rt, ..rig }, root)
}

fn mount(source: &Path, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source: MountSource::HostPath {
            path: source.display().to_string(),
        },
        target: target.to_owned(),
        mode,
        sub_path: None,
        no_exec: true,
        propagation: Propagation::Private,
    }
}

fn fs_of(mounts: Vec<JobMount>) -> JobFs {
    JobFs {
        layers: Vec::new(),
        mounts,
    }
}

fn handle() -> RunHandle {
    RunHandle("7.0".to_owned())
}

/// Starts the run `r1` (lease `7.0`) of the build service's shape: its put mount of
/// `<root>/cas/b/b1/n1` at `/work/.kbf-out`, and `declared` outputs. Answers its folder.
fn started(rig: &mut Rig, root: &Path, declared: Vec<OutputSpec>) -> PathBuf {
    let put = mount(
        &root.join("cas/b/b1/n1"),
        "/work/.kbf-out",
        MountMode::ReadWrite,
    );
    let fs = fs_of(vec![put]);
    assert_eq!(rig.rt.check_job_fs(&fs, &[]), Ok(()));
    assert_eq!(rig.rt.resolve_fs("r1", "", &fs, 0), FsPrepared::Ready);
    let mut body = native("/bin/sh", &[]);
    body.outputs = declared;
    let spec = rig.spec("r1", body);
    rig.rt.create_and_start(&spec).unwrap();
    rig.home_of(&account(rig)).join("exec")
}

fn account(rig: &Rig) -> String {
    rig.launcher
        .last()
        .var("USER")
        .expect("the account")
        .to_owned()
}

/// Writes `bytes` as the job's pack, as `kbf-task` does in its own folder.
fn write_pack(exec: &Path, bytes: &[u8]) {
    fs::create_dir(exec.join(".kbf-out")).unwrap();
    fs::write(exec.join(".kbf-out/pack"), bytes).unwrap();
}

/// Asserts that the job's pack is committed after the job and before the account is retired,
/// once: a removal whose retirement is held (the record cannot be deleted) fails with the
/// pack already in the put directory, the bytes the job wrote, mode 0640; the removal tried
/// again retires the account and does not commit again (the object is left as it was, and a
/// second commit would have been refused by the `pack` there); and the lifecycle is told the
/// key, digest and size once. Catches a commit after the retirement (the copy is gone with
/// the home), a commit made twice, and a report lost or repeated.
#[test]
fn a_put_mount_is_committed_once_before_the_retirement() {
    let (mut rig, root) = rig("put-once");
    let exec = started(&mut rig, &root, Vec::new());
    write_pack(&exec, b"the build's pack");
    assert!(matches!(rig.settle(&handle()), Status::Exited(_)));
    let name = account(&rig);
    rig.accounts.keep_record(&name);
    let held = rig.rt.remove(&handle()).unwrap_err();
    assert!(!held.is_outputs(), "{held}");
    let dir = root.join("cas/b/b1/n1");
    let pack = dir.join("pack");
    assert_eq!(fs::read(&pack).unwrap(), b"the build's pack");
    assert_eq!(fs::metadata(&pack).unwrap().mode() & 0o7777, 0o640);
    assert_eq!(rig.rt.outputs_report(&handle()), None, "not until removed");

    rig.accounts.free_record(&name);
    rig.rt.remove(&handle()).unwrap();
    assert!(!rig.accounts.records().contains(&name));
    assert_eq!(fs::read(&pack).unwrap(), b"the build's pack");
    let report = OutputsReport {
        key: "cas/b/b1/n1/pack".to_owned(),
        sha256: hex::encode(Sha256::digest(b"the build's pack")),
        bytes: 16,
    };
    assert_eq!(rig.rt.outputs_report(&handle()), Some(report));
    assert_eq!(rig.rt.outputs_report(&handle()), None, "answered once");
}

/// Asserts that an agent with a trees root admits the build service's put mount before its
/// directory exists: the put target is no tree and is never resolved against the trees root's
/// rules, so its missing `<nonce>` folder is not `input-unavailable`. Catches every build
/// service job refused on a Mac that mounts trees, since the agent, not the build service,
/// makes the put directory at the commit.
#[test]
fn a_put_target_not_made_yet_is_admitted_beside_a_trees_root() {
    let (mut rig, root) = rig_with("put-trees-root", true);
    let dir = root.join("cas/b/b1/n1");
    assert!(fs::symlink_metadata(&dir).is_err(), "not made yet");
    let fs = fs_of(vec![mount(&dir, "/work/.kbf-out", MountMode::ReadWrite)]);
    assert_eq!(rig.rt.check_job_fs(&fs, &[]), Ok(()));
    assert_eq!(rig.rt.resolve_fs("r1", "", &fs, 0), FsPrepared::Ready);
}

/// Asserts that the job never holds its put directory: the put mount is not linked into its
/// folder, no profile parameter but a read deny names the put directory or anything below the
/// put root, and the put root itself is a read deny of the run's profile (the runtime makes it
/// one; it is no mount root). Catches a job given a writable put directory, and one that can
/// read other builds' puts.
#[test]
fn the_put_directory_is_never_the_jobs() {
    let (mut rig, root) = rig("put-never");
    let exec = started(&mut rig, &root, Vec::new());
    assert!(fs::symlink_metadata(exec.join(".kbf-out")).is_err());
    let launched = rig.launcher.last();
    let below = root.display().to_string();
    let named: Vec<&(String, String)> = launched
        .params
        .iter()
        .filter(|(key, value)| value.starts_with(&below) && !key.starts_with("RD_"))
        .collect();
    assert!(named.is_empty(), "{named:?}");
    assert!(
        launched
            .params
            .iter()
            .any(|(key, value)| key.starts_with("RD_") && *value == below),
        "{:?}",
        launched.params
    );
}

/// Asserts that a declared output that cannot be copied means no commit: the removal fails
/// as the outputs (`outputs not copied`), the put directory is never made, the account is
/// retired and the lifecycle is told of no commit. Catches a pack committed under a key while
/// declared outputs are missing.
#[test]
fn a_collection_failure_commits_nothing() {
    let (mut rig, root) = rig("put-failed-copy");
    let declared = vec![OutputSpec {
        path: "out".to_owned(),
        kind: OutputKind::File,
    }];
    let exec = started(&mut rig, &root, declared);
    write_pack(&exec, b"pack");
    symlink("elsewhere", exec.join("out")).unwrap();
    rig.settle(&handle());
    let name = account(&rig);
    let why = rig.rt.remove(&handle()).unwrap_err();
    assert!(why.is_outputs(), "{why}");
    assert!(
        why.to_string().starts_with("outputs not copied: out: "),
        "{why}"
    );
    assert!(!root.join("cas").exists());
    assert!(!rig.accounts.records().contains(&name));
    assert_eq!(rig.rt.outputs_report(&handle()), None);
}

/// Asserts that a commit that fails (the put directory already holds a file) fails the
/// removal as the outputs only, with the reason, after the account is retired: the run's
/// exit stays what it was, the stray file is untouched and nothing is reported committed.
/// Catches a failed commit that holds the account or changes the run's result, and one that
/// overwrites what is in the put directory.
#[test]
fn a_failed_commit_is_outputs_failed_and_the_account_still_retired() {
    let (mut rig, root) = rig("put-failed-commit");
    let dir = root.join("cas/b/b1/n1");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("stray"), b"not ours").unwrap();
    let exec = started(&mut rig, &root, Vec::new());
    write_pack(&exec, b"pack");
    assert!(matches!(rig.settle(&handle()), Status::Exited(_)));
    let name = account(&rig);
    let why = rig.rt.remove(&handle()).unwrap_err();
    assert!(why.is_outputs(), "{why}");
    let said = format!(
        "outputs not committed: {} already holds stray",
        dir.display()
    );
    assert_eq!(why.to_string(), said);
    assert!(!rig.accounts.records().contains(&name));
    assert_eq!(fs::read(dir.join("stray")).unwrap(), b"not ours");
    assert!(!dir.join("pack").exists());
    assert_eq!(rig.rt.outputs_report(&handle()), None);
}

/// Asserts that a run whose job wrote no pack commits nothing and fails nothing: the removal
/// succeeds, the put directory is never made, nothing is reported. Catches a job that wrote
/// nothing turned into a failure or an empty object.
#[test]
fn a_job_that_writes_no_pack_commits_nothing() {
    let (mut rig, root) = rig("put-no-pack");
    started(&mut rig, &root, Vec::new());
    rig.settle(&handle());
    rig.rt.remove(&handle()).unwrap();
    assert!(!root.join("cas").exists());
    assert_eq!(rig.rt.outputs_report(&handle()), None);
}

/// Asserts that beneath the put root only a read-write mount of `cas/b/<build>/<nonce>` is
/// taken: a read-only mount of a put directory, a read-write mount of `cas/b/<build>`, a
/// read-write mount elsewhere in the root whose link leads into the put root, and a mount
/// (read-only or read-write) of the directory that holds the put root are refused
/// `input-not-allowed`, naming why. Catches a job given a way to read or write other builds'
/// puts.
#[test]
fn any_other_mount_beneath_the_put_root_is_refused() {
    let (mut rig, root) = rig("put-refused");
    fs::create_dir_all(root.join("cas/b/b1/n1")).unwrap();
    let holder = root.parent().unwrap().to_owned();
    symlink(&root, holder.join("to-put")).unwrap();
    let cases = [
        (
            mount(
                &root.join("cas/b/b1/n1"),
                "/work/.kbf-out",
                MountMode::ReadOnly,
            ),
            format!(
                "{} lies beneath --native-put-root, where a job is given only its put target \
                 (read-write)",
                root.join("cas/b/b1/n1").display()
            ),
        ),
        (
            mount(
                &root.join("cas/b/b1"),
                "/work/.kbf-out",
                MountMode::ReadWrite,
            ),
            format!(
                "a put target: {}: not of the shape cas/b/<build>/<nonce> below the put root",
                root.join("cas/b/b1").display()
            ),
        ),
        (
            mount(&holder.join("to-put"), "/work/out", MountMode::ReadWrite),
            format!(
                "{} holds or lies beneath --native-put-root once its links are followed",
                holder.join("to-put").display()
            ),
        ),
        (
            mount(&holder, "/work/out", MountMode::ReadOnly),
            format!(
                "{} holds or lies beneath --native-put-root once its links are followed",
                holder.display()
            ),
        ),
        (
            mount(&holder, "/work/out", MountMode::ReadWrite),
            format!(
                "{} holds or lies beneath --native-put-root once its links are followed",
                holder.display()
            ),
        ),
    ];
    for (one, why) in cases {
        let refused = rig.rt.resolve_fs("r1", "", &fs_of(vec![one]), 0);
        let FsPrepared::Refused(refused) = refused else {
            panic!("not refused: {why}");
        };
        assert_eq!(refused.code, refusal::INPUT_NOT_ALLOWED, "{why}");
        assert_eq!(refused.why, why);
    }
}

/// Asserts the checks of the put flags: neither flag is no put; one without the other, a root
/// that is not a directory (or not absolute) and a user the directory service does not know
/// are refused naming the flag; a
/// number is the node's uid and group, a name is looked up; the root is kept canonical.
/// Catches an agent that starts with a put root it cannot use or a node it cannot hand a
/// directory to. A lookup that fails is refused with its error.
#[test]
fn the_put_flags_are_checked() {
    let dir = crate::support::Scratch::new("native-put-flags");
    fs::create_dir_all(dir.path("rw/put")).unwrap();
    fs::write(dir.path("rw/file"), "").unwrap();
    let users = |name: &str| -> io::Result<Option<Node>> {
        match name {
            "_broken" => Err(io::Error::other("the directory service did not answer")),
            name => Ok((name == "_tierstore").then_some(Node { uid: 7, gid: 8 })),
        }
    };
    let flags = |root: Option<PathBuf>, user: Option<&str>| PutFlags {
        native_put_root: root,
        native_put_user: user.map(str::to_owned),
    };
    assert_eq!(put_of(&flags(None, None), &users), Ok(None));
    let put = dir.path("rw/put");
    let canonical = fs::canonicalize(&put).unwrap();
    let refused = [
        (
            flags(Some(put.clone()), None),
            "--native-put-root: needs --native-put-user".to_owned(),
        ),
        (
            flags(None, Some("_tierstore")),
            "--native-put-user: needs --native-put-root".to_owned(),
        ),
        (
            flags(Some(dir.path("rw/file")), Some("_tierstore")),
            format!(
                "--native-put-root: {} is not a directory",
                dir.path("rw/file").display()
            ),
        ),
        (
            flags(Some(PathBuf::from("rw/put")), Some("_tierstore")),
            "--native-put-root: rw/put is not a directory".to_owned(),
        ),
        (
            flags(Some(put.clone()), Some("_nobody_here")),
            "--native-put-user: no user \"_nobody_here\"".to_owned(),
        ),
        (
            flags(Some(put.clone()), Some("_broken")),
            "--native-put-user: _broken: the directory service did not answer".to_owned(),
        ),
    ];
    for (given, why) in refused {
        assert_eq!(put_of(&given, &users), Err(why));
    }
    let named = put_of(&flags(Some(put.clone()), Some("_tierstore")), &users);
    let want = |node| {
        Ok(Some(PutConfig {
            root: canonical.clone(),
            node,
        }))
    };
    assert_eq!(named, want(Node { uid: 7, gid: 8 }));
    let numbered = put_of(&flags(Some(put), Some("281")), &users);
    assert_eq!(numbered, want(Node { uid: 281, gid: 281 }));
}

/// Asserts that a put root no mount root holds takes its put mount end to end: the runtime
/// says it takes the mount's source (so the lifecycle does not hold it to the mount roots),
/// and not a path beside the root; the put directory, which does not exist yet, resolves as
/// the run's put target; the job's pack is committed and
/// reported. Catches a put root that must be a mount root (which put it on the state root's
/// file system); `a_put_target_not_made_yet_is_admitted_beside_a_trees_root` holds the nonce
/// folder not made yet to the tree check.
#[test]
fn a_put_root_that_is_no_mount_root_takes_its_put_mount() {
    let rig = Rig::in_dir("put-no-root", 1, |_, dir| {
        fs::create_dir_all(dir.path("put")).unwrap();
    });
    let root = fs::canonicalize(rig.dir.path("put")).unwrap();
    let put = PutConfig {
        root: root.clone(),
        node: node(),
    };
    let rt = rig.rt.with_put(Some(put));
    let mut rig = Rig { rt, ..rig };
    let dir = root.join("cas/b/b1/n1");
    assert!(rig.rt.owns_host_path(&dir.display().to_string()));
    let beside = root.with_file_name("put-other");
    assert!(!rig.rt.owns_host_path(&beside.display().to_string()));
    assert!(!dir.exists(), "the build service makes no nonce folder");
    let exec = started(&mut rig, &root, Vec::new());
    write_pack(&exec, b"pack");
    rig.settle(&handle());
    rig.rt.remove(&handle()).unwrap();
    assert_eq!(fs::read(dir.join("pack")).unwrap(), b"pack");
    assert!(rig.rt.outputs_report(&handle()).is_some());
}

/// Asserts the put flags through the whole macOS agent, as its command line and its start read
/// them: beside a native agent's flags, `--native-put-root` and `--native-put-user` given as a
/// number or as a user of the directory service (`nobody`, on every Mac) start the agent,
/// which says hello. One without the other, a root that is not a directory and a user the
/// directory service does not know end it with status 2 before the hello, naming the flag by
/// exact text; and `--no-jobs` refuses each flag by name. Catches a Mac agent that drops the
/// flags, checks them only in a struct no command line reaches, or starts with a put ingest it
/// cannot hand to the node.
#[test]
fn the_put_flags_go_through_the_mac_agents_start() {
    use serde_json::Value;

    use super::agent::{hello_on, native_flags};
    use crate::jobs::fake::FakeMachine;
    use crate::support::{
        CountingSleeper, Jobs, Lowest, Scratch, Script, ScriptConnector, code, flags, mac_host,
        run_on_defaulted, welcome,
    };

    let dir = Scratch::new("native-put-agent");
    fs::create_dir_all(dir.path("put")).unwrap();
    fs::write(dir.path("not-a-dir"), "").unwrap();
    let put = dir.path("put").display().to_string();
    // The native flags make their directories once; each run of the agent reuses them.
    let base = native_flags(&dir);
    let with = |extra: &[&str]| {
        let mut given = base.clone();
        given.extend(extra.iter().map(|flag| (*flag).to_owned()));
        hello_on(&mac_host(), &given, welcome(1))
    };
    for user in ["281", "nobody"] {
        let (ran, hello) = with(&["--native-put-root", &put, "--native-put-user", user]);
        assert_eq!(ran.status(), code(0), "{user}: {}", ran.err);
        assert_ne!(hello, Value::Null, "{user}: no hello");
    }
    let file = dir.path("not-a-dir").display().to_string();
    let refused = [
        (
            vec!["--native-put-root", put.as_str()],
            "--native-put-root: needs --native-put-user".to_owned(),
        ),
        (
            vec!["--native-put-user", "nobody"],
            "--native-put-user: needs --native-put-root".to_owned(),
        ),
        (
            vec!["--native-put-root", &file, "--native-put-user", "nobody"],
            format!("--native-put-root: {file} is not a directory"),
        ),
        (
            vec![
                "--native-put-root",
                &put,
                "--native-put-user",
                "_js-no-such-user",
            ],
            "--native-put-user: no user \"_js-no-such-user\"".to_owned(),
        ),
    ];
    for (extra, why) in refused {
        let (ran, hello) = with(&extra);
        assert_eq!(ran.status(), code(2), "{why}: {}", ran.err);
        assert_eq!(
            ran.err.lines().last(),
            Some(format!("js-agent: {why}").as_str()),
            "{}",
            ran.err
        );
        assert_eq!(hello, Value::Null, "{why}");
    }
    for (flag, value) in [
        ("--native-put-root", put.as_str()),
        ("--native-put-user", "nobody"),
    ] {
        let args = flags(&["--no-jobs", flag, value]);
        let (script, sent) = Script::new(vec![welcome(1)]);
        let connector = ScriptConnector::of(script);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let jobs: Jobs<'_> = (std::sync::Arc::new(FakeMachine::new()), None);
        let ran = run_on_defaulted(
            &args,
            &mac_host(),
            &connector,
            &mut sleeper,
            &mut Lowest,
            jobs,
        );
        assert_eq!(ran.status(), code(2), "{flag}: {}", ran.err);
        assert_eq!(
            ran.err,
            format!("js-agent: {flag}: not with --no-jobs (an agent that runs no job)\n"),
            "{flag}"
        );
        assert!(sent.borrow().is_empty(), "{flag}: something was sent");
    }
}
