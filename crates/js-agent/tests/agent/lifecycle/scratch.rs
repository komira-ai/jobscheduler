//! Removing a run's scratch directory whatever the job left in it, and sweeping the
//! directories no kept run owns when the agent starts.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};

use js_agent::lifecycle::label;

use super::{Harness, job, names};

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Asserts that a subdirectory of mode 0 holding files, and a read-only file in a
/// read-only directory, do not stop the removal. It catches a removal that cannot enter
/// a directory it has no permission on or cannot unlink in one it cannot write.
#[test]
fn removes_directories_the_job_made_unreadable() {
    let h = Harness::new("unreadable");
    let dir = h.config.scratch.create("r1").unwrap();
    let closed = dir.join("closed");
    fs::create_dir_all(closed.join("inner")).unwrap();
    fs::write(closed.join("a"), "a").unwrap();
    fs::write(closed.join("inner").join("b"), "b").unwrap();
    let frozen = dir.join("frozen");
    fs::create_dir(&frozen).unwrap();
    fs::write(frozen.join("c"), "c").unwrap();
    chmod(&frozen.join("c"), 0o400);
    chmod(&frozen, 0o500);
    chmod(&closed.join("inner"), 0o000);
    chmod(&closed, 0o000);

    h.config.scratch.remove("r1").unwrap();
    assert!(!dir.exists());
    assert!(h.config.scratch.path().is_dir());
}

/// Asserts that a tree far deeper than a path can be (and than the stack allows
/// recursion) is removed. It catches a recursive removal and one that names every
/// directory by its full path.
#[test]
fn removes_a_tree_ten_thousand_levels_deep() {
    let h = Harness::new("deep");
    let dir = h.config.scratch.create("r1").unwrap();
    let mut cur = openat(
        CWD,
        &dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    for _ in 0..10_000 {
        mkdirat(&cur, "d", Mode::RWXU).unwrap();
        cur = openat(
            &cur,
            "d",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
    }
    drop(cur);

    h.config.scratch.remove("r1").unwrap();
    assert!(!dir.exists());
}

/// Asserts that a link inside the tree is removed and never followed: the directory it
/// points at, outside the tree, keeps its files and its mode. It catches a removal that
/// descends through a link or changes the mode of what a link points to.
#[test]
fn links_are_removed_not_followed() {
    let h = Harness::new("links");
    let outside = h.dir.path("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "keep").unwrap();
    chmod(&outside, 0o750);
    let file = h.dir.path("outside-file");
    fs::write(&file, "x").unwrap();
    chmod(&file, 0o640);

    let dir = h.config.scratch.create("r1").unwrap();
    symlink(&outside, dir.join("to-dir")).unwrap();
    symlink(&file, dir.join("to-file")).unwrap();
    symlink("nowhere", dir.join("dangling")).unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    symlink(&outside, dir.join("sub").join("again")).unwrap();
    chmod(&dir.join("sub"), 0o500);

    h.config.scratch.remove("r1").unwrap();
    assert!(!dir.exists());
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "keep");
    assert_eq!(mode_of(&outside), 0o750);
    assert_eq!(mode_of(&file), 0o640);
}

/// Asserts that a run whose job left a mode-0 directory still reaches "scratch removed"
/// and does not taint the host. It catches a cleanup that fails, and so taints the
/// host, for what the job did to its own scratch directory.
#[test]
fn cleanup_does_not_taint_the_host_for_a_closed_directory() {
    let mut h = Harness::new("cleanup-closed");
    h.start(job("r1"));
    let closed = h.scratch_of("r1").join("closed");
    fs::create_dir(&closed).unwrap();
    fs::write(closed.join("a"), "a").unwrap();
    chmod(&closed, 0o000);
    h.fake.exit("r1", 0);

    let events = names(&h.life.step());
    assert!(
        events.contains(&"scratch removed r1".to_owned()),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| e.starts_with("tainted")),
        "{events:?}"
    );
    assert!(!h.scratch_of("r1").exists());
}

/// Asserts that a restart removes the scratch directories no adopted run owns (a
/// directory with a closed subdirectory, a stray file and a link whose target is outside
/// the root) and leaves the adopted run's directory and its content, and the target of
/// the link. It catches a start that leaves the directories of dead runs behind, and a
/// sweep that removes a kept run's directory or follows a link.
#[test]
fn restart_removes_orphan_scratch_and_keeps_adopted_runs() {
    let mut h = Harness::new("sweep");
    h.start(job("r1"));
    let kept = h.scratch_of("r1");
    fs::write(kept.join("work"), "w").unwrap();
    let root = kept.parent().unwrap().to_owned();

    let orphan = root.join("gone");
    fs::create_dir_all(orphan.join("closed")).unwrap();
    fs::write(orphan.join("closed").join("a"), "a").unwrap();
    chmod(&orphan.join("closed"), 0o000);
    fs::write(root.join("stray-file"), "s").unwrap();
    let outside = h.dir.path("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "keep").unwrap();
    symlink(&outside, root.join("link")).unwrap();

    let events = h.restart().unwrap();
    assert_eq!(names(&events), ["adopted r1"]);
    assert!(!orphan.exists());
    assert!(!root.join("stray-file").exists());
    assert!(fs::symlink_metadata(root.join("link")).is_err());
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "keep");
    assert_eq!(fs::read_to_string(kept.join("work")).unwrap(), "w");
    assert!(root.is_dir());
}

/// Asserts that a run id that is not one normal path component (empty, `.`, `..`, or
/// holding a `/`) is refused and removes nothing: the file and directory beside the
/// run's directory survive. It catches a removal that follows a run id out of the root.
#[test]
fn a_run_id_that_is_not_one_path_component_is_refused() {
    let h = Harness::new("bad-id");
    let root = h.config.scratch.path().to_owned();
    fs::create_dir_all(root.join("a").join("b")).unwrap();
    fs::write(root.join("a").join("b").join("f"), "f").unwrap();
    fs::write(root.join("file"), "f").unwrap();
    for bad in ["", ".", "..", "a/b", "/a", "a/", "../scratch/a"] {
        let why = h.config.scratch.remove(bad).unwrap_err();
        assert_eq!(why.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
    assert!(root.join("a").join("b").join("f").is_file());
    assert!(root.join("file").is_file());
}

/// Asserts that the directory of a container that could not be removed (it may still
/// run) is not swept at a restart, and is swept at the restart after the container is
/// gone, while the directory of a container that was evicted and removed is swept at
/// once. It catches a sweep that removes scratch from under a live container.
#[test]
fn restart_keeps_the_scratch_of_a_container_it_could_not_remove() {
    let mut h = Harness::new("sweep-stuck");
    let root = h.config.scratch.path().to_owned();
    let odd = [
        (label::RUN, "odd"),
        (label::LEASE, "a b"),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "0"),
        (label::GPUS, "1"),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ];
    h.fake.leak(&odd, 0);
    h.fake.fail("remove", 1);
    fs::create_dir(root.join("odd")).unwrap();
    fs::write(root.join("odd").join("f"), "f").unwrap();

    h.restart().unwrap();
    assert!(
        root.join("odd").join("f").is_file(),
        "the container is still there"
    );

    h.life.step();
    assert!(h.fake.existing().is_empty(), "the retry removed it");
    h.restart().unwrap();
    assert!(!root.join("odd").exists());
}

/// Asserts that a sweep of a root that is gone removes nothing, reports 0 and makes
/// nothing. It catches a sweep that fails or creates the root.
#[test]
fn sweep_of_a_missing_root_does_nothing() {
    let h = Harness::new("sweep-missing");
    let root = h.config.scratch.path().to_owned();
    fs::remove_dir(&root).unwrap();
    assert_eq!(h.config.scratch.sweep(["r1"]), 0);
    assert!(!root.exists());
}
