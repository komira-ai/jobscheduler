//! Tests of the account's record attributes and its home folder.

use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use js_testkit::dir::TestDir;

use super::*;
use crate::mac_accounts::tests::rig::{me, my_gid};

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-accounts-home-{name}"),
    )
}

/// Asserts the attributes of a new account's record: its uid, its primary group, its home,
/// the shell that cannot log in, its real name and the hidden flag, and none that carries a
/// password or an authentication authority.
#[test]
fn the_record_has_the_attributes_of_a_hidden_account_that_cannot_log_in() {
    let user = NewUser {
        name: "kbf-acct-3".to_owned(),
        uid: 612,
        gid: 20,
        home: PathBuf::from("/Users/kbf-acct-3"),
        generated_uid: generated_uid(612),
    };
    let attributes = account_attributes(&user);
    assert_eq!(
        attributes,
        [
            ("dsAttrTypeStandard:UniqueID", "612".to_owned()),
            ("dsAttrTypeStandard:PrimaryGroupID", "20".to_owned()),
            (
                "dsAttrTypeStandard:NFSHomeDirectory",
                "/Users/kbf-acct-3".to_owned()
            ),
            ("dsAttrTypeStandard:UserShell", "/usr/bin/false".to_owned()),
            ("dsAttrTypeStandard:RealName", "kbf-acct-3".to_owned()),
            ("dsAttrTypeNative:IsHidden", "1".to_owned()),
        ]
    );
    for (attribute, _) in attributes {
        assert!(
            !attribute.contains("Password") && !attribute.contains("Authentication"),
            "{attribute}"
        );
    }
}

/// Asserts the slot's `GeneratedUID`: the fixed prefix and the uid in twelve digits, so the same
/// uid always has the same identifier whatever the account's name, a different uid a different
/// one, and each is the 8-4-4-4-12 shape of a UUID; and that a record is created with its name
/// and that identifier, which the directory takes only at creation.
#[test]
fn a_slots_generated_uid_is_fixed_by_its_uid_and_given_at_creation() {
    assert_eq!(generated_uid(612), "4B424641-4343-5400-8000-000000000612");
    assert_eq!(generated_uid(0), "4B424641-4343-5400-8000-000000000000");
    assert_eq!(
        generated_uid(u32::MAX),
        "4B424641-4343-5400-8000-004294967295"
    );
    assert_ne!(generated_uid(612), generated_uid(613));
    let lengths: Vec<usize> = generated_uid(612).split('-').map(str::len).collect();
    assert_eq!(lengths, [8, 4, 4, 4, 12]);
    let user = |name: &str| NewUser {
        name: name.to_owned(),
        uid: 612,
        gid: 20,
        home: PathBuf::from("/Users").join(name),
        generated_uid: generated_uid(612),
    };
    for name in ["kbf-acct-3", "kbf-acct-9"] {
        assert_eq!(
            creation_attributes(&user(name)),
            [
                ("dsAttrTypeStandard:RecordName", name.to_owned()),
                (
                    "dsAttrTypeStandard:GeneratedUID",
                    "4B424641-4343-5400-8000-000000000612".to_owned()
                ),
            ]
        );
    }
    for (attribute, _) in account_attributes(&user("kbf-acct-3")) {
        assert!(
            !attribute.contains("GeneratedUID") && !attribute.contains("RecordName"),
            "{attribute} is set after creation, where the directory refuses it"
        );
    }
}

/// Asserts that a home is made with mode 0700, owned by the uid and group it was made for,
/// and that a home that exists is refused and left as it is: a new account takes nothing of
/// what was there.
#[test]
fn a_home_is_made_0700_and_an_existing_one_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("make");
    make_home(&dir, "kbf-acct-1", me(), my_gid()).unwrap();
    let meta = std::fs::metadata(dir.join("kbf-acct-1")).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
    assert_eq!((meta.uid(), meta.gid()), (me(), my_gid()));

    std::fs::write(dir.join("kbf-acct-1/kept"), "x").unwrap();
    let again = make_home(&dir, "kbf-acct-1", me(), my_gid()).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        again.to_string(),
        format!("the home {} exists", dir.join("kbf-acct-1").display())
    );
    assert!(dir.join("kbf-acct-1/kept").is_file());
}

/// Asserts that a home is not made through a link at the folder of the homes, in a folder
/// that is not there, or for an owner the process may not give a folder to (every failure is
/// an error and nothing is left behind that the caller does not name).
#[test]
fn a_home_is_refused_through_a_link_or_a_missing_folder() {
    let dir = scratch("refuse");
    std::fs::create_dir(dir.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    assert!(make_home(&dir.join("link"), "kbf-acct-1", me(), my_gid()).is_err());
    assert!(!dir.join("real/kbf-acct-1").exists());
    assert!(make_home(&dir.join("missing"), "kbf-acct-1", me(), my_gid()).is_err());
    let nested = make_home(&dir.join("real"), "no/such/folder", me(), my_gid()).unwrap_err();
    assert_eq!(nested.kind(), io::ErrorKind::NotFound);
    // Giving the folder to another uid is refused unless the process is root.
    let other = make_home(&dir.join("real"), "kbf-acct-2", me() + 1, my_gid());
    assert_eq!(other.is_ok(), me() == 0);
}
