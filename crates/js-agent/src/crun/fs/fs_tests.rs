//! [`Fs`]: the directory of the runs' mounts and the roots opened when the agent starts, the
//! lookups of host paths on the pool, an assembly asked once and taken once, a discard, and
//! the check after a job, with the fakes of [`super::tests`].

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_agent_wire::{JobMount, Layer, MountMode, refusal};
use js_testkit::dir::TestDir;

use super::pool::Poll;
use super::tests::{Mounts, Paths, host_layer, mount, owners, scratch, until};
use super::{CHECK_MS, Fs, FsError, FsSettings, LOOKUP_MS};
use crate::lifecycle::{FsPrepared, JobFs, MountRoots, NativeRefusal};

/// The file systems of a test: the mounts in `dir`, the roots `data` (read-write) and `ro`.
fn open(dir: &TestDir, paths: Paths) -> (Fs, Arc<Mounts>) {
    let (data, ro) = (dir.path().join("data"), dir.path().join("ro"));
    for root in [&data, &ro] {
        fs::create_dir_all(root.join("tree/sub")).expect("a root");
    }
    let roots = MountRoots::with_read_only(&[("data".to_owned(), data)], &[("ro".to_owned(), ro)])
        .expect("roots");
    let calls = Arc::new(Mounts::under(dir.path()));
    let settings = FsSettings {
        dir: dir.path().join("mounts"),
        roots,
        calls: calls.clone(),
        paths: Arc::new(paths),
        image_allow: Vec::new(),
    };
    (Fs::open(settings).expect("opened"), calls)
}

/// A start's file system of the mounts `mounts` alone.
fn job(mounts: Vec<JobMount>) -> JobFs {
    JobFs {
        layers: Vec::new(),
        mounts,
    }
}

/// Catches a directory of mounts that is not the agent's private mount (or a second bind
/// over one an earlier process left), and roots that are not opened when the agent starts:
/// the directory is made `0711`, bound onto itself only when it is not a mount, made
/// private, and every root opened; a root that cannot be opened, a directory that cannot be
/// made and a mount that cannot be made private stop the runtime, named.
#[test]
fn the_directory_of_mounts_is_made_private_and_the_roots_opened() {
    let dir = scratch("fs-open");
    let (fs_, calls) = open(&dir, Paths::default());
    assert_eq!(
        calls.asked(),
        ["inspect mounts", "bind mounts", "private mounts"]
    );
    let mode = fs::metadata(dir.path().join("mounts"))
        .expect("made")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o711);
    assert_eq!(fs_.dir_of("lease"), dir.path().join("mounts/lease"));
    assert!(format!("{fs_:?}").starts_with("Fs { dir: "));
    let settings = |dir: PathBuf, calls: Mounts, roots: MountRoots| FsSettings {
        dir,
        roots,
        calls: Arc::new(calls),
        paths: Arc::new(Paths::default()),
        image_allow: Vec::new(),
    };
    // Found a mount already, it is only made private.
    let found = Mounts {
        mounted: true,
        ..Mounts::under(dir.path())
    };
    let again = settings(dir.path().join("mounts"), found, MountRoots::default());
    assert!(format!("{again:?}").starts_with("FsSettings { dir: "));
    drop(Fs::open(again).expect("opened again"));
    let gone = dir.path().join("gone");
    fs::create_dir(&gone).expect("a root");
    let roots = MountRoots::new(&[("gone".to_owned(), gone.clone())]).expect("roots");
    fs::remove_dir(&gone).expect("gone");
    let failed = Fs::open(settings(
        dir.path().join("m2"),
        Mounts::under(dir.path()),
        roots,
    ));
    assert!(matches!(failed, Err(FsError::Root { name, .. }) if name == "gone"));
    fs::write(dir.path().join("file"), b"x").expect("a file");
    let failed = Fs::open(settings(
        dir.path().join("file/m"),
        Mounts::under(dir.path()),
        MountRoots::default(),
    ));
    assert!(matches!(failed, Err(FsError::Dir { .. })));
    let unshared = Mounts {
        fail: Some("private"),
        ..Mounts::under(dir.path())
    };
    let failed = Fs::open(settings(
        dir.path().join("m3"),
        unshared,
        MountRoots::default(),
    ));
    let Err(why) = failed else {
        panic!("not private");
    };
    assert!(why.to_string().contains("make private"), "{why}");
}

/// Catches a host path opened by its text (a link out of its root followed), one of a
/// read-only root opened where it may not be, a lookup that hangs the worker, and a refusal
/// that waits for the other lookups: each path is opened beneath its root on the pool, a
/// path under no root is `input-not-allowed`, a link out of its root or out of its host path
/// (a `sub_path`) is `input-not-allowed`, a missing path is `input-unavailable` at once
/// while another is still held, and a path is opened once whatever is asked after.
#[test]
fn each_host_path_is_opened_beneath_its_root_on_the_pool() {
    let dir = scratch("fs-resolve");
    let (mut fs_, _) = open(&dir, Paths::default());
    let data = dir.path().join("data");
    symlink("/etc", data.join("out")).expect("a link out of the root");
    symlink("../../ro", data.join("tree/climbs")).expect("a link out of the host path");
    let mut sub = mount(&data.join("tree"), "/t", MountMode::ReadOnly);
    sub.sub_path = Some("sub".to_owned());
    let ok = JobFs {
        layers: vec![host_layer(&dir.path().join("ro/tree"))],
        mounts: vec![sub],
    };
    let resolved = |fs_: &mut Fs, run: &str, job: &JobFs| {
        assert_eq!(fs_.resolve((run, "lease"), "", job, 0), FsPrepared::Pending);
        until(|| match fs_.resolve((run, "lease"), "", job, 0) {
            FsPrepared::Pending => None,
            other => Some(other),
        })
    };
    assert_eq!(resolved(&mut fs_, "ok", &ok), FsPrepared::Ready);
    assert_eq!(
        fs_.resolve(("ok", "lease"), "", &ok, 0),
        FsPrepared::Ready,
        "opened once"
    );
    let refused =
        |code: &'static str, why: String| FsPrepared::Refused(NativeRefusal { code, why });
    let nowhere = job(vec![mount(
        Path::new("/nowhere"),
        "/n",
        MountMode::ReadOnly,
    )]);
    assert_eq!(
        resolved(&mut fs_, "nowhere", &nowhere),
        refused(
            refusal::INPUT_NOT_ALLOWED,
            "/nowhere lies beneath no root of this agent".to_owned()
        )
    );
    let out = job(vec![mount(&data.join("out"), "/o", MountMode::ReadOnly)]);
    assert_eq!(
        resolved(&mut fs_, "out", &out),
        refused(
            refusal::INPUT_NOT_ALLOWED,
            "out leads out of the directory it is opened beneath".to_owned()
        )
    );
    let mut climbs = mount(&data.join("tree"), "/c", MountMode::ReadOnly);
    climbs.sub_path = Some("climbs".to_owned());
    assert_eq!(
        resolved(&mut fs_, "climbs", &job(vec![climbs])),
        refused(
            refusal::INPUT_NOT_ALLOWED,
            "climbs leads out of the directory it is opened beneath".to_owned()
        )
    );
    let missing = job(vec![mount(
        &data.join("missing"),
        "/m",
        MountMode::ReadOnly,
    )]);
    assert_eq!(
        resolved(&mut fs_, "missing", &missing),
        refused(
            refusal::INPUT_UNAVAILABLE,
            "missing: No such file or directory (os error 2)".to_owned()
        )
    );
    // A file where a directory should be, a directory that cannot be searched, and a
    // `sub_path` that names a file are unavailable; a link that stays inside the root is
    // followed.
    fs::write(data.join("tree/file"), b"x").expect("a file");
    fs::create_dir_all(data.join("locked/inner")).expect("a directory");
    fs::set_permissions(data.join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");
    symlink("tree/sub", data.join("inside")).expect("a link inside the root");
    let mut file_sub = mount(&data.join("tree"), "/s", MountMode::ReadOnly);
    file_sub.sub_path = Some("file".to_owned());
    for (run, mount, why) in [
        (
            "file",
            mount(&data.join("tree/file"), "/f", MountMode::ReadOnly),
            "tree/file: Not a directory (os error 20)",
        ),
        (
            "locked",
            mount(&data.join("locked/inner"), "/l", MountMode::ReadOnly),
            "locked/inner: Permission denied (os error 13)",
        ),
        ("file-sub", file_sub, "file: Not a directory (os error 20)"),
    ] {
        let answer = resolved(&mut fs_, run, &job(vec![mount]));
        assert_eq!(answer, refused(refusal::INPUT_UNAVAILABLE, why.to_owned()), "{run}");
    }
    fs::set_permissions(data.join("locked"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let inside = job(vec![mount(&data.join("inside"), "/i", MountMode::ReadOnly)]);
    assert_eq!(resolved(&mut fs_, "inside", &inside), FsPrepared::Ready);
}

/// Catches a host path read by its text after it was looked up: once a path is open, the
/// assembly clones the directory that was opened, though the path has since been swapped
/// for a link out of its root.
#[test]
fn a_path_swapped_after_its_lookup_is_cloned_as_it_was_opened() {
    let dir = scratch("fs-swap");
    let (mut fs_, calls) = open(&dir, Paths::default());
    let data = dir.path().join("data");
    let start = job(vec![mount(&data.join("tree"), "/t", MountMode::ReadOnly)]);
    assert_eq!(fs_.resolve(("r", "lease"), "", &start, 0), FsPrepared::Pending);
    until(|| (fs_.resolve(("r", "lease"), "", &start, 0) == FsPrepared::Ready).then_some(()));
    fs::rename(data.join("tree"), data.join("moved")).expect("moved away");
    symlink("/etc", data.join("tree")).expect("a link in its place");
    let lease = dir.path().join(".lease-lease");
    fs::create_dir(&lease).expect("the lease directory");
    fs_.ask_assembly("r", (None, Vec::new()), lease, owners())
        .expect("asked");
    let made = until(|| match fs_.assembled("r", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(made, FsPrepared::Ready);
    let clones: Vec<String> = calls
        .asked()
        .into_iter()
        .filter(|call| call.starts_with("clone "))
        .collect();
    assert_eq!(clones, ["clone data/moved"]);
}

/// Catches a lookup that blocks the caller, a bound counted from when a thread took the
/// lookup instead of from when it was asked, and a pool that makes threads on demand: four
/// held lookups hold the pool's four threads, a fifth waits behind them, every one is
/// waiting before its bound and refused `input-unavailable` at it (the fifth too, though no
/// thread ever took it), on the caller's clock.
#[test]
fn held_lookups_wait_their_bound_and_a_fifth_waits_behind_them() {
    let dir = scratch("fs-held");
    let (paths, began, go) = Paths::held();
    let (mut fs_, _) = open(&dir, paths);
    let data = dir.path().join("data/tree");
    let five = job((0..5)
        .map(|n| mount(&data, &format!("/m{n}"), MountMode::ReadOnly))
        .collect());
    assert_eq!(
        fs_.resolve(("held", "lease"), "", &five, 1_000),
        FsPrepared::Pending
    );
    for _ in 0..4 {
        began.recv().expect("a lookup began");
    }
    assert!(began.try_recv().is_err(), "the fifth waits for a thread");
    assert_eq!(
        fs_.resolve(("held", "lease"), "", &five, 1_000 + LOOKUP_MS - 1),
        FsPrepared::Pending
    );
    assert_eq!(
        fs_.resolve(("held", "lease"), "", &five, 1_000 + LOOKUP_MS),
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!(
                "the lookup of {} did not answer in time (bound {LOOKUP_MS} ms)",
                data.display()
            ),
        })
    );
    drop(go);
}

/// Catches an assembly asked twice, asked before the paths are open, made without its
/// images' trees, or given to a container that did not ask: the assembly is asked once
/// with the opened sources, answers on the pool, is taken once; a run not prepared, or not
/// asked for, has none.
#[test]
fn an_assembly_is_asked_once_and_taken_once() {
    let dir = scratch("fs-assembly");
    let (mut fs_, calls) = open(&dir, Paths::default());
    let data = dir.path().join("data/tree");
    let layered = JobFs {
        layers: vec![
            Layer::Image {
                image: "registry.example/l@sha256:0".to_owned(),
            },
            host_layer(&data),
        ],
        mounts: vec![mount(&data, "/d", MountMode::ReadOnly)],
    };
    let not_prepared = FsPrepared::Failed(crate::lifecycle::RuntimeError::new(
        "run none has no file system being prepared",
    ));
    assert_eq!(fs_.assembled("none", 0), not_prepared);
    assert!(
        fs_.ask_assembly("none", (None, Vec::new()), dir.path().join("l"), owners())
            .is_err()
    );
    assert!(fs_.take("none").is_none());
    let held_dir = scratch("fs-assembly-held");
    let (paths, began, go) = Paths::held();
    let (mut held, _) = open(&held_dir, paths);
    let tree = held_dir.path().join("data/tree");
    let waiting = job(vec![mount(&tree, "/d", MountMode::ReadOnly)]);
    assert_eq!(
        held.resolve(("r", "lease"), "", &waiting, 0),
        FsPrepared::Pending
    );
    began.recv().expect("began");
    let early = held.ask_assembly("r", (None, Vec::new()), dir.path().join("l"), owners());
    assert_eq!(
        early.expect_err("not open"),
        format!("{} is not open yet", tree.display())
    );
    drop(go);
    assert_eq!(
        fs_.resolve(("r", "lease"), "", &layered, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("r", "lease"), "", &layered, 0) == FsPrepared::Ready).then_some(()));
    assert_eq!(
        fs_.assembled("r", 0),
        FsPrepared::Failed(crate::lifecycle::RuntimeError::new(
            "the assembly of run r was not asked for"
        ))
    );
    let (name, image, layers) = fs_.unassembled("r").expect("unassembled");
    assert_eq!((name, image, layers.len()), ("lease", "", 2));
    assert!(fs_.take("r").is_none(), "not assembled");
    assert_eq!(
        fs_.resolve(("r", "lease"), "", &layered, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("r", "lease"), "", &layered, 0) == FsPrepared::Ready).then_some(()));
    let lease = dir.path().join(".lease-lease");
    fs::create_dir(&lease).expect("the lease directory");
    let no_tree = fs_.ask_assembly("r", (None, Vec::new()), lease.clone(), owners());
    assert_eq!(
        no_tree.expect_err("no tree"),
        "run r: an image layer has no tree"
    );
    // The lookups were taken by the failed ask: the run is prepared again.
    fs_.discard("r", "lease").expect("discarded");
    assert_eq!(
        fs_.resolve(("r", "lease"), "", &layered, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("r", "lease"), "", &layered, 0) == FsPrepared::Ready).then_some(()));
    let layer = dir.path().join("store/layer");
    fs::create_dir_all(&layer).expect("a layer's tree");
    // A directory of mounts that cannot be made, as below a file, fails the ask.
    let mounts = dir.path().join("mounts");
    fs::rename(&mounts, dir.path().join("mounts-aside")).expect("set aside");
    fs::write(&mounts, b"x").expect("a file in its place");
    let below = fs_.ask_assembly("r", (None, vec![layer.clone()]), lease.clone(), owners());
    assert!(below.expect_err("not a directory").starts_with("make "));
    fs::remove_file(&mounts).expect("the file");
    fs::rename(dir.path().join("mounts-aside"), &mounts).expect("put back");
    fs_.discard("r", "lease").expect("discarded");
    assert_eq!(
        fs_.resolve(("r", "lease"), "", &layered, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("r", "lease"), "", &layered, 0) == FsPrepared::Ready).then_some(()));
    fs_.ask_assembly("r", (None, vec![layer]), lease, owners())
        .expect("asked");
    assert!(fs_.unassembled("r").is_none(), "asked once");
    let ready = until(|| match fs_.assembled("r", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(ready, FsPrepared::Ready);
    assert_eq!(fs_.assembled("r", 0), FsPrepared::Ready, "made, and kept");
    assert!(
        calls
            .asked()
            .contains(&"attach mounts/lease/src/0".to_owned())
    );
    assert_eq!(fs_.leases(), ["lease"]);
    assert_eq!(fs_.sources_of("lease"), 2);
    assert_eq!(fs_.sources_of("other"), 0);
    let made = fs_.take("r").expect("made");
    assert_eq!(made.sources, 2);
    assert!(fs_.take("r").is_none(), "taken once");
}

/// Catches an assembly's refusal or failure lost on its way to the lifecycle, and a discard
/// that leaves a mount: a refused assembly answers its refusal, a failed one the failure,
/// and a discard unmounts every listed point and removes the directory.
#[test]
fn an_assemblys_refusal_and_failure_reach_the_caller_and_a_discard_unmounts() {
    let dir = scratch("fs-refused");
    let paths = Paths {
        search: Some(libc::EACCES),
        ..Paths::default()
    };
    let (mut fs_, calls) = open(&dir, paths);
    let data = dir.path().join("data/tree");
    let start = job(vec![mount(&data, "/d", MountMode::ReadOnly)]);
    let lease = dir.path().join(".lease-lease");
    fs::create_dir(&lease).expect("the lease directory");
    let assembled = |fs_: &mut Fs, run: &str| {
        assert_eq!(
            fs_.resolve((run, "lease"), "", &start, 0),
            FsPrepared::Pending
        );
        until(|| (fs_.resolve((run, "lease"), "", &start, 0) == FsPrepared::Ready).then_some(()));
        fs_.ask_assembly(run, (None, Vec::new()), lease.clone(), owners())
            .expect("asked");
        until(|| match fs_.assembled(run, 0) {
            FsPrepared::Pending => None,
            other => Some(other),
        })
    };
    let FsPrepared::Refused(refused) = assembled(&mut fs_, "r") else {
        panic!("refused");
    };
    assert_eq!(refused.code, refusal::INPUT_UNAVAILABLE);
    fs_.discard("r", "lease").expect("discarded");
    assert!(
        calls
            .asked()
            .contains(&"detach mounts/lease/src/0".to_owned())
    );
    assert!(!dir.path().join("mounts/lease").exists());
    let failing = scratch("fs-failed");
    let (mut fs_, _) = open(&failing, Paths::default());
    let calls = Mounts {
        fail: Some("clone"),
        ..Mounts::under(failing.path())
    };
    fs_.calls = Arc::new(calls);
    let data = failing.path().join("data/tree");
    let start = job(vec![mount(&data, "/d", MountMode::ReadOnly)]);
    let lease = failing.path().join(".lease-lease");
    fs::create_dir(&lease).expect("the lease directory");
    assert_eq!(
        fs_.resolve(("f", "lease"), "", &start, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("f", "lease"), "", &start, 0) == FsPrepared::Ready).then_some(()));
    fs_.ask_assembly("f", (None, Vec::new()), lease, owners())
        .expect("asked");
    let failed = until(|| match fs_.assembled("f", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    let FsPrepared::Failed(why) = failed else {
        panic!("failed: {failed:?}");
    };
    assert!(why.to_string().starts_with("clone the tree for "), "{why}");
    // An assembly whose thread ends without an answer is the host's failure, named.
    fs_.calls = Arc::new(Mounts {
        panics: true,
        ..Mounts::under(failing.path())
    });
    assert_eq!(
        fs_.resolve(("p", "lost"), "", &start, 0),
        FsPrepared::Pending
    );
    until(|| (fs_.resolve(("p", "lost"), "", &start, 0) == FsPrepared::Ready).then_some(()));
    let lease = failing.path().join(".lease-lost");
    fs::create_dir(&lease).expect("the lease directory");
    fs_.ask_assembly("p", (None, Vec::new()), lease, owners())
        .expect("asked");
    let lost = until(|| match fs_.assembled("p", 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    assert_eq!(
        lost,
        FsPrepared::Failed(crate::lifecycle::RuntimeError::new(
            "the assembly of run p ended without an answer"
        ))
    );
}

/// Catches a check that passes over a source whose file system has gone, one that blocks
/// the caller, and one that never gives up: each source is asked on the pool; a source that
/// answers `ENOTCONN` is named; one that does not answer is unanswered at the caller's bound.
#[test]
fn the_check_after_a_job_names_a_source_that_has_gone() {
    let dir = scratch("fs-check");
    let (fs_, _) = open(&dir, Paths::default());
    for i in 0..2 {
        fs::create_dir_all(dir.path().join(format!("mounts/lease/src/{i}"))).expect("a clone");
    }
    let fine = fs_.check("lease", 2, 0);
    assert_eq!(
        until(|| match fine.poll(0) {
            Poll::Answered(found) => Some(found),
            _ => None,
        }),
        None
    );
    let gone = scratch("fs-check-gone");
    let (fs_, _) = open(
        &gone,
        Paths {
            answer: Some(libc::ENOTCONN),
            ..Paths::default()
        },
    );
    let check = fs_.check("lease", 1, 0);
    let found = until(|| match check.poll(0) {
        Poll::Answered(found) => Some(found),
        _ => None,
    });
    let point = gone.path().join("mounts/lease/src/0");
    assert_eq!(
        found,
        Some(format!(
            "{}: Transport endpoint is not connected (os error 107)",
            point.display()
        ))
    );
    let (paths, began, go) = Paths::held();
    let held = scratch("fs-check-held");
    let (fs_, _) = open(&held, paths);
    let check = fs_.check("lease", 1, 10);
    began.recv().expect("began");
    assert_eq!(check.poll(10 + CHECK_MS - 1), Poll::Waiting);
    assert_eq!(
        check.poll(10 + CHECK_MS),
        Poll::Unanswered("did not answer in time")
    );
    drop(go);
}
