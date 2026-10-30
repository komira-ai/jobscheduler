//! Tests that need a real XFS file system mounted with `prjquota`. They are `#[ignore]`d:
//! they set project ids and limits on a real device, so they run only on a provisioned
//! host, never on a development machine or a shared runner.
//!
//! To run them, on a host whose scratch volume is XFS mounted `prjquota` (and a second
//! directory on another kind of file system for the last test), as root:
//!
//! ```text
//! JS_QUOTA_XFS_ROOT=/var/lib/js-agent/scratch-test \
//! JS_QUOTA_OTHER_ROOT=/dev/shm \
//!     cargo test -p js-agent --test agent quota::xfs_host -- --ignored --test-threads=1
//! ```
//!
//! `JS_QUOTA_XFS_ROOT` is an existing empty directory on the XFS volume, owned by root,
//! mode 0700. Each test uses a slot of its own and removes what it made. These variables
//! are read by the tests only; the agent reads none.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use js_agent::quota::{Mode, ProjectId, ProjectIds, ScratchQuota, Usage, XfsQuota, probe};

const MIB: u64 = 1 << 20;

fn root_of(var: &str) -> PathBuf {
    let path = std::env::var_os(var).unwrap_or_else(|| panic!("set {var}: see the module docs"));
    fs::canonicalize(path).unwrap()
}

/// The quota of the XFS root, which must probe to project.
fn host() -> (XfsQuota, PathBuf) {
    let root = root_of("JS_QUOTA_XFS_ROOT");
    let quota = XfsQuota::new(root.clone(), ProjectIds::default());
    let probe = quota.probe();
    assert_eq!(probe.mode, Mode::Project, "{probe}");
    (quota, root)
}

/// A new empty run directory and the project of `slot`, which must read zero.
fn fresh(quota: &XfsQuota, root: &Path, slot: u32, name: &str) -> (PathBuf, ProjectId) {
    let project = ProjectIds::default().claim(quota, slot).unwrap();
    let dir = root.join(format!("{name}-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    (dir, project)
}

/// Writes `total` bytes in 64 KiB writes to a file in `dir` and syncs it.
fn write_file(dir: &Path, name: &str, total: u64) -> io::Result<()> {
    let mut file = File::create(dir.join(name))?;
    let chunk = vec![0xAB; 64 * 1024];
    for _ in 0..total / chunk.len() as u64 {
        file.write_all(&chunk)?;
    }
    file.sync_all()
}

fn is_edquot(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EDQUOT)
}

/// Removes the tree, clears the project, and waits for the usage to read zero (the file
/// system frees inodes in the background).
fn finish(quota: &XfsQuota, dir: &Path, project: ProjectId) {
    fs::remove_dir_all(dir).unwrap();
    quota.clear(dir, project).unwrap();
    for _ in 0..100 {
        if quota.usage(project).unwrap().is_zero() {
            return;
        }
        sleep(Duration::from_millis(100));
    }
    panic!(
        "usage of {project} did not read zero: {:?}",
        quota.usage(project)
    );
}

/// A write past the limit gets `EDQUOT` and stops at the limit, a neighbour run under its
/// own project is not affected, and the usage reads what was written. It catches a limit
/// that is not enforced, one counted in the wrong unit, and one shared between projects.
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn project_quota_enforced() {
    let (quota, root) = host();
    let (small, small_id) = fresh(&quota, &root, 100, "small");
    let (big, big_id) = fresh(&quota, &root, 101, "big");
    quota.apply(&small, small_id, MIB, 1000).unwrap();
    quota.apply(&big, big_id, 16 * MIB, 1000).unwrap();

    let over = write_file(&small, "fill", 4 * MIB).unwrap_err();
    assert!(is_edquot(&over), "{over}");
    let held = quota.usage(small_id).unwrap();
    assert!(held.bytes <= MIB && held.bytes > 0, "{held:?}");

    write_file(&big, "fill", 4 * MIB).unwrap();
    assert!(quota.usage(big_id).unwrap().bytes >= 4 * MIB);
    finish(&quota, &small, small_id);
    finish(&quota, &big, big_id);
}

/// A flood of empty files stops at the inode limit with `EDQUOT`. It catches an inode limit
/// that is not set (a job could exhaust the shared inode table).
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn inode_flood_hits_limit() {
    let (quota, root) = host();
    let (dir, project) = fresh(&quota, &root, 102, "flood");
    quota.apply(&dir, project, 64 * MIB, 100).unwrap();
    let mut made = 0;
    let stopped = loop {
        match File::create(dir.join(format!("f{made}"))) {
            Ok(_) => made += 1,
            Err(err) => break err,
        }
        assert!(made <= 100, "the inode limit did not stop the flood");
    };
    assert!(is_edquot(&stopped), "{stopped}");
    assert!((50..100).contains(&made), "{made} files before the limit");
    finish(&quota, &dir, project);
}

/// Files made after the apply are counted whoever owns the directory (the agent chowns it
/// to the slot's user afterwards), and a directory that already holds a file is refused,
/// because that file would carry project 0 and escape the count. It catches an apply that
/// comes after the chown or the first write.
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn project_set_before_chown() {
    let (quota, root) = host();
    let (dir, project) = fresh(&quota, &root, 103, "chown");
    quota.apply(&dir, project, 64 * MIB, 1000).unwrap();
    std::os::unix::fs::chown(&dir, Some(40_000), Some(40_000)).unwrap();
    write_file(&dir, "after", 2 * MIB).unwrap();
    assert!(quota.usage(project).unwrap().bytes >= 2 * MIB);
    finish(&quota, &dir, project);

    let (early, other) = fresh(&quota, &root, 104, "early");
    write_file(&early, "before", 2 * MIB).unwrap();
    let refused = quota.apply(&early, other, 64 * MIB, 1000);
    assert!(refused.is_err());
    assert_eq!(
        quota.usage(other).unwrap(),
        Usage::ZERO,
        "the file escaped the count"
    );
    fs::remove_dir_all(&early).unwrap();
}

/// After the cleanup the project has no limits and no usage, a directory that outlives the
/// clear is ordinary again (its writes are not counted), and the id can be claimed again.
/// It catches a limit or a project id left behind.
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn cleanup_clears_the_project_and_the_limit() {
    let (quota, root) = host();
    let (dir, project) = fresh(&quota, &root, 105, "clean");
    quota.apply(&dir, project, MIB, 100).unwrap();
    assert_eq!(quota.limits(project).unwrap(), (MIB, 100));
    write_file(&dir, "out", MIB / 2).unwrap();
    finish(&quota, &dir, project);
    assert_eq!(quota.limits(project).unwrap(), (0, 0));

    let (kept, project) = fresh(&quota, &root, 105, "kept");
    quota.apply(&kept, project, MIB, 100).unwrap();
    quota.clear(&kept, project).unwrap();
    write_file(&kept, "free", 4 * MIB).unwrap();
    assert_eq!(
        quota.usage(project).unwrap(),
        Usage::ZERO,
        "the directory kept its project"
    );
    fs::remove_dir_all(&kept).unwrap();
}

/// A root on another kind of file system probes to none, with the file system named.
///
/// Run with `JS_QUOTA_OTHER_ROOT` set to a directory on a tmpfs or ext4 volume; see the
/// module documentation.
#[test]
#[ignore = "needs JS_QUOTA_OTHER_ROOT: see the module docs"]
fn unsupported_fs_probe_is_none() {
    let root = root_of("JS_QUOTA_OTHER_ROOT");
    let found = probe(&root);
    assert_eq!(found.mode, Mode::None, "{found}");
    assert!(found.reason.contains("not xfs"), "{found}");
}
