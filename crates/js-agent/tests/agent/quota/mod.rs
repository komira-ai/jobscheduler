//! The scratch quota: project ids, the fake every other test of the agent can use, the
//! errors a failed call gives the caller, and the probe on a path that has no quota. The
//! tests that need an XFS host with project quota are in `xfs_host` and do not run by
//! default.

#[cfg(target_os = "linux")]
mod xfs_host;

use std::fs;
use std::io;
use std::path::Path;

use js_agent::Machine;
use js_agent::quota::{
    Call, FakeQuota, MAX_BLOCK_LIMIT_BYTES, MAX_INODE_LIMIT, MAX_SLOTS, Mode, Probe, ProjectId,
    ProjectIds, QuotaError, ScratchQuota, Usage, probe,
};

use crate::support::Scratch;

fn ids() -> ProjectIds {
    ProjectIds::default()
}

fn run_dir(scratch: &Scratch, name: &str) -> std::path::PathBuf {
    let dir = scratch.path(name);
    fs::create_dir(&dir).unwrap();
    dir
}

/// A run's use of the quota in order, against the [`FakeQuota`]: claim the id, apply to the empty directory, run,
/// remove the tree, clear. The calls reach the quota in that order with the arguments the
/// caller gave, and the limits are there from the apply until the clear. It catches an
/// `apply` that is not made before the directory is written, and a `clear` that leaves the
/// limits.
#[test]
fn fake_quota_apply_then_clear_in_order() {
    let scratch = Scratch::new("quota-order");
    let dir = run_dir(&scratch, "r1");
    let quota = FakeQuota::new();
    let project = ids().claim(&quota, 4).unwrap();
    quota.apply(&dir, project, 1 << 30, 5000).unwrap();
    assert_eq!(quota.limits_of(project), Some((1 << 30, 5000)));
    fs::write(dir.join("job-output"), "x").unwrap();
    fs::remove_file(dir.join("job-output")).unwrap();
    quota.clear(&dir, project).unwrap();
    assert_eq!(quota.limits_of(project), None);
    assert_eq!(
        quota.calls(),
        [
            Call::Usage(project),
            Call::Apply {
                dir: dir.clone(),
                project,
                block_limit_bytes: 1 << 30,
                inode_limit: 5000
            },
            Call::Clear { dir, project },
        ]
    );
}

/// A directory with something in it is refused (a file made before the project id is set
/// is not counted), and so is a limit of 0 (no limit for the kernel) or above the bounds. Nothing is bound.
#[test]
fn apply_refuses_a_full_directory_a_zero_limit_and_a_limit_too_big() {
    let scratch = Scratch::new("quota-refuse");
    let dir = run_dir(&scratch, "r1");
    let quota = FakeQuota::new();
    let project = ids().of_slot(1).unwrap();
    assert!(matches!(
        quota.apply(&dir, project, 0, 10),
        Err(QuotaError::BadLimit(_))
    ));
    assert!(matches!(
        quota.apply(&dir, project, 10, 0),
        Err(QuotaError::BadLimit(_))
    ));
    // A limit above the bound could wrap to 0 on the way to the kernel: refused, and the
    // largest accepted is taken.
    for (bytes, inodes) in [
        (MAX_BLOCK_LIMIT_BYTES + 1, 10),
        (u64::MAX, 10),
        (10, MAX_INODE_LIMIT + 1),
        (10, u64::MAX),
    ] {
        assert!(matches!(
            quota.apply(&dir, project, bytes, inodes),
            Err(QuotaError::BadLimit(_))
        ));
    }
    assert_eq!(quota.limits_of(project), None);
    quota
        .apply(&dir, project, MAX_BLOCK_LIMIT_BYTES, MAX_INODE_LIMIT)
        .unwrap();
    assert_eq!(
        quota.limits_of(project),
        Some((MAX_BLOCK_LIMIT_BYTES, MAX_INODE_LIMIT))
    );
    quota.clear(&dir, project).unwrap();
    fs::write(dir.join("early"), "x").unwrap();
    assert!(matches!(
        quota.apply(&dir, project, 10, 10),
        Err(QuotaError::NotEmpty(_))
    ));
    let gone = quota.apply(&scratch.path("none"), project, 10, 10);
    assert!(matches!(gone, Err(QuotaError::Io { .. })));
    assert_eq!(quota.limits_of(project), None);
}

/// A failed `clear` is an error, leaves the limits where they were, converts to an
/// `io::Error` the caller can treat like a failed scratch removal, and succeeds when it is
/// tried again. It catches a clear that forgets the project before it has finished.
#[test]
fn a_failed_clear_is_an_error_and_can_be_retried() {
    let scratch = Scratch::new("quota-failed-clear");
    let dir = run_dir(&scratch, "r1");
    let quota = FakeQuota::new();
    let project = ids().of_slot(2).unwrap();
    quota.apply(&dir, project, 4096, 10).unwrap();
    quota.set_fail_clear(true);
    let err = quota.clear(&dir, project).unwrap_err();
    assert!(matches!(err, QuotaError::Io { what: "clear", .. }), "{err}");
    assert_eq!(quota.limits_of(project), Some((4096, 10)));
    let as_io: io::Error = err.into();
    assert!(as_io.to_string().contains("clear"), "{as_io}");
    quota.set_fail_clear(false);
    quota.clear(&dir, project).unwrap();
    assert_eq!(quota.limits_of(project), None);
}

/// A failed `apply` binds nothing.
#[test]
fn a_failed_apply_binds_nothing() {
    let scratch = Scratch::new("quota-failed-apply");
    let dir = run_dir(&scratch, "r1");
    let quota = FakeQuota::new();
    let project = ids().of_slot(2).unwrap();
    quota.set_fail_apply(true);
    let err = quota.apply(&dir, project, 4096, 10).unwrap_err();
    assert!(matches!(err, QuotaError::Io { what: "apply", .. }), "{err}");
    assert_eq!(quota.limits_of(project), None);
    quota.set_fail_apply(false);
    quota.apply(&dir, project, 4096, 10).unwrap();
}

/// A slot's id is not handed out again while the project holds usage: the previous run's
/// files (or a clear that did not finish) would be counted against the next run. It is
/// again when usage reads zero. Two slots never share an id.
#[test]
fn an_id_is_not_reused_while_it_holds_usage() {
    let scratch = Scratch::new("quota-reuse");
    let dir = run_dir(&scratch, "r1");
    let quota = FakeQuota::new();
    let first = ids().claim(&quota, 7).unwrap();
    assert_ne!(first, ids().claim(&quota, 8).unwrap());
    quota.apply(&dir, first, 4096, 10).unwrap();
    let held = Usage {
        bytes: 4096,
        inodes: 3,
    };
    quota.charge(first, held);
    quota.clear(&dir, first).unwrap();
    match ids().claim(&quota, 7) {
        Err(QuotaError::InUse { project, usage }) => assert_eq!((project, usage), (first, held)),
        other => panic!("{other:?}"),
    }
    quota.charge(first, Usage::ZERO);
    assert_eq!(ids().claim(&quota, 7).unwrap(), first);
}

/// A slot with no id, and a quota that cannot say what a project holds, give no id.
#[test]
fn claim_fails_without_an_id_or_a_reading() {
    let quota = FakeQuota::new();
    assert!(matches!(
        ids().claim(&quota, MAX_SLOTS),
        Err(QuotaError::BadProject(_))
    ));
    let none = FakeQuota::unsupported("tmpfs");
    assert!(matches!(
        ids().claim(&none, 0),
        Err(QuotaError::Unsupported(_))
    ));
}

/// The id of a slot is the base plus the slot, the probe's id is after the last slot's, and
/// a base that is 0 or leaves the range outside an `i32` is refused (the id of `quotactl`
/// is a signed int, and 0 is no project).
#[test]
fn ids_are_the_base_plus_the_slot() {
    let ids = ProjectIds::new(0x1000).unwrap();
    assert_eq!(ids.of_slot(5).unwrap().get(), 0x1005);
    assert_eq!(ids.probe_id().get(), 0x1000 + MAX_SLOTS);
    assert_eq!(ids.of_slot(5).unwrap().to_string(), "4101");
    assert!(ids.of_slot(MAX_SLOTS).is_err());
    assert!(ProjectIds::new(0).is_err());
    assert!(ProjectIds::new(i32::MAX as u32 - MAX_SLOTS + 1).is_err());
    assert!(ProjectIds::new(u32::MAX).is_err());
    assert!(ProjectIds::new(i32::MAX as u32 - MAX_SLOTS).is_ok());
    assert_eq!(
        ProjectIds::default().of_slot(0).unwrap().get(),
        ProjectIds::DEFAULT_BASE
    );
}

/// A fake that is unsupported probes to none with its reason and refuses every call; a
/// working one probes to project and reports what a test charged to a project.
#[test]
fn the_fake_probes_and_reports() {
    let scratch = Scratch::new("quota-fake");
    let dir = run_dir(&scratch, "r1");
    let project = ids().of_slot(0).unwrap();
    let off = FakeQuota::unsupported("no prjquota");
    assert_eq!(off.probe(), Probe::none("no prjquota"));
    assert!(matches!(
        off.apply(&dir, project, 1, 1),
        Err(QuotaError::Unsupported(_))
    ));
    assert!(matches!(
        off.clear(&dir, project),
        Err(QuotaError::Unsupported(_))
    ));
    assert!(matches!(
        off.usage(project),
        Err(QuotaError::Unsupported(_))
    ));

    let on = FakeQuota::new();
    assert_eq!(on.probe().mode, Mode::Project);
    assert_eq!(on.usage(project).unwrap(), Usage::ZERO);
    on.charge(
        project,
        Usage {
            bytes: 9,
            inodes: 2,
        },
    );
    assert_eq!(
        on.usage(project).unwrap(),
        Usage {
            bytes: 9,
            inodes: 2
        }
    );
}

/// The texts an operator reads in the log.
#[test]
fn probe_and_error_texts() {
    assert_eq!(Probe::none("tmpfs").to_string(), "none (tmpfs)");
    assert_eq!(
        Probe::project("xfs on /dev/x").to_string(),
        "project (xfs on /dev/x)"
    );
    let busy = QuotaError::InUse {
        project: ids().of_slot(1).unwrap(),
        usage: Usage {
            bytes: 5,
            inodes: 2,
        },
    };
    assert!(
        busy.to_string().contains("still holds 5 bytes in 2 inodes"),
        "{busy}"
    );
    assert!(
        QuotaError::NotEmpty("/s/r1".into())
            .to_string()
            .contains("not empty")
    );
    let _: ProjectId = ids().of_slot(0).unwrap();
}

/// The probe of a path with no project quota is none and says why. `/proc` is on no XFS on
/// any Linux, and is read-only, so nothing is made there; elsewhere there is no quota at
/// all. It catches a probe that says project for a file system that cannot bound a run.
#[test]
fn a_root_that_is_not_on_xfs_probes_to_none() {
    let probe = probe(Path::new("/proc"));
    assert_eq!(probe.mode, Mode::None, "{probe}");
    let want = match cfg!(target_os = "linux") {
        true => "not xfs",
        false => "no project quota on this platform",
    };
    assert!(probe.reason.contains(want), "{probe}");
}

/// Asserts that the quota the machine hands the agent for a scratch root is the one `probe`
/// describes: `quota::system` and the default `Machine::scratch_quota` of the real machine
/// give the same answer as the free function, on a directory that is not on a project-quota
/// file system as well as on one that is. It catches a machine seam that is wired to another
/// root or another mechanism than the one the probe reports.
#[test]
fn the_machines_quota_probes_like_the_free_function() {
    let scratch = Scratch::new("quota-system");
    let dir = run_dir(&scratch, "root");
    let want = probe(&dir);
    assert_eq!(js_agent::quota::system(&dir).probe(), want);
    assert_eq!(js_agent::System::new().scratch_quota(&dir).probe(), want);
}
