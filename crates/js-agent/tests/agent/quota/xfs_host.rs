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
//! or, with the test binary built by `.github/workflows/js-agent-quota-tests.yml` (same
//! variables):
//!
//! ```text
//! ./agent --ignored --test-threads=1 quota::xfs_host
//! ```
//!
//! `JS_QUOTA_XFS_ROOT` is an existing empty directory on the XFS volume, owned by root,
//! mode 0700. Each test uses a slot of its own (the tests use slots 100 to 105) and removes
//! what it made, also when it fails: a guard removes the run directory and clears the
//! project on drop, even while panicking. These variables are read by the tests only; the
//! agent reads none.
//!
//! **Leftovers of a failed run are the operator's to clear.** A run that is killed (not
//! failed) leaves directories with their project ids and limits set, and `ProjectIds::claim`
//! then refuses those slots with `InUse`. The tests do not weaken `claim` to get past that:
//! clear the project of slots 100 to 105 first (the held run script does it), then rerun.
//!
//! A rerun right after a failed run may need to wait a few seconds: XFS frees inodes in the
//! background, and a `claim` made before that finishes can briefly see `InUse`.
//!
//! **What a job sees at a project quota.** XFS reports a hit on a *project* quota as
//! `ENOSPC` ("No space left on device"): the project looks like a full file system. User
//! and group quotas give `EDQUOT`. The tests accept either, and prove it was the quota and
//! not a full file system (usage at the limit, and writes outside the project still work).

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

/// A run directory and its project, cleaned up when dropped. On the success path
/// [`Run::finish`] also waits for the usage to read zero. On a failure (a panic) the drop
/// removes the tree and clears the project so that a rerun does not meet `InUse`; errors
/// are ignored then, because the test is already failing.
struct Run<'a> {
    quota: &'a XfsQuota,
    dir: PathBuf,
    project: ProjectId,
    done: bool,
}

impl Run<'_> {
    /// Removes the tree, clears the project, and waits for the usage to read zero (the file
    /// system frees inodes in the background).
    fn finish(mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
        self.quota.clear(&self.dir, self.project).unwrap();
        self.done = true;
        for _ in 0..100 {
            if self.quota.usage(self.project).unwrap().is_zero() {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "usage of {} did not read zero: {:?}",
            self.project,
            self.quota.usage(self.project)
        );
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let removed = fs::remove_dir_all(&self.dir);
        let cleared = self.quota.clear(&self.dir, self.project);
        if !std::thread::panicking() {
            removed.unwrap();
            cleared.unwrap();
        }
    }
}

/// A directory that is removed when dropped (ignoring errors), for a directory outside any
/// project.
struct Plain(PathBuf);

impl Drop for Plain {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A new empty run directory and the project of `slot`, which must read zero. A slot with
/// usage left by an earlier failed run is refused with `InUse`: see the module docs.
fn fresh<'a>(quota: &'a XfsQuota, root: &Path, slot: u32, name: &str) -> Run<'a> {
    let project = ProjectIds::default().claim(quota, slot).unwrap();
    let dir = root.join(format!("{name}-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    Run {
        quota,
        dir,
        project,
        done: false,
    }
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

/// Whether the error is what a quota hit looks like: `ENOSPC` for an XFS project quota,
/// `EDQUOT` for a user or group quota. The caller proves it was the quota and not a full
/// file system.
fn is_quota_hit(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// A write past the limit gets `ENOSPC` (`EDQUOT` is accepted too) and stops at the limit,
/// a neighbour run under its own project is not affected (which also shows the file system
/// was not full), and the usage after the refused write is between 3/4 of the limit and
/// the limit (so a refusal far below the limit, from a full file system, fails). It catches a limit
/// that is not enforced, one counted in the wrong unit, and one shared between projects.
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn project_quota_enforced() {
    let (quota, root) = host();
    let small = fresh(&quota, &root, 100, "small");
    let big = fresh(&quota, &root, 101, "big");
    quota.apply(&small.dir, small.project, MIB, 1000).unwrap();
    quota.apply(&big.dir, big.project, 16 * MIB, 1000).unwrap();

    let over = write_file(&small.dir, "fill", 4 * MIB).unwrap_err();
    assert!(
        is_quota_hit(&over),
        "expected ENOSPC (XFS project quota) or EDQUOT, got: {over}"
    );
    let held = quota.usage(small.project).unwrap();
    assert!(
        held.bytes >= MIB * 3 / 4 && held.bytes <= MIB,
        "usage must be at least 3/4 of the 1 MiB limit (else this was not the quota) and at most the limit: {held:?}"
    );

    // The file system itself has room: the neighbour project writes 4 MiB.
    write_file(&big.dir, "fill", 4 * MIB).unwrap();
    assert!(quota.usage(big.project).unwrap().bytes >= 4 * MIB);
    small.finish();
    big.finish();
}

/// A flood of empty files stops at the inode limit with `ENOSPC` (`EDQUOT` is accepted
/// too), the project's inode usage has reached the limit, and a file can still be made
/// outside the project (the file system was not full). It catches an inode limit that is
/// not set (a job could exhaust the shared inode table).
///
/// Run on a provisioned XFS `prjquota` host: see the module documentation.
#[test]
#[ignore = "needs an XFS prjquota host: see the module docs"]
fn inode_flood_hits_limit() {
    const LIMIT: u64 = 100;
    let (quota, root) = host();
    let run = fresh(&quota, &root, 102, "flood");
    quota.apply(&run.dir, run.project, 64 * MIB, LIMIT).unwrap();
    let mut made = 0;
    let stopped = loop {
        match File::create(run.dir.join(format!("f{made}"))) {
            Ok(_) => made += 1,
            Err(err) => break err,
        }
        assert!(made <= LIMIT, "the inode limit did not stop the flood");
    };
    assert!(
        is_quota_hit(&stopped),
        "expected ENOSPC (XFS project quota) or EDQUOT, got: {stopped}"
    );
    assert!((50..LIMIT).contains(&made), "{made} files before the limit");
    // The directory itself is an inode of the project, so the count is at the limit or one
    // under it.
    let usage = quota.usage(run.project).unwrap();
    assert!(
        (LIMIT - 1..=LIMIT).contains(&usage.inodes),
        "inodes {} did not reach the limit {LIMIT}: {usage:?}",
        usage.inodes
    );

    // Outside the project a file can be made: the refusal was the quota, not a full table.
    let sibling = Plain(root.join(format!("sibling-{}", std::process::id())));
    fs::create_dir(&sibling.0).unwrap();
    // The documented setup has the XFS root in project 0, so the sibling is outside any
    // project (the quota module has no call that reads a directory's project id).
    File::create(sibling.0.join("free")).unwrap();
    run.finish();
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
    let run = fresh(&quota, &root, 103, "chown");
    quota.apply(&run.dir, run.project, 64 * MIB, 1000).unwrap();
    std::os::unix::fs::chown(&run.dir, Some(40_000), Some(40_000)).unwrap();
    write_file(&run.dir, "after", 2 * MIB).unwrap();
    assert!(quota.usage(run.project).unwrap().bytes >= 2 * MIB);
    run.finish();

    // Dropped, not finished: the guard removes the tree and clears the project.
    let early = fresh(&quota, &root, 104, "early");
    write_file(&early.dir, "before", 2 * MIB).unwrap();
    let refused = quota.apply(&early.dir, early.project, 64 * MIB, 1000);
    assert!(refused.is_err());
    assert_eq!(
        quota.usage(early.project).unwrap(),
        Usage::ZERO,
        "the file escaped the count"
    );
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
    let run = fresh(&quota, &root, 105, "clean");
    let project = run.project;
    quota.apply(&run.dir, project, MIB, 100).unwrap();
    assert_eq!(quota.limits(project).unwrap(), (MIB, 100));
    write_file(&run.dir, "out", MIB / 2).unwrap();
    run.finish();
    assert_eq!(quota.limits(project).unwrap(), (0, 0));

    let kept = fresh(&quota, &root, 105, "kept");
    quota.apply(&kept.dir, kept.project, MIB, 100).unwrap();
    quota.clear(&kept.dir, kept.project).unwrap();
    write_file(&kept.dir, "free", 4 * MIB).unwrap();
    assert_eq!(
        quota.usage(kept.project).unwrap(),
        Usage::ZERO,
        "the directory kept its project"
    );
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
