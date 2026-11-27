//! The host setup of a Mac in `prepare` (`scratch_root`): the state root and the scratch root are
//! made and the scratch root's parent gets a volume of its own, mounted on it (mode 0711, the
//! scratch root a private directory on it), with no file system table written, and no group made
//! or looked up; a part that cannot be made defers `host_setup_missing` with nothing written and
//! nothing cordoned; a rerun after a kill at any point ends as an uninterrupted run; a systemd
//! host that names a part is refused. The error paths of the volume are in
//! steps_host_setup_volume.rs; a Mac an earlier release's setup left half done, and a call the
//! system refuses, are in steps_host_setup_adopt.rs.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::update_hook::step::{DISKUTIL, SCRATCH_VOLUME};
use js_agent::update_hook::{Class, Done, Outcome, Reason};

use super::fake_mac::{
    DEVICE, FakeMac, QUOTA, UUID, Volume, mac_rig, mount_of, scratch_of, spy_on,
};
use super::step_rig::{MAC_UNIT, RETRY_AFTER_SECS, Rig, Spy, killable};

/// The permission bits of the directory at `path`.
fn mode(path: &Path) -> u32 {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir(), "{}", path.display());
    meta.permissions().mode() & 0o7777
}

/// Asserts that no file of the rig's tree is named `fstab`: the setup wrote no file system
/// table, which macOS refuses to the hook.
pub(super) fn assert_no_table(rig: &Rig) {
    let mut dirs = vec![rig.tmp.path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            assert_ne!(entry.file_name(), "fstab", "{}", dir.display());
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.path());
            }
        }
    }
}

/// What a first install on an empty Mac changes, in order.
pub(super) fn first_changes(rig: &Rig) -> Vec<String> {
    let scratch = mount_of(rig).display().to_string();
    vec![
        format!("{DISKUTIL} apfs addVolume disk3 APFS {SCRATCH_VOLUME} -quota {QUOTA} -nomount"),
        format!("{DISKUTIL} mount nobrowse -mountPoint {scratch} {DEVICE}"),
        format!("{DISKUTIL} enableOwnership {scratch}"),
    ]
}

/// Asserts the deferral of a host setup that is not complete: exit 75, `host_setup_missing`, an
/// environment matter, the retry hint, and exactly `detail`.
pub(super) fn assert_missing(done: &Done, detail: &str) {
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::HostSetupMissing));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert_eq!(done.report.detail.as_deref(), Some(detail));
}

/// Asserts that the setup of a first install and of its cutover runs no directory service
/// command: no group is looked up or made, whatever a release before the one-program package
/// needed for the socket of its helper.
#[test]
fn a_mac_setup_makes_and_looks_up_no_group() {
    let rig = mac_rig("hook-setup-no-group", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Success);
    for call in spy.log() {
        assert!(
            !call.contains("/usr/bin/dscl") && !call.contains("/usr/sbin/dseditgroup"),
            "{call}"
        );
    }
}

/// Asserts that a first install on an empty Mac makes the state root (0700), makes the scratch
/// volume with nine tenths of the container as its quota, mounts it (`nobrowse`) on the scratch
/// root's parent and gives it ownership, in that order, then makes the scratch root on the volume
/// (0700) and sets the mount point to 0711 (its parent 0755), makes no store directory, writes
/// no file system table, and writes its record. Catches a mount point a lease user cannot search
/// (a native job of a bundle in the store beside the scratch root ends 71 `Permission denied`),
/// a scratch root made under the mount instead of on the volume, an agent that refuses to start
/// because its two roots share a file system, and a setup that needs a table macOS refuses.
#[test]
fn a_first_install_makes_the_roots_and_mounts_the_scratch_volume_on_the_scratch_roots_parent() {
    let rig = mac_rig("hook-setup-roots", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let (scratch, mount) = (scratch_of(&rig), mount_of(&rig));
    assert_eq!(mode(&rig.params.state_root), 0o700);
    assert_eq!(mode(&scratch), 0o700);
    assert_eq!(mode(&mount), 0o711);
    assert_eq!(mode(mount.parent().unwrap()), 0o755);
    assert!(!mount.join("bundles").exists());
    assert_eq!(mac.changes(), first_changes(&rig));
    assert_no_table(&rig);
    assert_eq!(
        *mac.volume.borrow(),
        Some(Volume {
            mount: Some(mount),
            owners: true,
        })
    );
    assert!(rig.record().is_some());
}

/// Asserts that a `prepare` on a Mac set up already changes nothing: no volume or mount is made
/// again.
#[test]
fn a_prepare_on_a_set_up_mac_changes_nothing() {
    let rig = mac_rig("hook-setup-rerun", false);
    let mac = FakeMac::new();
    assert_eq!(spy_on(&rig, &mac).prepare(&rig).outcome, Outcome::Success);
    let before = mac.changes();
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(mac.changes(), before);
}

/// Asserts that an update of a Mac the previous release did not set up sets it up before the
/// cordon is placed, and then drains as before.
#[test]
fn an_update_sets_up_the_mac_before_it_cordons() {
    let rig = mac_rig("hook-setup-update", true);
    rig.world.seed_unit(MAC_UNIT, "active", "running");
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(mac.changes(), first_changes(&rig));
    let log = spy.log();
    let mount = log
        .iter()
        .position(|call| call.contains(" mount nobrowse -mountPoint "));
    let cordon = log.iter().position(|call| call.starts_with("cordon("));
    assert!(mount.unwrap() < cordon.expect("cordoned"), "{log:?}");
}

/// Asserts that an update whose setup cannot be completed defers with the host left as it was:
/// no record, no cordon asked for.
#[test]
fn an_update_whose_setup_defers_leaves_the_host_uncordoned() {
    let rig = mac_rig("hook-setup-update-defers", true);
    rig.world.seed_unit(MAC_UNIT, "active", "running");
    let mac = FakeMac::new();
    mac.answer(
        &format!("{DISKUTIL} apfs list -plist disk3"),
        Err("eDSPermissionError"),
    );
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
    assert_eq!(rig.cordon_expiry(), None);
    assert!(rig.record().is_none());
}

/// Everything a host setup leaves behind, with the rig's tree written `<root>`: the record,
/// whether the leftover copy of the file system table is there, the modes of the roots and the
/// volume.
fn ends(rig: &Rig, spy: &Spy<'_>, mac: &FakeMac) -> String {
    let record = rig
        .record()
        .map(|record| serde_json::to_string(&record).unwrap());
    let state = format!(
        "{record:?} {:?} {:o} {:o} {:o} {:?}",
        spy.fstab_leftover.exists(),
        mode(&rig.params.state_root),
        mode(&mount_of(rig)),
        mode(&scratch_of(rig)),
        mac.volume.borrow(),
    );
    state.replace(&rig.tmp.path().display().to_string(), "<root>")
}

/// Asserts that a first install's `prepare` on a Mac with the leftover copy of the file system
/// table, killed at any point of the host setup and run again, ends as one that was never
/// killed, with the volume made once and the leftover gone.
#[test]
fn a_setup_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let baseline = mac_rig("hook-setup-sweep-base", false);
    let base_mac = FakeMac::new();
    let base_spy = spy_on(&baseline, &base_mac);
    leave_leftover(&base_spy);
    assert_eq!(base_spy.prepare(&baseline).outcome, Outcome::Success);
    let points = baseline.kill.points();
    let end = ends(&baseline, &base_spy, &base_mac);
    assert!(!base_spy.fstab_leftover.exists());
    assert!(points > first_changes(&baseline).len() as u32);
    for k in 1..=points {
        let rig = mac_rig(&format!("hook-setup-sweep-{k}"), false);
        let mac = FakeMac::new();
        leave_leftover(&spy_on(&rig, &mac));
        rig.kill.arm(k);
        assert!(
            killable(|| spy_on(&rig, &mac).prepare(&rig)).is_none(),
            "{k}"
        );
        rig.kill.disarm();
        let spy = spy_on(&rig, &mac);
        let done = spy.prepare(&rig);
        assert_eq!(
            done.outcome,
            Outcome::Success,
            "killed at point {k}: {done:?}"
        );
        assert_eq!(ends(&rig, &spy, &mac), end, "killed at point {k}");
        let changes = mac.changes();
        let once: BTreeSet<&String> = changes.iter().collect();
        assert_eq!(
            once.len(),
            changes.len(),
            "killed at point {k}: {changes:?}"
        );
    }
}

/// Writes the copy of the file system table that an earlier release's setup left beside it.
pub(super) fn leave_leftover(spy: &Spy<'_>) {
    fs::create_dir_all(spy.fstab_leftover.parent().unwrap()).unwrap();
    let line = format!("UUID={UUID} /var/db/js-agent/scratch apfs rw,nobrowse\n");
    fs::write(&spy.fstab_leftover, line).unwrap();
}

/// Asserts that a systemd host whose params name a scratch root is refused by every step,
/// `bad_params`, naming the key, before anything is looked at or done.
#[test]
fn every_step_refuses_a_scratch_root_on_a_systemd_host() {
    let mut rig = Rig::new("hook-setup-systemd-scratch");
    rig.params.scratch_root = Some("/var/db/js-agent/scratch".into());
    let spy = Spy::new(&rig);
    let dones = [
        spy.prepare(&rig),
        spy.cutover(&rig),
        spy.verify(&rig),
        spy.rollback(&rig),
        spy.cleanup(&rig),
    ];
    for done in dones {
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::BadParams));
        assert_eq!(
            done.report.detail.as_deref(),
            Some(
                "scratch_root \"/var/db/js-agent/scratch\" is set, and only a launchd host's setup gives it a volume"
            )
        );
    }
    assert_eq!(spy.log(), Vec::<String>::new());
    assert!(rig.record().is_none());
}
