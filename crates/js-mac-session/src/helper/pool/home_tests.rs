//! The reset of a pool user keeps its home folder: it empties it and never removes or
//! makes it again, which macOS refuses for a folder of `/Users` to a process without Full
//! Disk Access.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::tests::{home, slots};
use crate::helper::tests::{Rig, log, me, rig};

/// Makes the folder `homes` of the rig refuse the removal of its entries, as `/Users`
/// refuses the helper's, and puts the permissions back when it goes.
struct Protected(PathBuf);

impl Protected {
    /// Takes the write permission off `rig`'s homes folder.
    fn new(rig: &Rig) -> Self {
        let homes = rig.helper.settings.homes.clone();
        std::fs::set_permissions(&homes, std::fs::Permissions::from_mode(0o500)).unwrap();
        Self(homes)
    }
}

impl Drop for Protected {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// Whether this process is root, which no permission bit stops.
fn root() -> bool {
    rustix::process::geteuid().is_root()
}

/// The sorted names directly in `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    found
}

/// Takes slot 0's user in `rig` and plants in its home a file, a dot file, a dot
/// directory with a file and a subdirectory with a nested one.
fn plant(rig: &Rig) -> PathBuf {
    rig.helper.user_create("80.1", None).unwrap();
    let home = home(rig, 0);
    std::fs::create_dir_all(home.join("Documents/deep/er")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join("file"), "x").unwrap();
    std::fs::write(home.join(".zshrc"), "x").unwrap();
    std::fs::write(home.join(".ssh/id"), "x").unwrap();
    std::fs::write(home.join("Documents/deep/er/note"), "x").unwrap();
    home
}

/// Catches a reset that ends in the removal of the home folder itself: where the folder
/// above it refuses the removal of its entries (as `/Users` does), the reset still ends
/// clean, the slot is free (no replacement made), and the home is the same folder as
/// before, empty.
#[test]
fn a_reset_where_the_home_cannot_be_removed_ends_clean_with_the_home_kept() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let rig = rig("home-protected", 1);
    let home = plant(&rig);
    let before = std::fs::metadata(&home).unwrap().ino();
    let _protected = Protected::new(&rig);
    log(&rig);
    assert_eq!(rig.helper.user_delete("80.1"), Ok(true));
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("delete_user")),
        "reset, not replaced: {calls:#?}"
    );
    assert_eq!(rig.helper.pool_status().free, slots(&[0]));
    assert_eq!(std::fs::metadata(&home).unwrap().ino(), before);
    assert_eq!(names(&home), Vec::<String>::new());
}

/// Catches entries of a home left for the next lease: files, dot files, dot directories
/// and nested directories below the home are all gone after the reset, and the home is
/// the same folder, mode 0700, with the host asked to give it back to the pool user.
#[test]
fn a_reset_empties_the_home_whatever_is_below_it() {
    let rig = rig("home-emptied", 1);
    let home = plant(&rig);
    let before = std::fs::metadata(&home).unwrap().ino();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    log(&rig);
    assert_eq!(rig.helper.user_delete("80.1"), Ok(true));
    assert_eq!(names(&home), Vec::<String>::new());
    assert_eq!(std::fs::metadata(&home).unwrap().ino(), before);
    let made = format!(
        "make_home {} {}:{}",
        home.display(),
        me(),
        crate::helper::tests::my_gid()
    );
    assert!(log(&rig).contains(&made));
    assert_eq!(rig.helper.pool_status().free, slots(&[0]));
}

/// Catches a reset that goes past an entry below the home it cannot remove: the reset
/// fails, naming that entry, and removes what it can (the rest of the home) first.
#[test]
fn an_entry_below_the_home_that_cannot_be_removed_fails_the_reset_naming_it() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let rig = rig("home-stuck", 1);
    let home = plant(&rig);
    let stuck = home.join("Documents/deep");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
    let uid = me();
    let error = rig.helper.reset("kbf-lease-pool-0", uid).unwrap_err();
    let named = stuck.join("er");
    assert!(
        error.to_string().starts_with(&format!(
            "the sweep for kbf-lease-pool-0 is incomplete: {}",
            home.join("Documents").display()
        )),
        "{error}"
    );
    assert!(
        error.to_string().contains(&named.display().to_string()),
        "{error}"
    );
    assert!(!home.join("file").exists());
    assert!(!home.join(".zshrc").exists());
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Catches a reset that removes the home folder, or deletes the user, when a step fails:
/// a reset whose home cannot be given back leaves the slot broken, naming that step, and
/// deletes no record; the home was emptied and is the same folder, where the folder above
/// refuses the removal of its entries, and the next `pool-create` resets the user again
/// in place and frees the slot.
#[test]
fn a_reset_that_fails_keeps_the_emptied_home_and_the_next_one_frees_the_slot() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let rig = rig("home-replace", 1);
    let home = plant(&rig);
    let before = std::fs::metadata(&home).unwrap().ino();
    let _protected = Protected::new(&rig);
    rig.host
        .state()
        .fail_script
        .insert("make_home", [true].into());
    log(&rig);
    let error = rig.helper.user_delete("80.1").unwrap_err();
    assert!(
        error.contains(&format!("making {}: make_home failed", home.display())),
        "{error}"
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));
    assert_eq!(std::fs::metadata(&home).unwrap().ino(), before);
    assert_eq!(names(&home), Vec::<String>::new());
    assert_eq!(rig.helper.pool_create().free, slots(&[0]));
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("delete_user")),
        "{calls:#?}"
    );
}
