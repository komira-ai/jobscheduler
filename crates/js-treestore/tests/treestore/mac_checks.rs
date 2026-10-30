//! The store volume checks and the securelevel check, over injected readings, so that they
//! run on every platform.

use std::io;
use std::path::Path;

use js_treestore::mac::{
    SecurelevelError, VolumeError, VolumeFacts, check_securelevel, check_volumes,
};

const MNT_NOEXEC: u32 = 0x4;
const MNT_LOCAL: u32 = 0x1000;
const MNT_IGNORE_OWNERSHIP: u32 = 0x0020_0000;

fn apfs(flags: u32, fsid: i32) -> VolumeFacts {
    VolumeFacts {
        fs_type: "apfs".into(),
        flags,
        fsid: [fsid, 7],
    }
}

fn check(store: VolumeFacts, scratch: VolumeFacts) -> Result<(), VolumeError> {
    check_volumes(Path::new("/store"), Path::new("/scratch"), |path| {
        Ok(if path == Path::new("/store") {
            store.clone()
        } else {
            scratch.clone()
        })
    })
}

#[test]
fn an_apfs_volume_that_honours_ownership_holding_both_roots_is_accepted() {
    check(apfs(MNT_LOCAL, 1), apfs(MNT_LOCAL, 1)).unwrap();
}

#[test]
fn a_volume_that_ignores_ownership_is_refused() {
    let error = check(
        apfs(MNT_LOCAL | MNT_IGNORE_OWNERSHIP, 1),
        apfs(MNT_LOCAL, 1),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        VolumeError::IgnoresOwnership { what: "store" }
    ));
}

#[test]
fn a_noexec_volume_is_refused() {
    let error = check(apfs(MNT_LOCAL | MNT_NOEXEC, 1), apfs(MNT_LOCAL, 1)).unwrap_err();
    assert!(matches!(error, VolumeError::NoExec { what: "store" }));
}

#[test]
fn a_volume_that_is_not_apfs_is_refused_and_named() {
    let hfs = VolumeFacts {
        fs_type: "hfs".into(),
        ..apfs(MNT_LOCAL, 1)
    };
    match check(hfs, apfs(MNT_LOCAL, 1)).unwrap_err() {
        VolumeError::NotApfs { what, fs_type } => {
            assert_eq!(what, "store");
            assert_eq!(fs_type, "hfs");
        }
        other => panic!("{other}"),
    }
}

#[test]
fn a_scratch_root_on_another_volume_is_refused() {
    let error = check(apfs(MNT_LOCAL, 1), apfs(MNT_LOCAL, 2)).unwrap_err();
    assert!(matches!(error, VolumeError::DifferentVolumes));
}

#[test]
fn volumes_with_the_same_first_half_of_the_id_but_another_second_half_differ() {
    let other = VolumeFacts {
        fsid: [1, 8],
        ..apfs(MNT_LOCAL, 1)
    };
    let error = check(apfs(MNT_LOCAL, 1), other).unwrap_err();
    assert!(matches!(error, VolumeError::DifferentVolumes));
}

#[test]
fn a_volume_that_cannot_be_read_is_an_error_naming_which() {
    let store = check_volumes(Path::new("/store"), Path::new("/scratch"), |_| {
        Err(io::Error::other("no statfs"))
    })
    .unwrap_err();
    assert!(matches!(store, VolumeError::Read { what: "store", .. }));
    let scratch = check_volumes(Path::new("/store"), Path::new("/scratch"), |path| {
        if path == Path::new("/store") {
            Ok(apfs(MNT_LOCAL, 1))
        } else {
            Err(io::Error::other("no statfs"))
        }
    })
    .unwrap_err();
    assert!(matches!(
        scratch,
        VolumeError::Read {
            what: "scratch",
            ..
        }
    ));
}

#[test]
fn the_default_macos_securelevel_and_zero_are_accepted() {
    check_securelevel(&|| Ok(-1)).unwrap();
    check_securelevel(&|| Ok(0)).unwrap();
}

#[test]
fn a_securelevel_above_zero_is_refused() {
    for level in [1, 2] {
        let error = check_securelevel(&|| Ok(level)).unwrap_err();
        assert!(matches!(error, SecurelevelError::TooHigh(got) if got == level));
    }
}

#[test]
fn an_unreadable_securelevel_is_refused() {
    let error = check_securelevel(&|| Err(io::Error::other("no sysctl"))).unwrap_err();
    assert!(matches!(error, SecurelevelError::Read(_)));
}
