//! Duty 2: the per-run scratch disk quota (`--scratch-quota`): XFS project quota set with
//! `FS_IOC_FSSETXATTR` on the run's directory (project id and inherit flag) and `quotactl`
//! (`Q_XSETQLIM` for the limits, `Q_XGETQUOTA` for the usage). Which capability each call
//! takes for a non-root agent is read here from a real prjquota XFS volume (the workflow
//! loop-mounts an image and names it in `JS_PRIV_XFS_ROOT`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent::quota::{ProjectId, ProjectIds, QuotaError, ScratchQuota, XfsQuota};
use js_testkit::dir::TestDir;

use crate::harness::{AGENT, JOB, Kv, arg, cap, host_path, kv, make_dir, require_root, run_agent};

const MIB: u64 = 1 << 20;
const INODES: u64 = 1000;
const EPERM: i32 = 1;

fn outcome<T>(result: &Result<T, QuotaError>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(QuotaError::Io { source, .. }) => {
            format!("errno:{}", source.raw_os_error().unwrap_or(-1))
        }
        Err(other) => format!("other:{other}"),
    }
}

fn slot_arg() -> u32 {
    arg("SLOT").to_str().unwrap().parse().unwrap()
}

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    let project = |slot| ProjectIds::default().of_slot(slot).unwrap();
    match name {
        "quota_apply" => {
            let quota = XfsQuota::new(arg("SCRATCH"), ProjectIds::default());
            let probed = quota.probe();
            let applied = quota.apply(&arg("RUN"), project(slot_arg()), 64 * MIB, INODES);
            Some(vec![
                kv("probe", probed.mode),
                kv("apply", outcome(&applied)),
            ])
        }
        "quota_usage" => {
            let quota = XfsQuota::new(arg("SCRATCH"), ProjectIds::default());
            let used = quota.usage(project(slot_arg()));
            Some(vec![kv("usage", outcome(&used))])
        }
        "quota_clear_after_removal" => {
            let quota = XfsQuota::new(arg("SCRATCH"), ProjectIds::default());
            let run = arg("RUN");
            fs::remove_dir(&run).unwrap();
            let cleared = quota.clear(&run, project(slot_arg()));
            Some(vec![kv("clear", outcome(&cleared))])
        }
        _ => None,
    }
}

/// A scratch root on the XFS volume owned by the agent's user (shape A), and the quota of
/// it as root sees it.
struct Volume {
    _dir: TestDir,
    scratch: PathBuf,
    quota: XfsQuota,
}

impl Volume {
    fn new(name: &str, owner: u32) -> Self {
        require_root();
        let dir = TestDir::under(host_path("XFS_ROOT"), name);
        let scratch = dir.path().join("scratch");
        make_dir(&scratch, 0o700, owner);
        let scratch = fs::canonicalize(scratch).unwrap();
        let quota = XfsQuota::new(scratch.clone(), ProjectIds::default());
        Self {
            _dir: dir,
            scratch,
            quota,
        }
    }

    /// An empty run directory below the scratch root, `mode` and owned by `owner`.
    fn run_dir(&self, name: &str, mode: u32, owner: u32) -> PathBuf {
        let run = self.scratch.join(name);
        make_dir(&run, mode, owner);
        run
    }

    fn project(slot: u32) -> ProjectId {
        ProjectIds::default().of_slot(slot).unwrap()
    }

    /// What the file system holds as the limits of the slot's project, read as root.
    fn limits(&self, slot: u32) -> (u64, u64) {
        self.quota.limits(Self::project(slot)).unwrap()
    }

    /// Takes the limits and the project id off, as root, whatever the test left.
    fn clean(&self, run: &Path, slot: u32) {
        self.quota.clear(run, Self::project(slot)).unwrap();
        assert_eq!(self.limits(slot), (0, 0));
    }

    fn agent(&self, caps: &[u32], probe: &str, run: &Path, slot: u32) -> crate::harness::Report {
        let slot_text = PathBuf::from(slot.to_string());
        run_agent(
            caps,
            probe,
            &[
                ("SCRATCH", &self.scratch),
                ("RUN", run),
                ("SLOT", &slot_text),
            ],
        )
    }
}

/// A: a non-root agent that owns the scratch root and the run directory but holds no
/// capability cannot bound the run: the limits are set with `Q_XSETQLIM`, which the kernel
/// allows only to `CAP_SYS_ADMIN`, so `apply` fails with EPERM and the project holds no
/// limit, although the same agent's probe (which makes a directory and sets a project id on
/// it as its owner) succeeds. It catches a design that counts on directory ownership alone
/// for the quota.
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_without_a_capability_cannot_set_the_limits_of_its_own_directory() {
    let volume = Volume::new("priv-quota-none", AGENT);
    let run = volume.run_dir("run", 0o700, AGENT);
    let report = volume.agent(&[], "quota_apply", &run, 510);
    report.expect("probe", "project");
    report.expect("apply", &format!("errno:{EPERM}"));
    assert_eq!(volume.limits(510), (0, 0), "nothing was set");
    volume.clean(&run, 510);
}

/// A: `CAP_SYS_ADMIN` and nothing else is enough for a non-root agent that owns the run
/// directory: the project id and inherit flag are set as the directory's owner, the limits
/// with the capability, and the file system then holds exactly the limits asked for. This is
/// the capability the quota costs under shape A (broad: it is most of root).
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn cap_sys_admin_alone_bounds_a_directory_the_agent_owns() {
    let volume = Volume::new("priv-quota-admin", AGENT);
    let run = volume.run_dir("run", 0o700, AGENT);
    let report = volume.agent(&[cap::SYS_ADMIN], "quota_apply", &run, 511);
    report.expect("probe", "project");
    report.expect("apply", "ok");
    assert_eq!(volume.limits(511), (64 * MIB, INODES));
    volume.clean(&run, 511);
}

/// A: on a run directory the agent does NOT own (a directory already given to the job's
/// ids), `CAP_SYS_ADMIN` is not enough: the project id cannot be set on another user's
/// directory without `CAP_FOWNER`. So the quota must be applied before the give, as the
/// ordering rule of the quota module already requires, or the agent needs both. It catches
/// an ordering that gives the directory away first.
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn setting_the_project_of_a_directory_the_agent_does_not_own_needs_cap_fowner() {
    let volume = Volume::new("priv-quota-fowner", AGENT);
    let run = volume.run_dir("run", 0o755, JOB);
    let report = volume.agent(&[cap::SYS_ADMIN], "quota_apply", &run, 512);
    report.expect("apply", &format!("errno:{EPERM}"));
    assert_eq!(volume.limits(512), (0, 0));
    let report = volume.agent(&[cap::SYS_ADMIN, cap::FOWNER], "quota_apply", &run, 512);
    report.expect("apply", "ok");
    assert_eq!(volume.limits(512), (64 * MIB, INODES));
    volume.clean(&run, 512);
}

/// A: reading a project's usage (`Q_XGETQUOTA` on a project, which `ProjectIds::claim`
/// does for every run) is also refused without `CAP_SYS_ADMIN`: the kernel lets an
/// unprivileged user read only the user and group quotas it belongs to.
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn reading_the_usage_of_a_project_needs_cap_sys_admin() {
    let volume = Volume::new("priv-quota-usage", AGENT);
    let run = volume.run_dir("run", 0o700, AGENT);
    let none = volume.agent(&[], "quota_usage", &run, 513);
    none.expect("usage", &format!("errno:{EPERM}"));
    let admin = volume.agent(&[cap::SYS_ADMIN], "quota_usage", &run, 513);
    admin.expect("usage", "ok");
}

/// A: taking the limits off after the run (`clear`, with the directory already removed)
/// is a `Q_XSETQLIM` too: refused without `CAP_SYS_ADMIN` (the slot then stays held), done
/// with it. The agent therefore needs the capability at both ends of a run, not only at
/// its start.
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn removing_the_limits_after_the_run_needs_cap_sys_admin() {
    let volume = Volume::new("priv-quota-clear", AGENT);
    let run = volume.run_dir("run", 0o700, AGENT);
    volume
        .quota
        .apply(&run, Volume::project(514), 64 * MIB, INODES)
        .unwrap();
    let none = volume.agent(&[], "quota_clear_after_removal", &run, 514);
    none.expect("clear", &format!("errno:{EPERM}"));
    assert_eq!(
        volume.limits(514),
        (64 * MIB, INODES),
        "the limits are still set"
    );

    let run = volume.run_dir("run2", 0o700, AGENT);
    volume
        .quota
        .apply(&run, Volume::project(515), 64 * MIB, INODES)
        .unwrap();
    let admin = volume.agent(&[cap::SYS_ADMIN], "quota_clear_after_removal", &run, 515);
    admin.expect("clear", "ok");
    assert_eq!(volume.limits(515), (0, 0));
    volume.clean(&volume.scratch.join("run"), 514);
}

/// B: a root agent sets the limits on a directory that belongs to any user, reads the usage
/// and takes the limits off, with no capability named: the whole quota duty, as the
/// `--scratch-quota` code runs today.
#[test]
#[ignore = "needs root and a prjquota XFS volume: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_bounds_and_clears_a_directory_that_belongs_to_the_job() {
    let volume = Volume::new("priv-quota-root", AGENT);
    let run = volume.run_dir("run", 0o755, JOB);
    let project = Volume::project(516);
    volume.quota.apply(&run, project, 64 * MIB, INODES).unwrap();
    assert_eq!(volume.limits(516), (64 * MIB, INODES));
    assert!(volume.quota.usage(project).is_ok());
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir(&run).unwrap();
    volume.quota.clear(&run, project).unwrap();
    assert_eq!(volume.limits(516), (0, 0));
}
