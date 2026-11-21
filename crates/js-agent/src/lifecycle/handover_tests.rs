//! The handover file: its form decides whether a stop hands over, and the hook's write and
//! removal are what the agent then reads.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use js_testkit::dir::TestDir;

use super::{
    HANDOVER_FILE, HANDOVER_TEMP_FILE, HANDOVER_TEXT, Handover, read, remove, root_for, write,
};
use crate::egress::Config;
use crate::lifecycle::StateRoot;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[test]
fn a_written_file_is_a_request_and_a_removed_one_is_none() {
    let dir = scratch("handover-write");
    let root = dir.path();
    assert_eq!(read(root, me()), Handover::Absent);
    write(root).unwrap();
    assert_eq!(read(root, me()), Handover::Asked);
    let file = root.join(HANDOVER_FILE);
    assert_eq!(fs::read_to_string(&file).unwrap(), HANDOVER_TEXT);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!root.join(HANDOVER_TEMP_FILE).exists());
    // Written twice, it is still one request.
    write(root).unwrap();
    assert_eq!(read(root, me()), Handover::Asked);
    assert!(remove(root).unwrap());
    assert_eq!(read(root, me()), Handover::Absent);
    // Removing none is no error, and says there was none.
    assert!(!remove(root).unwrap());
}

#[test]
fn a_temporary_file_a_crash_left_open_to_others_is_written_over_private() {
    let dir = scratch("handover-temp");
    let root = dir.path();
    let temp = root.join(HANDOVER_TEMP_FILE);
    fs::write(&temp, "half").unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o666)).unwrap();
    write(root).unwrap();
    assert_eq!(read(root, me()), Handover::Asked);
}

#[test]
fn a_file_that_breaks_the_form_is_no_request_and_says_why() {
    let dir = scratch("handover-form");
    let root = dir.path();
    let file = root.join(HANDOVER_FILE);

    fs::write(&file, "x").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        read(root, me()),
        Handover::Refused("group or others have permission on it (mode 0640)".to_owned())
    );

    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let other = me().wrapping_add(1);
    assert_eq!(
        read(root, other),
        Handover::Refused(format!(
            "it is owned by user {}, not by the user running the agent ({other})",
            me()
        ))
    );

    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert_eq!(
        read(root, me()),
        Handover::Refused("it is not a regular file (a link is not accepted)".to_owned())
    );
    fs::remove_dir(&file).unwrap();

    let target = root.join("elsewhere");
    fs::write(&target, "x").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &file).unwrap();
    assert_eq!(
        read(root, me()),
        Handover::Refused("it is not a regular file (a link is not accepted)".to_owned())
    );
}

#[test]
fn a_root_that_cannot_be_looked_in_is_no_request_and_a_removal_there_fails() {
    let dir = scratch("handover-root");
    // A file where the directory should be: the look and the removal fail.
    let root = dir.path().join("not-a-dir");
    fs::write(&root, "x").unwrap();
    let Handover::Refused(why) = read(&root, me()) else {
        panic!("a root that is a file is read as a request or as none");
    };
    assert!(why.starts_with("it cannot be looked at: "));
    assert!(remove(&root).is_err());
    assert!(write(&root).is_err());
}

#[test]
fn an_agent_with_no_egress_ceiling_looks_for_a_handover_and_one_with_a_ceiling_never_does() {
    let dir = scratch("handover-root-for");
    let path = dir.path().join("state");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let state = StateRoot::open(&path).unwrap();
    let none = Config::none();
    let looked = root_for(Some(&state), &none);
    assert_eq!(looked.as_ref().map(StateRoot::path), Some(path.as_path()));
    assert!(
        root_for(None, &none).is_none(),
        "no state root, no handover"
    );
    #[cfg(target_os = "linux")]
    {
        let open = Config::from_flags(&crate::egress::Flags {
            mode: Some("open"),
            ip: Some("/usr/bin/true"),
            nft: Some("/usr/bin/true"),
            nsenter: Some("/usr/bin/true"),
            ..crate::egress::Flags::default()
        })
        .unwrap();
        assert!(
            root_for(Some(&state), &open).is_none(),
            "its next process removes the runs it would adopt: the stop drains"
        );
    }
}
