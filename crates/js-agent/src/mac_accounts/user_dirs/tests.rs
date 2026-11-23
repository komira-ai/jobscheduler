//! The check of an account's per-user folders as `getconf` names them.

use std::fs;

use js_testkit::dir::TestDir;

use super::*;

/// This process's own uid: the owner of every directory a test makes.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A per-user folders root with one slot's folder in it, `T` and `C` made.
fn folders(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-user-dirs-{name}"));
    let root = dir.join("folders");
    fs::create_dir_all(root.join("ab/slot/T")).unwrap();
    fs::create_dir_all(root.join("ab/slot/C")).unwrap();
    let root = fs::canonicalize(root).unwrap();
    (dir, root)
}

/// Asserts that a folder of the account below the root is taken as the kernel resolves it,
/// the trailing slash and line `getconf` prints dropped, through a link too.
///
/// Catches a grant spelled as `getconf` printed it, which a profile does not match.
#[test]
fn a_folder_of_the_account_below_the_root_is_taken_resolved() {
    let (dir, root) = folders("ok");
    let temp = root.join("ab/slot/T");
    let printed = format!("{}/\n", temp.display());
    assert_eq!(checked(&printed, &root, me()).unwrap(), temp);
    std::os::unix::fs::symlink(root.join("ab/slot/C"), dir.join("cache")).unwrap();
    let through = format!("{}\n", dir.join("cache").display());
    assert_eq!(
        checked(&through, &root, me()).unwrap(),
        root.join("ab/slot/C")
    );
}

/// Asserts each refusal, by its text: nothing printed, a path that does not resolve, the root
/// itself, a path outside the root, a file, and a folder of another uid.
///
/// Catches a profile that grants writes to whatever a lookup answered, such as all of
/// `/var/folders` or another account's folder.
#[test]
fn what_is_not_the_accounts_own_folder_is_refused() {
    let (dir, root) = folders("refused");
    assert_eq!(
        checked(" \n", &root, me()).unwrap_err().to_string(),
        "getconf printed no folder"
    );
    let missing = root.join("ab/slot/X");
    let why = checked(&missing.display().to_string(), &root, me()).unwrap_err();
    assert!(
        why.to_string()
            .starts_with(&format!("{}: ", missing.display())),
        "{why}"
    );
    let at_root = root.display().to_string();
    assert_eq!(
        checked(&at_root, &root, me()).unwrap_err().to_string(),
        format!("{at_root}: not below {at_root}")
    );
    let outside = fs::canonicalize(dir.path()).unwrap().display().to_string();
    assert_eq!(
        checked(&outside, &root, me()).unwrap_err().to_string(),
        format!("{outside}: not below {at_root}")
    );
    let file = root.join("ab/slot/file");
    fs::write(&file, "x").unwrap();
    let file = file.display().to_string();
    assert_eq!(
        checked(&file, &root, me()).unwrap_err().to_string(),
        format!("{file}: not a directory")
    );
    let temp = root.join("ab/slot/T").display().to_string();
    let other = me() + 1;
    assert_eq!(
        checked(&temp, &root, other).unwrap_err().to_string(),
        format!("{temp}: owned by uid {}, not uid {other}", me())
    );
}
