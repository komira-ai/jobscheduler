//! [`Fs`]: the directory of the runs' mounts and the roots opened when the agent starts, the
//! lookups of host paths on the pool, an assembly asked once and taken once, a discard, and
//! the check after a job, with the fakes of [`super::tests`].

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;

use js_agent_wire::{JobMount, Layer, MountMode, refusal};
use js_testkit::dir::TestDir;

use super::tests::{GUARD, Mounts, Paths, host_layer, mount, owners, scratch, until};
use super::{CHECK_MS, Fs, FsError, FsSettings, LOOKUP_MS};
use crate::lifecycle::{FsPrepared, JobFs, MountRoots, NativeRefusal};
use crate::pool::Poll;
use crate::pool::THREADS;

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

/// Catches a directory of mounts under a parent that only root may search: on a node the
/// parent, `/run/js-agent`, is made first by the subordinate-id claim, mode `0700`, so
/// neither the job user (which runs `crun`) nor the job's own ids could search down to a
/// clone, and every job with a mount was refused `input-unavailable` ("Permission denied").
/// Opening the runs' directory makes it and its parent `0711`, an existing parent too.
#[test]
fn a_parent_only_root_could_search_is_opened_to_search() {
    let dir = scratch("fs-open-parent");
    let parent = dir.path().join("parent");
    fs::create_dir(&parent).expect("a parent");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("closed");
    let settings = FsSettings {
        dir: parent.join("mounts"),
        roots: MountRoots::default(),
        calls: Arc::new(Mounts::under(dir.path())),
        paths: Arc::new(Paths::default()),
        image_allow: Vec::new(),
    };
    drop(Fs::open(settings).expect("opened"));
    let mode = |path: &Path| fs::metadata(path).expect("there").permissions().mode() & 0o777;
    assert_eq!(mode(&parent), 0o711);
    assert_eq!(mode(&parent.join("mounts")), 0o711);
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
            format!(
                "the host path {} (root data): out leads out of the directory it is opened beneath",
                data.join("out").display()
            )
        )
    );
    let mut climbs = mount(&data.join("tree"), "/c", MountMode::ReadOnly);
    climbs.sub_path = Some("climbs".to_owned());
    assert_eq!(
        resolved(&mut fs_, "climbs", &job(vec![climbs])),
        refused(
            refusal::INPUT_NOT_ALLOWED,
            format!(
                "the sub_path climbs of the host path {} (root data): climbs leads out of the \
                 directory it is opened beneath",
                data.join("tree").display()
            )
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
            format!(
                "the host path {} (root data): missing: No such file or directory (os error 2)",
                data.join("missing").display()
            )
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
            format!(
                "the host path {} (root data): tree/file: Not a directory (os error 20)",
                data.join("tree/file").display()
            ),
        ),
        (
            "locked",
            mount(&data.join("locked/inner"), "/l", MountMode::ReadOnly),
            format!(
                "the host path {} (root data): locked/inner: Permission denied (os error 13)",
                data.join("locked/inner").display()
            ),
        ),
        (
            "file-sub",
            file_sub,
            format!(
                "the sub_path file of the host path {} (root data): file: Not a directory \
                 (os error 20)",
                data.join("tree").display()
            ),
        ),
    ] {
        let answer = resolved(&mut fs_, run, &job(vec![mount]));
        assert_eq!(answer, refused(refusal::INPUT_UNAVAILABLE, why), "{run}");
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
    assert_eq!(made.0.sources, 2);
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

/// Asks until the lookups of the run `run` have an answer other than pending.
fn resolved_run(fs_: &mut Fs, run: &str, image: &str, job: &JobFs) -> FsPrepared {
    until(|| match fs_.resolve((run, "lease"), image, job, 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    })
}

/// Paths that count their opens, and the counter.
fn counting() -> (Paths, Arc<AtomicUsize>) {
    let paths = Paths::default();
    let opens = Arc::clone(&paths.opens);
    (paths, opens)
}

/// Catches a start with nothing to look up waiting for the worker's next step: the first
/// call answers `Ready` with no task run on the pool (a held pool would never answer), and
/// asking again changes nothing.
#[test]
fn a_start_with_no_host_path_is_ready_at_the_first_call() {
    let dir = scratch("fs-none");
    let (paths, began, go) = Paths::held();
    let (mut fs_, _) = open(&dir, paths);
    let layers = JobFs {
        layers: vec![Layer::Image {
            image: "registry.example/l@sha256:0".to_owned(),
        }],
        mounts: Vec::new(),
    };
    let answer = fs_.resolve(("r", "lease"), "", &layers, 0);
    assert_eq!(answer, FsPrepared::Ready);
    assert_eq!(
        fs_.resolve(("r", "lease"), "", &layers, 0),
        FsPrepared::Ready
    );
    assert!(began.try_recv().is_err(), "no lookup was asked");
    drop(go);
}

/// Catches the image of a start with neither layers nor mounts being lost: the first call
/// of such a start answers `Ready` at once and the assembly is given the start's image (a
/// lost one would assemble a root without it).
#[test]
fn a_start_with_an_image_and_no_layers_keeps_its_image_for_the_assembly() {
    let dir = scratch("fs-image");
    let (paths, began, go) = Paths::held();
    let (mut fs_, _) = open(&dir, paths);
    let image = "registry.example/i@sha256:1";
    let answer = fs_.resolve(("r", "lease"), image, &JobFs::default(), 0);
    assert_eq!(answer, FsPrepared::Ready);
    let (name, kept, layers) = fs_.unassembled("r").expect("unassembled");
    assert_eq!((name, kept, layers), ("lease", image, &[][..]));
    assert!(began.try_recv().is_err(), "no lookup was asked");
    drop(go);
}

/// Catches a refusal of a missing host path that names only the part below its root (a
/// tree named by its digest alone, with no word of the root or the path the start gave):
/// a start whose read-only layer names a directory not yet written beneath its root is
/// refused `input-unavailable` with the host path in full and the root's name, after one
/// open, a mount of the same kind is named the same way, a missing `sub_path` of a host path
/// that is there is named with that host path, and a tree written after a start's refusal is
/// found by a later start.
#[test]
fn a_missing_host_path_is_refused_by_its_full_path_and_root_after_one_open() {
    let dir = scratch("fs-missing-named");
    let (paths, opens) = counting();
    let (mut fs_, _) = open(&dir, paths);
    let tree = dir.path().join("ro/85a0");
    let layer = JobFs {
        layers: vec![host_layer(&tree)],
        mounts: Vec::new(),
    };
    let missing = |rest: &str, path: &Path| {
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!(
                "the host path {} (root ro): {rest}: No such file or directory (os error 2)",
                path.display()
            ),
        })
    };
    assert_eq!(
        resolved_run(&mut fs_, "layer", "", &layer),
        missing("85a0", &tree)
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1, "one open of the layer");
    let deep = dir.path().join("ro/tree/sub/gone");
    let mounted = job(vec![mount(&deep, "/g", MountMode::ReadOnly)]);
    assert_eq!(
        resolved_run(&mut fs_, "mount", "", &mounted),
        missing("tree/sub/gone", &deep)
    );
    assert_eq!(opens.load(Ordering::SeqCst), 2, "one open of the mount");
    let there = dir.path().join("ro/tree");
    let mut gone_sub = mount(&there, "/s", MountMode::ReadOnly);
    gone_sub.sub_path = Some("gone".to_owned());
    assert_eq!(
        resolved_run(&mut fs_, "sub", "", &job(vec![gone_sub])),
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!(
                "the sub_path gone of the host path {} (root ro): gone: No such file or \
                 directory (os error 2)",
                there.display()
            ),
        })
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        4,
        "the host path, then its sub_path"
    );
    fs::create_dir(&tree).expect("the tree written");
    assert_eq!(
        resolved_run(&mut fs_, "later", "", &layer),
        FsPrepared::Ready,
        "a start after the tree is written finds it"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        5,
        "one open of the later start"
    );
}

/// Catches a later call that looks up again what was begun, or loses its order: the first
/// call asks one host path, the second adds a provider's directory as a layer and a mount;
/// only the two new paths are opened, the first is kept, the assembly gets the image of the
/// second call, and receives the sources in the order of the final fs (layer, then the
/// mounts as listed).
#[test]
fn a_later_call_asks_only_the_host_paths_it_adds_and_keeps_their_order() {
    let dir = scratch("fs-grow");
    let (paths, opens) = counting();
    let (mut fs_, calls) = open(&dir, paths);
    let (data, ro) = (dir.path().join("data/tree"), dir.path().join("ro/tree"));
    let first = job(vec![mount(&data, "/a", MountMode::ReadOnly)]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &first), FsPrepared::Ready);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let grown = JobFs {
        layers: vec![host_layer(&dir.path().join("data/tree/sub"))],
        mounts: vec![
            mount(&data, "/a", MountMode::ReadOnly),
            mount(&ro, "/b", MountMode::ReadOnly),
        ],
    };
    let answer = resolved_run(&mut fs_, "r", "img", &grown);
    assert_eq!(answer, FsPrepared::Ready);
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "the layer and the mount were opened once each"
    );
    let (_, image, layers) = fs_.unassembled("r").expect("unassembled");
    assert_eq!((image, layers.len()), ("img", 1));
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
    assert_eq!(
        clones,
        ["clone data/tree/sub", "clone data/tree", "clone ro/tree"]
    );
}

/// Catches a lookup kept for a source the new fs no longer names, and a lookup matched
/// twice: a later call without the first source asks nothing again; two sources of one path
/// are two lookups, each matched once; a path with a `sub_path` is another lookup.
#[test]
fn a_later_call_drops_the_lookups_it_no_longer_names() {
    let dir = scratch("fs-drop");
    let (paths, opens) = counting();
    let (mut fs_, _) = open(&dir, paths);
    let (a, b) = (dir.path().join("data/tree"), dir.path().join("ro/tree"));
    let both = job(vec![
        mount(&a, "/a", MountMode::ReadOnly),
        mount(&b, "/b", MountMode::ReadOnly),
    ]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &both), FsPrepared::Ready);
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    let only_b = job(vec![mount(&b, "/b", MountMode::ReadOnly)]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &only_b), FsPrepared::Ready);
    assert_eq!(opens.load(Ordering::SeqCst), 2, "the kept one stays");
    let twice = job(vec![
        mount(&b, "/b", MountMode::ReadOnly),
        mount(&b, "/c", MountMode::ReadOnly),
    ]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &twice), FsPrepared::Ready);
    assert_eq!(opens.load(Ordering::SeqCst), 3, "one lookup for one source");
    let mut under = mount(&b, "/b", MountMode::ReadOnly);
    under.sub_path = Some("sub".to_owned());
    let moved = job(vec![under]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &moved), FsPrepared::Ready);
    assert_eq!(
        opens.load(Ordering::SeqCst),
        5,
        "a sub path is another lookup"
    );
}

/// Catches a refusal lost among the lookups a later call adds: a new host path beneath no
/// root is refused `input-not-allowed`, though the lookup kept from the first call is open.
#[test]
fn a_refused_lookup_among_the_new_ones_is_the_answer() {
    let dir = scratch("fs-grow-refused");
    let (mut fs_, _) = open(&dir, Paths::default());
    let data = dir.path().join("data/tree");
    let first = job(vec![mount(&data, "/a", MountMode::ReadOnly)]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &first), FsPrepared::Ready);
    let grown = job(vec![
        mount(&data, "/a", MountMode::ReadOnly),
        mount(Path::new("/nowhere"), "/n", MountMode::ReadOnly),
    ]);
    assert_eq!(
        resolved_run(&mut fs_, "r", "", &grown),
        FsPrepared::Refused(NativeRefusal {
            code: refusal::INPUT_NOT_ALLOWED,
            why: "/nowhere lies beneath no root of this agent".to_owned(),
        })
    );
}

/// Catches a call with the fs it was prepared with that asks again: an open path is not
/// opened again however often the same fs is asked for.
#[test]
fn a_call_with_an_unchanged_fs_only_polls() {
    let dir = scratch("fs-same");
    let (paths, opens) = counting();
    let (mut fs_, _) = open(&dir, paths);
    let data = dir.path().join("data/tree");
    let start = job(vec![mount(&data, "/a", MountMode::ReadOnly)]);
    assert_eq!(resolved_run(&mut fs_, "r", "", &start), FsPrepared::Ready);
    for _ in 0..3 {
        let answer = fs_.resolve(("r", "lease"), "", &start, 0);
        assert_eq!(answer, FsPrepared::Ready);
    }
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

/// Asks the pool for the assembly of the run `run`, once its lookups have answered: the run's
/// lease is `lease-<run>`, and `start` its host paths.
fn assembly_asked(fs_: &mut Fs, dir: &TestDir, run: &str, start: &JobFs) {
    let lease_name = format!("lease-{run}");
    until(|| match fs_.resolve((run, &lease_name), "", start, 0) {
        FsPrepared::Pending => None,
        other => Some(other),
    });
    let lease = dir.path().join(format!(".{lease_name}"));
    fs::create_dir(&lease).expect("the lease directory");
    fs_.ask_assembly(run, (None, Vec::new()), lease, owners())
        .expect("asked");
}

/// Catches the lookups of a start sharing their threads with the assemblies and the checks
/// of others (a start's `admitted` to `resolved` queued behind another's mounts, and held
/// for ever behind a provider's file system that hung in an assembly): with every thread
/// of the assemblies' pool held in an assembly's clone, another assembly and a check queue
/// behind them, and a start's lookups still answer, ring the wake, and are `Ready`; once the
/// assemblies are let go, each answers `Ready` and the check answers.
#[test]
fn lookups_answer_while_every_thread_of_the_assemblies_is_held() {
    let dir = scratch("fs-lookups-apart");
    let (mut fs_, _) = open(&dir, Paths::default());
    let (held, began, go) = Mounts::holding_clones(dir.path());
    fs_.calls = Arc::new(held);
    let (rung, rings) = channel::<()>();
    let rung = std::sync::Mutex::new(rung);
    fs_.set_wake(crate::lifecycle::Wake::new(move || {
        let _ = rung.lock().map(|rung| rung.send(()));
    }));
    let tree = dir.path().join("data/tree");
    let start = job(vec![mount(&tree, "/t", MountMode::ReadOnly)]);
    let runs: Vec<String> = (0..=THREADS).map(|n| format!("run-{n}")).collect();
    for run in &runs[..THREADS] {
        assembly_asked(&mut fs_, &dir, run, &start);
        began.recv().expect("an assembly began its clone");
        // The ring of this run's lookup, which answered before its assembly was asked.
        rings.recv_timeout(GUARD).expect("the lookup rang");
    }
    // One more assembly and a check: no thread of the pool is free for them.
    assembly_asked(&mut fs_, &dir, &runs[THREADS], &start);
    rings.recv_timeout(GUARD).expect("its lookup rang");
    let check = fs_.check("lease-run-0", 1, 0);
    assert!(began.try_recv().is_err(), "the fifth assembly waits");
    assert_eq!(check.poll(0), Poll::Waiting, "the check waits");
    let ro_tree = dir.path().join("ro/tree");
    let later = job(vec![mount(&ro_tree, "/l", MountMode::ReadOnly)]);
    let answer = until(
        || match fs_.resolve(("later", "lease-later"), "", &later, 0) {
            FsPrepared::Pending => None,
            other => Some(other),
        },
    );
    assert_eq!(
        answer,
        FsPrepared::Ready,
        "a lookup is not behind an assembly"
    );
    rings.recv_timeout(GUARD).expect("the lookup rang the wake");
    drop(go);
    for run in &runs {
        let made = until(|| match fs_.assembled(run, 0) {
            FsPrepared::Pending => None,
            other => Some(other),
        });
        assert_eq!(made, FsPrepared::Ready, "{run}");
    }
    let checked = until(|| match check.poll(0) {
        Poll::Waiting => None,
        other => Some(other),
    });
    assert_eq!(checked, Poll::Answered(None));
}

/// Catches the lookups of a start sharing their threads with the checks after other jobs (a
/// source whose file system hung, checked after each job that failed, holding every thread):
/// with every thread of the assemblies' pool held in a check and another check queued behind
/// them, a start's lookups answer `Ready`; once the checks are let go each answers.
#[test]
fn lookups_answer_while_every_thread_of_the_checks_is_held() {
    let dir = scratch("fs-lookups-apart-checks");
    let (paths, began, go) = Paths::holding_checks();
    let (mut fs_, _) = open(&dir, paths);
    let checks: Vec<_> = (0..=THREADS).map(|_| fs_.check("lease", 1, 0)).collect();
    for _ in 0..THREADS {
        began.recv().expect("a check began");
    }
    assert!(began.try_recv().is_err(), "the fifth check waits");
    let tree = dir.path().join("data/tree");
    let start = job(vec![mount(&tree, "/t", MountMode::ReadOnly)]);
    assert_eq!(
        resolved_run(&mut fs_, "r", "", &start),
        FsPrepared::Ready,
        "a lookup is not behind a check"
    );
    assert!(checks.iter().all(|check| check.poll(0) == Poll::Waiting));
    drop(go);
    for check in &checks {
        let answer = until(|| match check.poll(0) {
            Poll::Waiting => None,
            other => Some(other),
        });
        assert_eq!(answer, Poll::Answered(None));
    }
}
