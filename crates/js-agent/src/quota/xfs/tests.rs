//! The XFS backend's logic against a scripted kernel: which device it aims at, what the
//! probe asks, the order of the calls of `apply` and `clear`, and every failure of each.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::*;
use crate::quota::mountinfo::makedev;
use crate::quota::sys::{Dq, Fsx};
use crate::quota::{Mode, ProjectIds};

#[derive(Default)]
struct St {
    mountinfo: String,
    rdev: Option<u64>,
    flags: u16,
    attrs: BTreeMap<PathBuf, Fsx>,
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

    fn get_attr(&self, dir: &Path) -> io::Result<Fsx> {
        self.enter("get_attr")?;
        match dir.exists() {
            true => Ok(self.st().attrs.get(dir).copied().unwrap_or_default()),
            false => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn set_attr(&self, dir: &Path, attr: Fsx) -> io::Result<()> {
        self.enter("set_attr")?;
        let mut st = self.st();
        if !st.forget_attrs {
            st.attrs.insert(dir.to_owned(), attr);
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
        "stale-probe-dir",
        |r| {
            let name = format!(".js-quota-probe-{}", std::process::id());
            std::fs::create_dir(r.root.join(name)).unwrap();
        },
        "cannot make a probe directory",
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
/// limits, in blocks of 512 rounded up. It catches limits set in bytes, and a directory
/// bound after the quota (a window in which the project is limited and the directory is
/// not counted is harmless, but the project must be set before the call returns).
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
/// anything: a limit of 0 (no limit for the kernel), a directory outside the root, one
/// that holds a file (it would escape the count), and a root that has no quota.
#[test]
fn apply_refuses_what_cannot_be_enforced() {
    let rig = Rig::new("apply-refuse");
    let dir = rig.run_dir("r1");
    let project = rig.project(1);
    let zero = |bytes, inodes| rig.quota.apply(&dir, project, bytes, inodes).unwrap_err();
    assert!(matches!(zero(0, 5), QuotaError::BadLimit(_)));
    assert!(matches!(zero(5, 0), QuotaError::BadLimit(_)));
    let outside = rig.quota.apply(Path::new("/elsewhere/r1"), project, 5, 5);
    assert!(matches!(outside, Err(QuotaError::Outside(_))));
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

/// Every system call of `apply` that fails is an error naming what failed, and an id the
/// file system does not keep is `Lost`. It catches a failure that is swallowed and a run
/// that would start unbounded.
#[test]
fn apply_reports_each_failed_call() {
    for op in ["get_attr#1", "set_attr", "get_attr#2", "set_limits"] {
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
        ["mountinfo", "rdev", "set_limits", "get_attr", "set_attr"]
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
}

/// A `clear` that fails at any call is an error, so the caller keeps the slot; and a retry
/// after the failure succeeds. It catches a clear that reports success after a failed step.
#[test]
fn clear_reports_each_failed_call() {
    for op in ["set_limits", "get_attr", "set_attr"] {
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
