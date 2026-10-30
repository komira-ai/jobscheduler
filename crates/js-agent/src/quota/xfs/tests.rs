//! The XFS backend's logic against a scripted kernel: which device it aims at, what the
//! probe asks, the order of the calls of `apply` and `clear`, and every failure of each.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::*;
use crate::quota::mountinfo::makedev;
use crate::quota::sys::{Dq, Fsx};
use crate::quota::{MAX_BLOCK_LIMIT_BYTES, MAX_INODE_LIMIT, Mode, ProjectIds};

#[derive(Default)]
struct St {
    mountinfo: String,
    rdev: Option<u64>,
    flags: u16,
    attrs: BTreeMap<PathBuf, Fsx>,
    /// The path each open descriptor was opened from (the script has no real project ids).
    paths: BTreeMap<i32, PathBuf>,
    dq: BTreeMap<u32, Dq>,
    /// Operations that fail: `get_attr` fails every call, `get_attr#2` only the second.
    fail: BTreeSet<String>,
    /// `set_attr` is accepted and does nothing.
    forget_attrs: bool,
    counts: BTreeMap<&'static str, usize>,
    log: Vec<&'static str>,
}

#[derive(Default)]
struct Script(Mutex<St>);

impl Script {
    fn st(&self) -> std::sync::MutexGuard<'_, St> {
        self.0.lock().unwrap()
    }

    fn fail(&self, op: &str) {
        self.st().fail.insert(op.to_owned());
    }

    fn log(&self) -> Vec<&'static str> {
        self.st().log.clone()
    }

    /// Records the call and fails it when the script says so.
    fn enter(&self, op: &'static str) -> io::Result<()> {
        let mut st = self.st();
        st.log.push(op);
        let count = st.counts.entry(op).or_default();
        *count += 1;
        let nth = format!("{op}#{count}");
        match st.fail.contains(op) || st.fail.contains(&nth) {
            true => Err(io::Error::other(format!("scripted failure of {nth}"))),
            false => Ok(()),
        }
    }
}

impl Sys for Arc<Script> {
    fn mountinfo(&self) -> io::Result<String> {
        self.enter("mountinfo")?;
        Ok(self.st().mountinfo.clone())
    }

    fn block_rdev(&self, _path: &Path) -> io::Result<Option<u64>> {
        self.enter("rdev")?;
        Ok(self.st().rdev)
    }

    fn quota_flags(&self, _device: &Path) -> io::Result<u16> {
        self.enter("flags")?;
        Ok(self.st().flags)
    }

    /// Like `O_DIRECTORY | O_NOFOLLOW`: a link is refused, not followed (the path is looked
    /// at without following it, unlike `exists`).
    fn open_dir(&self, path: &Path) -> io::Result<File> {
        self.enter("open_dir")?;
        if !std::fs::symlink_metadata(path)?.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let file = File::open(path)?;
        self.st().paths.insert(file.as_raw_fd(), path.to_owned());
        Ok(file)
    }

    fn is_empty(&self, dir: &File) -> io::Result<bool> {
        self.enter("is_empty")?;
        let path = self.st().paths[&dir.as_raw_fd()].clone();
        Ok(std::fs::read_dir(path)?.next().is_none())
    }

    fn get_attr(&self, dir: &File) -> io::Result<Fsx> {
        self.enter("get_attr")?;
        let st = self.st();
        Ok(st
            .attrs
            .get(&st.paths[&dir.as_raw_fd()])
            .copied()
            .unwrap_or_default())
    }

    fn set_attr(&self, dir: &File, attr: Fsx) -> io::Result<()> {
        self.enter("set_attr")?;
        let mut st = self.st();
        let path = st.paths[&dir.as_raw_fd()].clone();
        if !st.forget_attrs {
            st.attrs.insert(path, attr);
        }
        Ok(())
    }

    fn get_quota(&self, _device: &Path, project: u32) -> io::Result<Dq> {
        self.enter("get_quota")?;
        let dq = self.st().dq.get(&project).copied();
        dq.ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn set_limits(&self, _device: &Path, project: u32, blocks: u64, inodes: u64) -> io::Result<()> {
        self.enter("set_limits")?;
        let mut st = self.st();
        let dq = st.dq.entry(project).or_default();
        dq.blocks_limit = blocks;
        dq.inodes_limit = inodes;
        Ok(())
    }
}

const DEV: u64 = (7 << 8) | 5;

struct Rig {
    root: PathBuf,
    script: Arc<Script>,
    quota: XfsQuota,
}

impl Rig {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("js-quota-xfs-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let script = Arc::new(Script::default());
        {
            let mut st = script.st();
            st.mountinfo = Self::mountinfo("xfs", "/dev/test", &root);
            st.rdev = Some(DEV);
            st.flags = PROJECT_ACCT_AND_ENFD;
        }
        let quota = XfsQuota::with_sys(
            Box::new(script.clone()),
            root.clone(),
            ProjectIds::default(),
        );
        Self {
            root,
            script,
            quota,
        }
    }

    fn mountinfo(fstype: &str, source: &str, root: &Path) -> String {
        let (major, minor) = (7, 5);
        let point = root.display();
        format!(
            "1 0 {major}:{minor} / / rw - rootfs rootfs rw\n40 1 {major}:{minor} / {point} rw - {fstype} {source} rw\n"
        )
    }

    fn run_dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn project(&self, slot: u32) -> ProjectId {
        ProjectIds::default().of_slot(slot).unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// With XFS on a block device whose number is the mount's, prjquota on, and a project id
/// that can be set and read back, the probe says project, names the device, and leaves no
/// directory behind. It catches a probe that stops short of setting an id, or that leaves
/// its test directory in the scratch root.
#[test]
fn probe_finds_project_quota() {
    let rig = Rig::new("probe-ok");
    let probe = rig.quota.probe();
    assert_eq!(probe.mode, Mode::Project, "{probe}");
    assert!(probe.reason.contains("/dev/test"), "{probe}");
    assert_eq!(
        rig.script.log(),
        [
            "mountinfo",
            "rdev",
            "flags",
            "open_dir",
            "get_attr",
            "set_attr",
            "get_attr"
        ]
    );
    assert_eq!(std::fs::read_dir(&rig.root).unwrap().count(), 0);
    let set = rig.script.st().attrs.values().next().copied().unwrap();
    assert_eq!(set.projid, ProjectIds::default().probe_id().get());
    assert_ne!(set.xflags & XFLAG_PROJINHERIT, 0);
}

/// Every probe makes a directory of a name of its own: two probes of one process set
/// their project id on two different paths. It catches a name made of the process id
/// alone, which a leftover of an earlier process with the same id would block.
#[test]
fn probe_directories_have_random_names() {
    let rig = Rig::new("probe-names");
    assert_eq!(rig.quota.probe().mode, Mode::Project);
    assert_eq!(rig.quota.probe().mode, Mode::Project);
    let paths: Vec<_> = rig.script.st().attrs.keys().cloned().collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert_ne!(paths[0], paths[1]);
}

/// A probe directory left by a crashed process of the same process id does not make the
/// probe say none, and the probe does not touch it. It catches a probe that fails on
/// `AlreadyExists`, which would turn a crash into a host with no quota until the
/// scratch sweep removes the leftover.
#[test]
fn a_leftover_probe_directory_does_not_block_the_probe() {
    let rig = Rig::new("probe-stale");
    let stale = rig
        .root
        .join(format!(".js-quota-probe-{}", std::process::id()));
    std::fs::create_dir(&stale).unwrap();
    let probe = rig.quota.probe();
    assert_eq!(probe.mode, Mode::Project, "{probe}");
    assert!(stale.is_dir());
}

/// Probes a rig set up by `setup` and expects none with `want` in the reason.
fn expect_none(name: &str, setup: impl FnOnce(&Rig), want: &str) {
    let rig = Rig::new(&format!("probe-{name}"));
    setup(&rig);
    let probe = rig.quota.probe();
    assert_eq!(probe.mode, Mode::None, "{name}: {probe}");
    assert!(probe.reason.contains(want), "{name}: {probe}");
}

/// Each way the host can fall short is a probe of none that says which. It catches a probe
/// that says project where the quota is off, the device is the wrong one, or ids are not
/// kept, and one that gives no reason.
#[test]
fn probe_says_why_not() {
    expect_none(
        "mountinfo",
        |r| r.script.fail("mountinfo"),
        "cannot read /proc/self/mountinfo",
    );
    expect_none(
        "no-mount",
        |r| r.script.st().mountinfo.clear(),
        "no mount that",
    );
    expect_none(
        "ext4",
        |r| r.script.st().mountinfo = Rig::mountinfo("ext4", "/dev/test", &r.root),
        "ext4, not xfs",
    );
    expect_none(
        "not-dev",
        |r| r.script.st().mountinfo = Rig::mountinfo("xfs", "/srv/image.xfs", &r.root),
        "not a device node",
    );
    expect_none(
        "other-dev",
        |r| r.script.st().rdev = Some(makedev(8, 1)),
        "different major:minor",
    );
    expect_none(
        "not-block",
        |r| r.script.st().rdev = None,
        "not a block device",
    );
    expect_none(
        "no-node",
        |r| r.script.fail("rdev"),
        "cannot look at /dev/test",
    );
    expect_none(
        "flags-error",
        |r| r.script.fail("flags"),
        "cannot read the quota state",
    );
    expect_none("quota-off", |r| r.script.st().flags = 0, "without prjquota");
    expect_none(
        "no-enforce",
        |r| r.script.st().flags = 0x10,
        "without prjquota",
    );
    expect_none(
        "root-gone",
        |r| std::fs::remove_dir(&r.root).unwrap(),
        "cannot make a probe directory",
    );
    expect_none(
        "open-fails",
        |r| r.script.fail("open_dir"),
        "project ids cannot be set",
    );
    expect_none(
        "get-fails",
        |r| r.script.fail("get_attr"),
        "project ids cannot be set",
    );
    expect_none(
        "set-fails",
        |r| r.script.fail("set_attr"),
        "project ids cannot be set",
    );
    expect_none(
        "readback-fails",
        |r| r.script.fail("get_attr#2"),
        "project ids cannot be set",
    );
    expect_none(
        "forgotten",
        |r| r.script.st().forget_attrs = true,
        "did not keep project",
    );
}

/// `apply` sets the project id and the inherit flag on the empty directory first, then the
/// limits, in blocks of 512 rounded up, and does the emptiness check and the three
/// attribute calls on the one descriptor it opened (`open_dir` is in the log once). It
/// catches limits set in bytes, a project id set after the limits, and a path that is
/// opened again between the check and the change.
#[test]
fn apply_sets_the_project_then_the_limits() {
    let rig = Rig::new("apply-ok");
    let dir = rig.run_dir("r1");
    let project = rig.project(3);
    rig.quota.apply(&dir, project, 1000, 77).unwrap();
    assert_eq!(
        rig.script.log(),
        [
            "mountinfo",
            "rdev",
            "open_dir",
            "is_empty",
            "get_attr",
            "set_attr",
            "get_attr",
            "set_limits"
        ]
    );
    let attr = rig.script.st().attrs[&dir];
    assert_eq!(attr.projid, project.get());
    assert_ne!(attr.xflags & XFLAG_PROJINHERIT, 0);
    assert_eq!(rig.script.st().dq[&project.get()].blocks_limit, 2);
    assert_eq!(rig.quota.limits(project).unwrap(), (1024, 77));
}

/// `apply` refuses what would leave a run unbounded or uncounted, before it touches
/// anything: a limit of 0 (no limit for the kernel) or above the bounds, a directory that
/// is not a direct child of the root, one that holds a file (it would escape the count),
/// and a root that has no quota.
#[test]
fn apply_refuses_what_cannot_be_enforced() {
    let rig = Rig::new("apply-refuse");
    let dir = rig.run_dir("r1");
    let project = rig.project(1);
    let zero = |bytes, inodes| rig.quota.apply(&dir, project, bytes, inodes).unwrap_err();
    assert!(matches!(zero(0, 5), QuotaError::BadLimit(_)));
    assert!(matches!(zero(5, 0), QuotaError::BadLimit(_)));
    let too_big = |bytes, inodes| rig.quota.apply(&dir, project, bytes, inodes).unwrap_err();
    assert!(matches!(
        too_big(MAX_BLOCK_LIMIT_BYTES + 1, 5),
        QuotaError::BadLimit(_)
    ));
    assert!(matches!(too_big(u64::MAX, 5), QuotaError::BadLimit(_)));
    assert!(matches!(
        too_big(5, MAX_INODE_LIMIT + 1),
        QuotaError::BadLimit(_)
    ));
    assert!(matches!(too_big(5, u64::MAX), QuotaError::BadLimit(_)));
    assert!(rig.script.log().is_empty());
    std::fs::write(dir.join("early"), "x").unwrap();
    let full = rig.quota.apply(&dir, project, 5, 5);
    assert!(matches!(full, Err(QuotaError::NotEmpty(_))), "{full:?}");
    let gone = rig.quota.apply(&rig.root.join("none"), project, 5, 5);
    assert!(matches!(gone, Err(QuotaError::Io { .. })), "{gone:?}");
    assert!(!rig.script.log().contains(&"set_attr"));
    assert!(!rig.script.log().contains(&"set_limits"));

    let other = Rig::new("apply-ext4");
    other.script.st().mountinfo = Rig::mountinfo("ext4", "/dev/test", &other.root);
    let dir = other.run_dir("r1");
    let refused = other.quota.apply(&dir, project, 5, 5);
    assert!(
        matches!(refused, Err(QuotaError::Unsupported(_))),
        "{refused:?}"
    );
    assert!(matches!(
        other.quota.usage(project),
        Err(QuotaError::Unsupported(_))
    ));
    assert!(matches!(
        other.quota.clear(&dir, project),
        Err(QuotaError::Unsupported(_))
    ));
    assert!(matches!(
        other.quota.limits(project),
        Err(QuotaError::Unsupported(_))
    ));
}

/// The largest limits are taken, and the block limit does not overflow on the way to the
/// kernel's unit: 2^46 - 1 bytes are 2^37 blocks of 512.
#[test]
fn apply_takes_the_largest_limits() {
    let rig = Rig::new("apply-max");
    let dir = rig.run_dir("r1");
    let project = rig.project(1);
    rig.quota
        .apply(&dir, project, MAX_BLOCK_LIMIT_BYTES, MAX_INODE_LIMIT)
        .unwrap();
    assert_eq!(rig.script.st().dq[&project.get()].blocks_limit, 1 << 37);
    assert_eq!(
        rig.quota.limits(project).unwrap(),
        (1 << 46, MAX_INODE_LIMIT)
    );
}

/// A directory the root does not directly hold is `Outside`, for `apply` and for `clear`,
/// and nothing reaches the kernel: a path that goes up with `..` and starts with the root
/// by components, the root itself, `root/..`, a sibling of the root whose name begins like
/// it, a deeper path, and a relative path. It catches a prefix test on the path, which
/// passes the first four.
#[test]
fn apply_and_clear_refuse_a_directory_that_is_not_a_child_of_the_root() {
    let rig = Rig::new("outside");
    let project = rig.project(1);
    let name = rig.root.file_name().unwrap().to_owned();
    let mut sibling = name.clone();
    sibling.push("-x");
    let outside = [
        rig.root.join("../etc/x"),
        rig.root.join(".."),
        rig.root.clone(),
        rig.root.with_file_name(sibling).join("r1"),
        rig.root.join("a/r1"),
        PathBuf::from("r1"),
        rig.root.parent().unwrap().join("..").join(&name).join("r1"),
    ];
    for dir in outside {
        let applied = rig.quota.apply(&dir, project, 5, 5);
        assert!(
            matches!(applied, Err(QuotaError::Outside(_))),
            "{dir:?}: {applied:?}"
        );
        let cleared = rig.quota.clear(&dir, project);
        assert!(
            matches!(cleared, Err(QuotaError::Outside(_))),
            "{dir:?}: {cleared:?}"
        );
    }
    assert!(rig.script.log().is_empty());
    let message = rig.quota.clear(&rig.root, project).unwrap_err().to_string();
    assert!(message.contains("directly below"), "{message}");
}

/// A link to a directory is not followed: `apply` and `clear` on a link below the root fail
/// at the open, and the directory it points at gets no project id. It catches an open that
/// follows links (in the real kernel the flag is `O_NOFOLLOW`; `sys` tests that on the real
/// open, and this one that the logic stops at its failure).
#[test]
fn a_link_to_a_directory_is_refused() {
    let rig = Rig::new("link");
    let target = rig.run_dir("target");
    let link = rig.root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let project = rig.project(1);
    let applied = rig.quota.apply(&link, project, 5, 5);
    assert!(matches!(applied, Err(QuotaError::Io { .. })), "{applied:?}");
    let cleared = rig.quota.clear(&link, project);
    assert!(matches!(cleared, Err(QuotaError::Io { .. })), "{cleared:?}");
    assert!(!rig.script.log().contains(&"set_attr"));
    assert!(rig.script.st().attrs.is_empty());
}

/// Every system call of `apply` that fails is an error naming what failed, and an id the
/// file system does not keep is `Lost`. It catches a failure that is swallowed and a run
/// that would start unbounded.
#[test]
fn apply_reports_each_failed_call() {
    for op in [
        "open_dir",
        "is_empty",
        "get_attr#1",
        "set_attr",
        "get_attr#2",
        "set_limits",
    ] {
        let rig = Rig::new(&format!("apply-{op}"));
        let dir = rig.run_dir("r1");
        rig.script.fail(op);
        let failed = rig.quota.apply(&dir, rig.project(1), 5, 5);
        assert!(
            matches!(failed, Err(QuotaError::Io { .. })),
            "{op}: {failed:?}"
        );
    }
    let rig = Rig::new("apply-lost");
    let dir = rig.run_dir("r1");
    rig.script.st().forget_attrs = true;
    let lost = rig.quota.apply(&dir, rig.project(1), 5, 5);
    assert!(matches!(lost, Err(QuotaError::Lost { .. })), "{lost:?}");
}

/// Usage is the project's blocks times 512 and its inodes; a project the file system has no
/// record of holds nothing, and any other failure is an error (not zero: a reading that
/// failed must not let an id be reused).
#[test]
fn usage_reads_the_project() {
    let rig = Rig::new("usage");
    let project = rig.project(9);
    assert_eq!(rig.quota.usage(project).unwrap(), Usage::ZERO);
    rig.script.st().dq.insert(
        project.get(),
        Dq {
            blocks_used: 4,
            inodes_used: 3,
            ..Dq::default()
        },
    );
    assert_eq!(
        rig.quota.usage(project).unwrap(),
        Usage {
            bytes: 2048,
            inodes: 3
        }
    );
    rig.script.fail("get_quota");
    assert!(matches!(
        rig.quota.usage(project),
        Err(QuotaError::Io { .. })
    ));
    assert!(rig.quota.limits(project).is_err());
}

/// `clear` takes the limits off first and the project id off the directory second, and
/// leaves the directory ordinary. It catches the reverse order (a directory with no
/// project and a project with a limit would be harmless, but a project id left on a
/// directory with its limit gone is not what the spec says) and a flag left behind.
#[test]
fn clear_removes_the_limits_then_the_project() {
    let rig = Rig::new("clear-ok");
    let dir = rig.run_dir("r1");
    let project = rig.project(2);
    rig.quota.apply(&dir, project, 4096, 10).unwrap();
    rig.script.st().log.clear();
    rig.quota.clear(&dir, project).unwrap();
    assert_eq!(
        rig.script.log(),
        [
            "mountinfo",
            "rdev",
            "set_limits",
            "open_dir",
            "get_attr",
            "set_attr"
        ]
    );
    let attr = rig.script.st().attrs[&dir];
    assert_eq!((attr.projid, attr.xflags & XFLAG_PROJINHERIT), (0, 0));
    assert_eq!(rig.quota.limits(project).unwrap(), (0, 0));
}

/// A directory that is already gone has nothing to clear but the limits: the project id
/// went with the inode.
#[test]
fn clear_after_the_tree_is_removed() {
    let rig = Rig::new("clear-gone");
    let project = rig.project(2);
    rig.script.st().dq.insert(
        project.get(),
        Dq {
            blocks_limit: 9,
            ..Dq::default()
        },
    );
    rig.quota.clear(&rig.root.join("gone"), project).unwrap();
    assert_eq!(rig.quota.limits(project).unwrap(), (0, 0));
    assert!(!rig.script.log().contains(&"set_attr"));
    assert!(!rig.script.log().contains(&"get_attr"));
}

/// A `clear` that fails at any call is an error, so the caller keeps the slot; and a retry
/// after the failure succeeds. It catches a clear that reports success after a failed step.
#[test]
fn clear_reports_each_failed_call() {
    for op in ["set_limits", "open_dir", "get_attr", "set_attr"] {
        let rig = Rig::new(&format!("clear-{op}"));
        let dir = rig.run_dir("r1");
        let project = rig.project(2);
        rig.quota.apply(&dir, project, 4096, 10).unwrap();
        rig.script.fail(op);
        let failed = rig.quota.clear(&dir, project);
        assert!(
            matches!(failed, Err(QuotaError::Io { .. })),
            "{op}: {failed:?}"
        );
        rig.script.st().fail.clear();
        rig.quota.clear(&dir, project).unwrap();
    }
}
