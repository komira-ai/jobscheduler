//! The host setup of a Mac in `prepare` (`socket_group`, `scratch_root`): the helper's socket
//! group is made before the helper is bootstrapped, the state root and the scratch root are made
//! and the scratch root gets a volume of its own; a part that cannot be made defers
//! `host_setup_missing` with nothing written and nothing cordoned; a rerun after a kill at any
//! point ends as an uninterrupted run; a systemd host that names a part is refused.
//! The error paths of the volume are in steps_host_setup_volume.rs.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use js_agent::update_hook::step::{DISKUTIL, DSCL, DSEDITGROUP, SCRATCH_VOLUME};
use js_agent::update_hook::{Class, Done, LAUNCHCTL, Outcome, Reason};

use super::fake_mac::{
    DEVICE, FIRST_GID, FakeMac, GROUP, QUOTA, UUID, Volume, mac_rig, scratch_of, spy_on,
};
use super::step_rig::{MAC_UNIT, RETRY_AFTER_SECS, Rig, Spy, killable};

/// The permission bits of the directory at `path`.
fn mode(path: &Path) -> u32 {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir(), "{}", path.display());
    meta.permissions().mode() & 0o7777
}

/// The line the setup writes to the file system table for the scratch volume.
pub(super) fn fstab_line(rig: &Rig) -> String {
    format!(
        "UUID={UUID} {} apfs rw,nobrowse\n",
        scratch_of(rig).display()
    )
}

/// What a first install on an empty Mac changes, in order.
pub(super) fn first_changes(rig: &Rig) -> Vec<String> {
    let scratch = scratch_of(rig).display().to_string();
    vec![
        format!("{DSEDITGROUP} -o create {GROUP}"),
        format!("{DISKUTIL} apfs addVolume disk3 APFS {SCRATCH_VOLUME} -quota {QUOTA} -nomount"),
        format!("{DISKUTIL} mount -mountPoint {scratch} {DEVICE}"),
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

/// Asserts that the first install on a Mac with no `_jsagent` group makes it in `prepare`, and
/// that `cutover` bootstraps the helper, which names the group as its socket's, only after.
/// Catches a helper that starts on a Mac without its socket group and exits.
#[test]
fn a_first_install_makes_the_socket_group_and_then_bootstraps_the_helper() {
    let rig = mac_rig("hook-setup-group-absent", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(mac.groups.borrow().get(GROUP), Some(&FIRST_GID));
    let log = spy.log();
    let at = |prefix: &str| log.iter().position(|call| call.starts_with(prefix));
    let create = at(&format!("run({DSEDITGROUP} -o create {GROUP})")).expect("created");
    let bootstrap = at(&format!("run({LAUNCHCTL} bootstrap system ")).expect("bootstrapped");
    assert!(create < bootstrap, "{log:?}");
}

/// Asserts that a group the local directory lists already is looked up and not made again.
#[test]
fn a_socket_group_that_is_there_is_not_made_again() {
    let rig = mac_rig("hook-setup-group-present", false);
    let mac = FakeMac::new();
    mac.with_group(GROUP, 401);
    let spy = spy_on(&rig, &mac);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert!(
        spy.log()
            .contains(&format!("run({DSCL} . -list /Groups PrimaryGroupID)"))
    );
    assert!(
        mac.changes()
            .iter()
            .all(|change| !change.starts_with(DSEDITGROUP))
    );
    assert_eq!(mac.groups.borrow().get(GROUP), Some(&401));
}

/// Asserts that a first install on an empty Mac makes the state root and the scratch root
/// (0700, their parent 0755), makes the scratch volume with nine tenths of the container as its
/// quota, names it in the file system table, mounts it on the scratch root and gives it
/// ownership, in that order, and writes its record. Catches an agent that refuses to start
/// because its two roots share a file system.
#[test]
fn a_first_install_makes_the_roots_and_gives_the_scratch_root_its_own_volume() {
    let rig = mac_rig("hook-setup-roots", false);
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let scratch = scratch_of(&rig);
    assert_eq!(mode(&rig.params.state_root), 0o700);
    assert_eq!(mode(&scratch), 0o700);
    assert_eq!(mode(scratch.parent().unwrap()), 0o755);
    assert_eq!(mac.changes(), first_changes(&rig));
    assert_eq!(fs::read_to_string(&spy.fstab).unwrap(), fstab_line(&rig));
    assert_eq!(
        *mac.volume.borrow(),
        Some(Volume {
            mount: Some(scratch),
            owners: true,
        })
    );
    assert!(rig.record().is_some());
}

/// Asserts that a `prepare` on a Mac set up already changes nothing: no group, volume, table line
/// or mount is made again.
#[test]
fn a_prepare_on_a_set_up_mac_changes_nothing() {
    let rig = mac_rig("hook-setup-rerun", false);
    let mac = FakeMac::new();
    assert_eq!(spy_on(&rig, &mac).prepare(&rig).outcome, Outcome::Success);
    let before = (
        mac.changes(),
        fs::read_to_string(rig.tmp.path().join("fstab")).unwrap(),
    );
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let after = (
        mac.changes(),
        fs::read_to_string(rig.tmp.path().join("fstab")).unwrap(),
    );
    assert_eq!(after, before);
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
        .position(|call| call.contains(" mount -mountPoint "));
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
    mac.with_group(GROUP, 20);
    let spy = spy_on(&rig, &mac);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
    assert_eq!(rig.cordon_expiry(), None);
    assert!(rig.record().is_none());
}

/// Asserts that a socket group with staff's gid, which the helper refuses, defers naming it, and
/// that nothing is written.
#[test]
fn a_socket_group_with_staffs_gid_defers() {
    let rig = mac_rig("hook-setup-group-staff", false);
    let mac = FakeMac::new();
    mac.with_group(GROUP, 20);
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        "the group _jsagent has gid 20, staff's: the helper refuses a socket group its lease users are in",
    );
    assert!(rig.record().is_none());
    assert!(!rig.params.state_root.exists());
}

/// Asserts that a group the local directory lists with no gid defers, quoting the line.
#[test]
fn a_socket_group_listed_with_no_gid_defers() {
    let rig = mac_rig("hook-setup-group-no-gid", false);
    let mac = FakeMac::new();
    mac.answer(
        &format!("{DSCL} . -list /Groups PrimaryGroupID"),
        Ok("staff 20\n_jsagent\n"),
    );
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        "/usr/bin/dscl lists the group _jsagent with no gid: \"_jsagent\"",
    );
}

/// Asserts that a directory that cannot be listed defers naming the command and its error.
#[test]
fn a_directory_that_cannot_be_listed_defers() {
    let rig = mac_rig("hook-setup-group-unlisted", false);
    let mac = FakeMac::new();
    mac.answer(
        &format!("{DSCL} . -list /Groups PrimaryGroupID"),
        Err("eDSRecordNotFound"),
    );
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        "/usr/bin/dscl . -list /Groups PrimaryGroupID: eDSRecordNotFound",
    );
}

/// Asserts that a group `dseditgroup` cannot make defers naming the command and its error.
#[test]
fn a_socket_group_that_cannot_be_made_defers() {
    let rig = mac_rig("hook-setup-group-refused", false);
    let mac = FakeMac::new();
    mac.answer(
        &format!("{DSEDITGROUP} -o create {GROUP}"),
        Err("eDSPermissionError"),
    );
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        "/usr/sbin/dseditgroup -o create _jsagent: eDSPermissionError",
    );
}

/// Asserts that a group `dseditgroup` says it made and the directory does not list defers.
#[test]
fn a_socket_group_made_and_not_listed_defers() {
    let rig = mac_rig("hook-setup-group-lost", false);
    let mac = FakeMac::new();
    mac.answer(&format!("{DSEDITGROUP} -o create {GROUP}"), Ok(""));
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        "the group _jsagent is not listed after /usr/sbin/dseditgroup made it",
    );
}

/// Asserts that a Mac whose params name the scratch root and no socket group gets its roots and
/// no group lookup.
#[test]
fn a_scratch_root_alone_is_set_up_without_a_group() {
    let mut rig = mac_rig("hook-setup-roots-alone", false);
    rig.params.settings.socket_group = None;
    let mac = FakeMac::new();
    let spy = spy_on(&rig, &mac);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert!(spy.log().iter().all(|call| !call.contains(DSCL)));
    assert_eq!(mac.changes(), first_changes(&rig)[1..].to_vec());
}

/// Everything a host setup leaves behind, with the rig's tree written `<root>`: the record, the
/// file system table, the modes of the roots, the groups and the volume.
fn ends(rig: &Rig, spy: &Spy<'_>, mac: &FakeMac) -> String {
    let record = rig
        .record()
        .map(|record| serde_json::to_string(&record).unwrap());
    let state = format!(
        "{record:?} {:?} {:o} {:o} {:?} {:?}",
        fs::read_to_string(&spy.fstab).ok(),
        mode(&rig.params.state_root),
        mode(&scratch_of(rig)),
        mac.groups.borrow(),
        mac.volume.borrow(),
    );
    state.replace(&rig.tmp.path().display().to_string(), "<root>")
}

/// Asserts that a first install's `prepare` killed at any point of the host setup and run again
/// ends as one that was never killed, with the group, the volume and the table line made once.
#[test]
fn a_setup_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let baseline = mac_rig("hook-setup-sweep-base", false);
    let base_mac = FakeMac::new();
    let base_spy = spy_on(&baseline, &base_mac);
    assert_eq!(base_spy.prepare(&baseline).outcome, Outcome::Success);
    let points = baseline.kill.points();
    let end = ends(&baseline, &base_spy, &base_mac);
    assert!(points > first_changes(&baseline).len() as u32);
    for k in 1..=points {
        let rig = mac_rig(&format!("hook-setup-sweep-{k}"), false);
        let mac = FakeMac::new();
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

/// Asserts that a systemd host whose params name a socket group or a scratch root is refused by
/// every step, `bad_params`, naming the key, before anything is looked at or done.
#[test]
fn every_step_refuses_a_host_setup_key_on_a_systemd_host() {
    let cases = [
        (
            "socket",
            "socket_group \"_jsagent\" is set, and only a launchd host's setup makes the group",
        ),
        (
            "scratch",
            "scratch_root \"/var/db/js-agent/scratch\" is set, and only a launchd host's setup gives it a volume",
        ),
    ];
    for (key, detail) in cases {
        let mut rig = Rig::new(&format!("hook-setup-systemd-{key}"));
        if key == "socket" {
            rig.params.settings.socket_group = Some(GROUP.to_owned());
        } else {
            rig.params.scratch_root = Some("/var/db/js-agent/scratch".into());
        }
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
            assert_eq!(done.report.detail.as_deref(), Some(detail));
        }
        assert_eq!(spy.log(), Vec::<String>::new());
        assert!(rig.record().is_none());
    }
}
