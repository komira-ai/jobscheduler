//! Who a job runs as: an image's `User` resolved in the image's own files.

use std::fs;
use std::path::Path;

use super::user::{Identity, UserError, resolve};
use crate::test_dir::TestDir;

const PASSWD: &str = "\
# a comment

root:x:0:0:root:/root:/bin/sh
app:x:1000:1001:app:/home/app:/bin/sh
nobody:x:65534:65534:nobody:/:/sbin/nologin
broken:x:notanumber:5
";

const GROUP: &str = "\
root:x:0:
staff:x:50:app,other
app:x:1001:
video:x:44:other,app
";

/// A root whose `etc/passwd` and `etc/group` are `passwd` and `group`.
fn root_with(passwd: Option<&str>, group: Option<&str>) -> TestDir {
    let root = TestDir::under(std::env::temp_dir(), "js-oci-user");
    fs::create_dir_all(root.join("etc")).expect("etc");
    for (name, text) in [("passwd", passwd), ("group", group)] {
        if let Some(text) = text {
            fs::write(root.join("etc").join(name), text).expect("write");
        }
    }
    root
}

fn ids(user: &str, root: &Path) -> (u32, u32, Vec<u32>) {
    let who = resolve(user, root).expect(user);
    (who.uid, who.gid, who.additional_gids)
}

/// Proves an image that names no `User` runs as container root, 0:0, with no extra groups
/// and without reading the image (the root need not exist), as Docker runs it.
#[test]
fn no_user_is_container_root() {
    let who = resolve("", Path::new("/nonexistent/root")).expect("root");
    assert_eq!(who, Identity::root());
    assert_eq!((who.uid, who.gid, who.additional_gids), (0, 0, vec![]));
}

/// Proves a numeric `User` is taken as written: a uid the passwd file does not list keeps
/// group 0, and `uid:gid` sets both, with or without passwd and group files in the image.
#[test]
fn a_numeric_user_is_taken_as_written() {
    for root in [root_with(Some(PASSWD), Some(GROUP)), root_with(None, None)] {
        assert_eq!(ids("4000", &root), (4000, 0, vec![]));
        assert_eq!(ids("4000:4001", &root), (4000, 4001, vec![]));
    }
}

/// Proves a name is looked up in the image's passwd file: its uid and primary gid, and the
/// groups that list it as a member (not its primary group twice), as Docker does.
#[test]
fn a_name_is_looked_up_in_the_images_passwd_and_group_files() {
    let root = root_with(Some(PASSWD), Some(GROUP));
    assert_eq!(ids("app", &root), (1000, 1001, vec![50, 44]));
    assert_eq!(ids("root", &root), (0, 0, vec![]));
}

/// Proves a uid the passwd file lists takes that entry's primary group and member groups.
#[test]
fn a_listed_uid_takes_its_entrys_groups() {
    let root = root_with(Some(PASSWD), Some(GROUP));
    assert_eq!(ids("1000", &root), (1000, 1001, vec![50, 44]));
}

/// Proves an explicit group replaces the primary group and drops the member groups, by
/// name from the group file or by number, and that a group the file does not list is refused.
#[test]
fn an_explicit_group_is_a_name_or_a_number() {
    let root = root_with(Some(PASSWD), Some(GROUP));
    assert_eq!(ids("app:staff", &root), (1000, 50, vec![]));
    assert_eq!(ids("app:44", &root), (1000, 44, vec![]));
    assert_eq!(ids("app:51", &root), (1000, 51, vec![]));
    assert_eq!(ids("4000:video", &root), (4000, 44, vec![]));
    assert!(matches!(
        resolve("app:nogroup", &root),
        Err(UserError::NoGroup(name)) if name == "nogroup"
    ));
}

/// Proves a name no passwd file lists is refused, also where the image has no passwd file,
/// and a passwd line without a numeric uid does not name a user.
#[test]
fn a_name_the_image_does_not_list_is_refused() {
    for root in [root_with(Some(PASSWD), Some(GROUP)), root_with(None, None)] {
        for name in ["ghost", "broken"] {
            let why = resolve(name, &root).expect_err(name);
            assert!(matches!(&why, UserError::NoUser(n) if n == name), "{why}");
        }
    }
}

/// Proves a text that is not `user` or `user:group` is refused.
#[test]
fn an_empty_part_is_a_syntax_error() {
    let root = root_with(Some(PASSWD), Some(GROUP));
    for text in [":5", "app:", ":"] {
        let why = resolve(text, &root).expect_err(text);
        assert!(matches!(&why, UserError::Syntax(t) if t == text), "{why}");
    }
}

/// Proves an id the job's namespace does not map (65536 and up) is refused, whichever of
/// the uid, the gid or a member group it is, since the container would not start.
#[test]
fn an_id_outside_the_namespace_is_refused() {
    let root = root_with(Some(PASSWD), Some(GROUP));
    assert!(matches!(
        resolve("65536", &root),
        Err(UserError::Range(text, 65_536)) if text == "65536"
    ));
    assert!(matches!(
        resolve("1000:65536", &root),
        Err(UserError::Range(..))
    ));
    let member = root_with(Some("w:x:7:7\n"), Some("w:x:7:\nbig:x:70000:w\n"));
    assert!(matches!(resolve("w", &member), Err(UserError::Range(..))));
}

/// Proves the files are read through the image's root: an absolute link in the image leads
/// to the image's own file and never to the host's, and a link out of the root finds
/// nothing there.
#[test]
fn a_link_in_the_image_leads_only_inside_it() {
    let root = root_with(None, None);
    fs::write(root.join("real-passwd"), "linked:x:1234:1235\n").expect("file");
    std::os::unix::fs::symlink("/real-passwd", root.join("etc/passwd")).expect("link");
    assert_eq!(ids("linked", &root), (1234, 1235, vec![]));
    fs::remove_file(root.join("etc/passwd")).expect("unlink");
    std::os::unix::fs::symlink("../../../../../../../etc/hostname", root.join("etc/passwd"))
        .expect("link");
    assert!(matches!(
        resolve("anyone", &root),
        Err(UserError::NoUser(_))
    ));
}

/// Proves a passwd file that exists and cannot be read as text (here a directory, and
/// bytes that are not UTF-8), an `etc` that is a file and a root that cannot be opened are errors that name the
/// file, not an empty file.
#[test]
fn a_file_that_cannot_be_read_is_an_error() {
    let root = root_with(None, None);
    fs::create_dir(root.join("etc/passwd")).expect("dir");
    let why = resolve("app", &root).expect_err("a directory");
    assert!(matches!(&why, UserError::Read { .. }), "{why}");
    assert!(why.to_string().contains("etc/passwd"), "{why}");
    let root = root_with(None, None);
    fs::write(root.join("etc/passwd"), [0xff, 0xfe, b'\n']).expect("bytes");
    assert!(matches!(resolve("app", &root), Err(UserError::Read { .. })));
    let no_etc = TestDir::under(std::env::temp_dir(), "js-oci-user-no-etc");
    fs::write(no_etc.join("etc"), "a file where the directory is").expect("file");
    assert!(matches!(
        resolve("app", &no_etc),
        Err(UserError::Read { .. })
    ));
    let gone = root.join("missing-root");
    assert!(matches!(resolve("app", &gone), Err(UserError::Read { .. })));
}
