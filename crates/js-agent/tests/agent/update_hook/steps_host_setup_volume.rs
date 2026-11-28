//! The scratch volume of a Mac's host setup: a volume that is there is found and not made again,
//! one mounted elsewhere is moved onto the scratch root's parent (its mount point), and every way `diskutil` or a root can
//! fail defers `host_setup_missing` naming the cause.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use js_agent::update_hook::Outcome;
use js_agent::update_hook::step::{DATA_VOLUME, DISKUTIL, SCRATCH_VOLUME};

use super::fake_mac::{
    CONTAINER, DEVICE, FakeMac, QUOTA, UUID, Volume, container, dictionary, mac_rig, mount_of,
    scratch_of, spy_on,
};
use super::steps_host_setup::assert_missing;

/// `diskutil info -plist` of the Data volume.
fn data_info() -> String {
    format!("{DISKUTIL} info -plist {DATA_VOLUME}")
}

/// `diskutil apfs list -plist` of the container.
fn apfs_list() -> String {
    format!("{DISKUTIL} apfs list -plist {CONTAINER}")
}

/// Asserts that a scratch volume the container holds already, unmounted, is mounted, not made
/// again; a volume with ownership on is not given it again.
#[test]
fn a_scratch_volume_that_is_there_is_mounted_and_not_made_again() {
    let rig = mac_rig("hook-setup-volume-there", false);
    let mac = FakeMac::new();
    mac.with_volume(Volume {
        mount: None,
        owners: true,
    });
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let mount = mount_of(&rig).display().to_string();
    assert_eq!(
        mac.changes(),
        [format!(
            "{DISKUTIL} mount nobrowse -mountPoint {mount} {DEVICE}"
        )]
    );
}

/// Asserts that a scratch volume mounted somewhere else (in `/Volumes`, where macOS may mount
/// a volume by itself) is unmounted and then mounted on the scratch root's parent.
#[test]
fn a_scratch_volume_mounted_elsewhere_is_moved_onto_the_scratch_root() {
    let rig = mac_rig("hook-setup-volume-elsewhere", false);
    let mac = FakeMac::new();
    mac.with_volume(Volume {
        mount: Some(PathBuf::from("/Volumes/js-agent-scratch")),
        owners: true,
    });
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let mount = mount_of(&rig);
    assert_eq!(
        mac.changes(),
        [
            format!("{DISKUTIL} unmount {DEVICE}"),
            format!(
                "{DISKUTIL} mount nobrowse -mountPoint {} {DEVICE}",
                mount.display()
            ),
        ]
    );
    assert_eq!(mac.volume.borrow().as_ref().unwrap().mount, Some(mount));
}

/// Asserts that a mount that leaves the scratch volume's mount point on the state root's file
/// system defers naming both roots and the mount point.
#[test]
fn a_mount_that_does_not_move_the_scratch_root_off_the_state_root_defers() {
    let rig = mac_rig("hook-setup-mount-no-effect", false);
    let mac = FakeMac::new();
    mac.mount_takes.set(false);
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        &format!(
            "the scratch root {} is on the file system of the state root {} after {SCRATCH_VOLUME} was mounted on {}",
            scratch_of(&rig).display(),
            rig.params.state_root.display(),
            mount_of(&rig).display()
        ),
    );
    assert!(rig.record().is_none());
}

/// Asserts each answer of `diskutil` the setup cannot use defers with the detail that names it:
/// a failed command, output that is no dictionary, a Data volume with no container, a list
/// without the container or its free space, a scratch volume with no device, and a volume made
/// that the list does not show.
#[test]
fn every_diskutil_answer_the_setup_cannot_use_defers_naming_it() {
    let no_device = container(&format!(
        "<dict><key>APFSVolumeUUID</key><string>{UUID}</string><key>Name</key><string>{SCRATCH_VOLUME}</string></dict>"
    ));
    let no_free = dictionary(&format!(
        "<key>Containers</key><array><dict><key>ContainerReference</key><string>{CONTAINER}</string></dict></array>"
    ));
    let add = format!(
        "{DISKUTIL} apfs addVolume {CONTAINER} APFS {SCRATCH_VOLUME} -quota {QUOTA} -nomount"
    );
    let cases: [(&str, String, Result<String, &str>, &str); 7] = [
        (
            "failed",
            data_info(),
            Err("Could not find disk"),
            "/usr/sbin/diskutil info -plist /System/Volumes/Data: Could not find disk",
        ),
        (
            "garbled",
            data_info(),
            Ok("not a property list".to_owned()),
            "/usr/sbin/diskutil info -plist /System/Volumes/Data printed no property list dictionary",
        ),
        (
            "no-container",
            data_info(),
            Ok(dictionary("")),
            "/usr/sbin/diskutil shows /System/Volumes/Data with no APFSContainerReference",
        ),
        (
            "not-listed",
            apfs_list(),
            Ok(dictionary("<key>Containers</key><array></array>")),
            "/usr/sbin/diskutil apfs list does not list the container disk3",
        ),
        (
            "no-ceiling",
            apfs_list(),
            Ok(no_free),
            "/usr/sbin/diskutil shows the container disk3 with no CapacityFree",
        ),
        (
            "no-device",
            apfs_list(),
            Ok(no_device),
            "/usr/sbin/diskutil shows js-agent-scratch with no DeviceIdentifier",
        ),
        (
            "lost",
            add,
            Ok(String::new()),
            "/usr/sbin/diskutil does not list js-agent-scratch in disk3 after it made it",
        ),
    ];
    for (name, line, answer, detail) in cases {
        let rig = mac_rig(&format!("hook-setup-diskutil-{name}"), false);
        let mac = FakeMac::new();
        mac.answer(
            &line,
            answer.as_ref().map(String::as_str).map_err(|why| *why),
        );
        let done = spy_on(&rig, &mac).prepare(&rig);
        assert_missing(&done, detail);
        assert!(rig.record().is_none(), "{name}");
    }
}

/// Asserts that a root that cannot be looked at defers naming it and the cause.
#[test]
fn a_root_that_cannot_be_looked_at_defers() {
    let rig = mac_rig("hook-setup-look-fails", false);
    let mac = FakeMac::new();
    let mount = mount_of(&rig);
    mac.look_fails.borrow_mut().insert(mount.clone());
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        &format!("{}: the disk does not answer", mount.display()),
    );
}

/// Asserts that a root that cannot be made (a file where its parent should be) defers naming it
/// and the cause, with nothing made.
#[test]
fn a_root_that_cannot_be_made_defers() {
    let mut rig = mac_rig("hook-setup-root-unmade", false);
    let file = rig.tmp.path().join("db-file");
    fs::write(&file, "not a directory").unwrap();
    let mount = file.join("scratch");
    rig.params.scratch_root = Some(mount.join("runs"));
    let why = fs::DirBuilder::new()
        .recursive(true)
        .create(&file)
        .unwrap_err();
    let mac = FakeMac::new();
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(&done, &format!("{}: {why}", mount.display()));
    assert!(mac.volume.borrow().is_none());
}

/// Asserts that each change `diskutil` refuses defers naming the command and its error, with the
/// volume as the refusal left it: the volume's make, the look at its mount point, the unmount of
/// a volume mounted elsewhere, the mount, and the ownership of a volume that ignores it.
#[test]
fn every_diskutil_change_that_fails_defers_naming_it() {
    let elsewhere = || Volume {
        mount: Some(PathBuf::from("/Volumes/js-agent-scratch")),
        owners: true,
    };
    let cases: [(&str, String, Option<Volume>); 5] = [
        (
            "add",
            format!("apfs addVolume {CONTAINER} APFS {SCRATCH_VOLUME} -quota {QUOTA} -nomount"),
            None,
        ),
        ("info", format!("info -plist {DEVICE}"), None),
        ("unmount", format!("unmount {DEVICE}"), Some(elsewhere())),
        (
            "mount",
            "mount nobrowse -mountPoint <mount> disk3s7".to_owned(),
            None,
        ),
        ("owners", "enableOwnership <mount>".to_owned(), None),
    ];
    for (name, args, volume) in cases {
        let rig = mac_rig(&format!("hook-setup-diskutil-refuses-{name}"), false);
        let mount = mount_of(&rig).display().to_string();
        let line = format!("{DISKUTIL} {}", args.replace("<mount>", &mount));
        let mac = FakeMac::new();
        if let Some(volume) = volume {
            mac.with_volume(volume);
        }
        mac.answer(&line, Err("exit status: 1"));
        let done = spy_on(&rig, &mac).prepare(&rig);
        assert_missing(&done, &format!("{line}: exit status: 1"));
        assert!(rig.record().is_none(), "{name}");
    }
}

/// Asserts that a state root that cannot be looked at, and a mount point that can no longer be
/// looked at once its volume is mounted, defer naming the path.
#[test]
fn a_root_that_cannot_be_looked_at_before_or_after_the_mount_defers() {
    let rig = mac_rig("hook-setup-look-state", false);
    let mac = FakeMac::new();
    mac.look_fails
        .borrow_mut()
        .insert(rig.params.state_root.clone());
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        &format!(
            "{}: the disk does not answer",
            rig.params.state_root.display()
        ),
    );

    let rig = mac_rig("hook-setup-look-after-mount", false);
    let mac = FakeMac::new();
    let mount = mount_of(&rig);
    mac.look_fails_after_mount
        .borrow_mut()
        .insert(mount.clone());
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(
        &done,
        &format!("{}: the disk does not answer", mount.display()),
    );
}

/// Asserts that a state root that is there but open to others is made 0700 again, and that one
/// that cannot be made the hook's user's (a link to nothing) defers naming it.
#[test]
fn a_state_root_that_is_there_is_made_private_or_defers() {
    let rig = mac_rig("hook-setup-state-open", false);
    fs::create_dir_all(&rig.params.state_root).unwrap();
    fs::set_permissions(&rig.params.state_root, fs::Permissions::from_mode(0o755)).unwrap();
    let mac = FakeMac::new();
    assert_eq!(spy_on(&rig, &mac).prepare(&rig).outcome, Outcome::Success);
    let mode = fs::metadata(&rig.params.state_root)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o700);

    let rig = mac_rig("hook-setup-state-dangling", false);
    let state = rig.params.state_root.clone();
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(rig.tmp.path().join("nowhere"), &state).unwrap();
    let why = std::os::unix::fs::chown(&state, None, None).unwrap_err();
    let mac = FakeMac::new();
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(&done, &format!("{}: {why}", state.display()));
    assert!(mac.volume.borrow().is_none());
}

/// Asserts that a state root that cannot be made (a file where its parent should be) defers
/// naming it, before the scratch root is looked at.
#[test]
fn a_state_root_that_cannot_be_made_defers() {
    let mut rig = mac_rig("hook-setup-state-unmade", false);
    let file = rig.tmp.path().join("state-file");
    fs::write(&file, "not a directory").unwrap();
    rig.params.state_root = file.join("state");
    let why = fs::DirBuilder::new()
        .recursive(true)
        .create(&file)
        .unwrap_err();
    let done = spy_on(&rig, &FakeMac::new()).prepare(&rig);
    assert_missing(
        &done,
        &format!("{}: {why}", rig.params.state_root.display()),
    );
}

/// Asserts that a root whose own directory cannot be made though its parent can (a name longer
/// than a file system allows) defers naming it and the cause, not taken for one that is there.
#[test]
fn a_root_whose_directory_cannot_be_made_under_its_parent_defers() {
    let mut rig = mac_rig("hook-setup-root-too-long", false);
    let parent = rig.tmp.path().join("db/js-agent");
    let mount = parent.join("s".repeat(300));
    rig.params.scratch_root = Some(mount.join("runs"));
    fs::create_dir_all(&parent).unwrap();
    let why = fs::create_dir(&mount).unwrap_err();
    assert_ne!(why.kind(), std::io::ErrorKind::AlreadyExists);
    let mac = FakeMac::new();
    let done = spy_on(&rig, &mac).prepare(&rig);
    assert_missing(&done, &format!("{}: {why}", mount.display()));
    assert!(mac.volume.borrow().is_none());
}

/// Asserts that a scratch root that cannot be made on the mounted volume (a name longer than a
/// file system allows) defers naming it and the cause, with the volume mounted and the mount
/// point already given its mode, and no record written.
#[test]
fn a_scratch_root_that_cannot_be_made_on_the_volume_defers() {
    let mut rig = mac_rig("hook-setup-runs-too-long", false);
    let mount = mount_of(&rig);
    let scratch = mount.join("r".repeat(300));
    rig.params.scratch_root = Some(scratch.clone());
    let mac = FakeMac::new();
    let done = spy_on(&rig, &mac).prepare(&rig);
    let why = fs::create_dir(&scratch).unwrap_err();
    assert_missing(&done, &format!("{}: {why}", scratch.display()));
    assert_eq!(
        mac.volume.borrow().as_ref().unwrap().mount,
        Some(mount.clone())
    );
    assert_eq!(
        fs::metadata(&mount).unwrap().permissions().mode() & 0o7777,
        0o711
    );
    assert!(rig.record().is_none());
}
