//! Edges of the sweep the larger tests above do not reach: a path whose parent is not
//! there, an entry that goes between a listing and its examination, a park that has
//! no free name or cannot be written, a folder the owner must be given back by name,
//! and a directory with no extended attributes.

use super::*;
use crate::sweep::flags::make_owner_rwx;

/// Catches: a path below a parent that does not exist taken as protected (there is
/// nothing there to be protected, and a clean would skip it for good).
#[test]
fn a_path_below_a_missing_parent_is_not_protected() {
    let dir = scratch("protected-missing-parent");
    assert!(!path_is_protected(&dir.join("absent").join("child")));
}

/// Catches: an entry that went between the listing and its examination taken as an
/// error by the walk that clears flags below a kept folder, or the walk stopping
/// there instead of going on to the next entry.
#[test]
fn clear_flags_within_passes_over_an_entry_gone_since_the_listing() {
    let dir = scratch("flags-gone");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T")).unwrap();
    fs::write(entry.join("gone"), "x").unwrap();
    fs::write(entry.join("stays"), "x").unwrap();
    let gone = entry.join("gone");
    let swept = vanishing(after_listing(1, move || remove(&gone)), || {
        clear_flags_within(&entry, me(), &["T"])
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(entry.join("stays").exists());
    assert!(!entry.join("gone").exists());
}

/// Catches: a deep tree whose every park name is taken moved over one of them, or
/// left without an error: the move must fail and say there is no free name.
#[test]
fn a_park_with_every_name_taken_fails_the_move_and_says_so() {
    let dir = scratch("park-full");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    build_deep(&top, 100);
    let pid = std::process::id();
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        {
            let top = top.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "d") && !done.replace(true) {
                    for n in 0..PARK_NAMES {
                        fs::create_dir(top.join(format!(".kbf-deep-{pid}-{n}"))).unwrap();
                    }
                }
            }
        },
        || empty(&top, me()),
    );
    assert!(
        swept
            .errors
            .iter()
            .any(|error| error.contains("no free name to move a deep tree up under")),
        "{swept:?}"
    );
}

/// Catches: a refused move (the park cannot be written) taken as a taken name and
/// retried 4096 times, or swallowed: the system's error must be recorded.
#[test]
fn a_park_that_cannot_be_written_records_the_refusal() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("park-unwritable");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    build_deep(&top, 100);
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        {
            let top = top.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "d") && !done.replace(true) {
                    chmod(&top, 0o500);
                }
            }
        },
        || empty(&top, me()),
    );
    chmod(&top, 0o700);
    assert!(
        swept
            .errors
            .iter()
            .any(|error| error.contains("ermission denied")),
        "{swept:?}"
    );
}

/// Catches: a folder the owner cannot open left closed by the flags walk (its contents
/// would then be skipped for good), or given back through a path that follows a link:
/// the owner's `rwx` is added by name when the descriptor cannot be opened.
#[test]
fn the_owner_is_given_back_rwx_by_name_on_a_folder_it_cannot_open() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("owner-rwx");
    fs::create_dir(dir.join("closed")).unwrap();
    chmod(&dir.join("closed"), 0o000);
    let fd = open_base(&dir).unwrap().unwrap();
    let stat = rustix::fs::statat(&fd, "closed", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    assert!(cannot_open(&fd, OsStr::new("closed")));
    make_owner_rwx(&fd, OsStr::new("closed"), &stat).unwrap();
    assert!(!cannot_open(&fd, OsStr::new("closed")));
    let mode = fs::metadata(dir.join("closed"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// Catches: a directory with no extended attributes listed as having some, or as an
/// error (the size answer is zero and no second call is made).
#[cfg(not(target_os = "macos"))]
#[test]
fn a_directory_without_user_attributes_lists_none() {
    let dir = scratch("xattr-none");
    let fd = open_base(&dir).unwrap().unwrap();
    let names = crate::xattr::list(&fd).unwrap();
    assert!(
        names.iter().all(|name| !name.starts_with("user.")),
        "{names:?}"
    );
}

/// Catches: a refused listing (a descriptor opened only to name the directory, which the
/// system will not list the attributes of) taken as an empty list: the system's error
/// must be returned, so a sweep never strips nothing and reports success.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_listing_the_system_refuses_is_an_error() {
    let dir = scratch("xattr-refused");
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        dir.path(),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    assert!(crate::xattr::list(&fd).is_err());
}

/// Gives the owner `rwx` on every directory of the chain of `d`s below `fd` and on `fd`.
fn reopen_chain(mut fd: OwnedFd) {
    loop {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700)).unwrap();
        match rustix::fs::openat(&fd, "d", DIRECTORY, Mode::empty()) {
            Ok(next) => fd = next,
            Err(_) => return,
        }
    }
}

/// Catches: a parked tree that cannot be removed (its deepest folder cannot be written)
/// reported as success, or the error lost: the sweep must record the system's refusal.
#[test]
fn a_parked_tree_that_cannot_be_removed_records_the_refusal() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("park-drain-fails");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    build_deep(&top, 100);
    // The deepest folder is closed to writes: its file cannot be unlinked.
    let mut fd = rustix::fs::openat(rustix::fs::CWD, &top, DIRECTORY, Mode::empty()).unwrap();
    while let Ok(next) = rustix::fs::openat(&fd, "d", DIRECTORY, Mode::empty()) {
        fd = next;
    }
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o500)).unwrap();
    drop(fd);
    let swept = empty(&top, me());
    for name in names(&top) {
        let parked = rustix::fs::openat(rustix::fs::CWD, top.join(name), DIRECTORY, Mode::empty());
        reopen_chain(parked.unwrap());
    }
    assert!(
        swept
            .errors
            .iter()
            .any(|error| error.contains("ermission denied")),
        "{swept:?}"
    );
}
