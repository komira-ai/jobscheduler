//! Tests of the walks over files: removing what a uid owns and looking for it, against real
//! directories. Every file a test makes belongs to the test's own uid, so the uid of an
//! account under test is the test's (`me`) when it should find them, and the next one
//! (`other`) when it should find none: a directory that is not the uid's is another owner's.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::*;
use crate::mac_accounts::tests::rig::me;

fn other() -> u32 {
    me() + 1
}

fn root() -> bool {
    me() == 0
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-accounts-tree-{name}"),
    )
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A place holding a file, a directory tree and an empty directory.
fn populate(base: &Path) {
    fs::create_dir_all(base).unwrap();
    fs::write(base.join("file"), "x").unwrap();
    fs::create_dir_all(base.join("tree/a/b")).unwrap();
    fs::write(base.join("tree/a/b/deep"), "x").unwrap();
    fs::create_dir(base.join("empty")).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Makes `levels` directories nested in `top` (named `d`) with a file at the bottom, through
/// descriptors, one open at a time: no path of this length can be made by name.
fn build_deep(top: &Path, levels: usize) {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let mut dir = rustix::fs::openat(CWD, top, flags, Mode::empty()).unwrap();
    for _ in 0..levels {
        rustix::fs::mkdirat(&dir, "d", Mode::from_raw_mode(0o755)).unwrap();
        dir = rustix::fs::openat(&dir, "d", flags, Mode::empty()).unwrap();
    }
    let file = rustix::fs::openat(
        &dir,
        "leaf",
        OFlags::CREATE | OFlags::WRONLY,
        Mode::from_raw_mode(0o644),
    );
    drop(file.unwrap());
}

fn stat_of(path: &Path) -> Stat {
    rustix::fs::lstat(path).unwrap()
}

/// Asserts that a place that is not there holds nothing: no entry, nothing to remove.
#[test]
fn a_place_that_does_not_exist_holds_nothing() {
    let dir = scratch("absent");
    assert_eq!(
        owned_in(&dir.join("none"), me()).unwrap(),
        Vec::<PathBuf>::new()
    );
    assert_eq!(sweep_owned(&dir.join("none"), me()).unwrap(), 0);
}

/// Asserts that a look finds each entry the uid owns, as the entry and not what is in it,
/// and changes nothing.
#[test]
fn a_look_finds_the_entries_the_uid_owns_and_changes_nothing() {
    let dir = scratch("look");
    let base = dir.join("Shared");
    populate(&base);
    let mut found = owned_in(&base, me()).unwrap();
    found.sort();
    assert_eq!(
        found,
        [base.join("empty"), base.join("file"), base.join("tree")]
    );
    assert!(base.join("tree/a/b/deep").is_file());
}

/// Asserts that nothing of another uid is found or removed (another account's files, the
/// system's), though every directory of it is searched.
#[test]
fn nothing_of_another_uid_is_found_or_removed() {
    let dir = scratch("others");
    let base = dir.join("Shared");
    populate(&base);
    assert!(owned_in(&base, other()).unwrap().is_empty());
    assert_eq!(sweep_owned(&base, other()).unwrap(), 0);
    assert_eq!(names(&base), ["empty", "file", "tree"]);
    assert!(base.join("tree/a/b/deep").is_file());
}

/// Asserts that everything the uid owns goes, whole, and the place itself stays; and that
/// the count is of entries removed at the top (a tree counts once).
#[test]
fn everything_the_uid_owns_goes_and_the_place_stays() {
    let dir = scratch("sweep");
    let base = dir.join("Shared");
    populate(&base);
    assert_eq!(sweep_owned(&base, me()).unwrap(), 3);
    assert!(names(&base).is_empty());
    assert!(base.is_dir());
}

/// Asserts that no walk follows a link: another account can plant one in a shared place to
/// somewhere root would then empty. The link goes and its target stays; a place that is
/// itself a link is not entered.
#[test]
fn a_planted_link_is_removed_and_its_target_kept() {
    let dir = scratch("link");
    let base = dir.join("Shared");
    let outside = dir.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "x").unwrap();
    symlink(&outside, base.join("to-outside")).unwrap();
    symlink(outside.join("precious"), base.join("to-file")).unwrap();
    fs::create_dir(base.join("sub")).unwrap();
    symlink(&outside, base.join("sub/nested-link")).unwrap();
    assert_eq!(sweep_owned(&base, me()).unwrap(), 3);
    assert!(names(&base).is_empty());
    assert_eq!(names(&outside), ["precious"]);

    symlink(&outside, dir.join("linked-place")).unwrap();
    assert!(sweep_owned(&dir.join("linked-place"), me()).is_err());
    assert!(owned_in(&dir.join("linked-place"), me()).is_err());
    assert_eq!(names(&outside), ["precious"]);
    fs::write(dir.join("a-file"), "x").unwrap();
    let why = owned_in(&dir.join("a-file"), me()).unwrap_err().to_string();
    assert!(
        why.starts_with(&format!("{}: ", dir.join("a-file").display())),
        "{why}"
    );
}

/// Asserts that a named entry is removed whole whoever owns it (a home, a crontab), a link
/// is removed and not followed, a missing entry or a missing directory above it holds
/// nothing, and a path that names no entry is refused.
#[test]
fn a_named_entry_goes_whole_and_an_absent_one_is_fine() {
    let dir = scratch("named");
    fs::create_dir_all(dir.join("Users/kbf-acct-2/Library/Caches")).unwrap();
    fs::write(dir.join("Users/kbf-acct-2/Library/Caches/c"), "x").unwrap();
    fs::write(dir.join("Users/kbf-acct-2/file"), "x").unwrap();
    for uid in [me(), other()] {
        assert!(
            remove_named(&dir.join("Users/kbf-acct-2"), uid).unwrap(),
            "{uid}"
        );
        assert!(!dir.join("Users/kbf-acct-2").exists());
        fs::create_dir_all(dir.join("Users/kbf-acct-2/inner")).unwrap();
    }
    fs::write(dir.join("elsewhere"), "x").unwrap();
    symlink(dir.join("elsewhere"), dir.join("Users/link")).unwrap();
    assert!(remove_named(&dir.join("Users/link"), me()).unwrap());
    assert!(dir.join("elsewhere").is_file());
    fs::write(dir.join("Users/plain"), "x").unwrap();
    assert!(remove_named(&dir.join("Users/plain"), me()).unwrap());
    assert!(!remove_named(&dir.join("Users/none"), me()).unwrap());
    assert!(!remove_named(&dir.join("no-such-dir/none"), me()).unwrap());
    let why = remove_named(Path::new("/"), me()).unwrap_err();
    assert_eq!(why.to_string(), "not a path to an entry");
    let why = remove_named(&dir.join("elsewhere/none"), me())
        .unwrap_err()
        .to_string();
    assert!(why.contains("elsewhere: "), "{why}");
}

/// Asserts that a look at a named entry sees whatever is there, a link included and not
/// followed, and nothing for a missing entry or directory; and fails for a path that names
/// no entry or whose directory cannot be opened.
#[test]
fn a_named_entry_exists_when_anything_is_there() {
    let dir = scratch("exists");
    fs::write(dir.join("file"), "x").unwrap();
    symlink(dir.join("missing-target"), dir.join("dangling")).unwrap();
    assert!(exists(&dir.join("file")).unwrap());
    assert!(exists(&dir.join("dangling")).unwrap());
    assert!(!exists(&dir.join("none")).unwrap());
    assert!(!exists(&dir.join("no-dir/none")).unwrap());
    assert_eq!(
        exists(Path::new("/")).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let why = exists(&dir.join("file/none")).unwrap_err().to_string();
    assert!(why.contains("file: "), "{why}");
}

/// Asserts that the places of an account are swept with its name and uid put in, that what
/// is not there is no error, and that a look finds what a sweep would (and a template that
/// names the account is not looked at when only a uid is known).
#[test]
fn places_are_swept_and_looked_at_with_the_accounts_name_and_uid() {
    let dir = scratch("places");
    let at = |rest: &str| format!("{}/{rest}", dir.display());
    fs::create_dir_all(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/kbf-acct-7"), "* * * * * true\n").unwrap();
    fs::write(dir.join("tabs/kbf-acct-8"), "x").unwrap();
    fs::create_dir_all(dir.join("launchd")).unwrap();
    fs::write(dir.join("launchd/disabled.4242.plist"), "x").unwrap();
    fs::write(dir.join("launchd/disabled.4243.plist"), "x").unwrap();
    populate(&dir.join("Shared"));
    let places = Places {
        named: vec![
            at("tabs/{name}"),
            at("launchd/disabled.{uid}.plist"),
            at("launchd/loginitems.{uid}.plist"),
            at("no-such-parent/{name}"),
        ],
        owned: vec![dir.join("Shared"), dir.join("no-such-place")],
    };
    // Uid 4242 owns nothing in the shared place: only the named entries are found.
    let mut left = left_in(&places, Some("kbf-acct-7"), 4242).unwrap();
    left.sort();
    assert_eq!(
        left,
        [
            dir.join("launchd/disabled.4242.plist"),
            dir.join("tabs/kbf-acct-7"),
        ]
    );
    // A uid alone does not name the crontab.
    assert_eq!(
        left_in(&places, None, 4242).unwrap(),
        [dir.join("launchd/disabled.4242.plist")]
    );
    // The test's own uid owns the shared place's entries.
    assert_eq!(left_in(&places, None, me()).unwrap().len(), 3);

    assert_eq!(sweep(&places, "kbf-acct-7", 4242).unwrap(), 2);
    assert_eq!(names(&dir.join("tabs")), ["kbf-acct-8"]);
    assert_eq!(names(&dir.join("launchd")), ["disabled.4243.plist"]);
    assert_eq!(
        names(&dir.join("Shared")).len(),
        3,
        "uid 4242 owns none of it"
    );
    assert_eq!(sweep(&places, "kbf-acct-7", me()).unwrap(), 3);
    assert!(names(&dir.join("Shared")).is_empty());
}

/// Asserts that a place that fails does not hide the others, and that every failure is
/// named: a sweep or a look over a place that is a file, below a file, and one that works.
#[test]
fn a_failing_place_does_not_hide_the_others() {
    let dir = scratch("failing");
    fs::write(dir.join("a-file"), "x").unwrap();
    populate(&dir.join("Shared"));
    let file = dir.join("a-file");
    let places = Places {
        named: vec![
            format!("{}/{{name}}", file.display()),
            format!("{}/x", dir.join("a-file").display()),
        ],
        owned: vec![file.clone(), dir.join("Shared")],
    };
    let why = sweep(&places, "kbf-acct-1", me()).unwrap_err().to_string();
    assert_eq!(why.matches("a-file").count(), 3, "{why}");
    assert!(
        names(&dir.join("Shared")).is_empty(),
        "the place that works was swept"
    );
    populate(&dir.join("Shared"));
    let why = left_in(&places, Some("kbf-acct-1"), me())
        .unwrap_err()
        .to_string();
    assert_eq!(why.matches("a-file").count(), 3, "{why}");
}

/// Asserts that the per-user folders are searched two levels down for directories the uid
/// owns, and nothing else: a file, a link, a directory of another uid, a file where a shard
/// should be and a shard that is a link are not entries.
#[test]
fn the_per_user_folders_are_searched_two_levels_down_for_directories_of_the_uid() {
    let dir = scratch("folders");
    let folders = dir.join("folders");
    fs::create_dir_all(folders.join("ab/hash1/T")).unwrap();
    fs::create_dir_all(folders.join("ab/hash2")).unwrap();
    fs::write(folders.join("ab/file"), "x").unwrap();
    symlink(dir.join("elsewhere"), folders.join("ab/link")).unwrap();
    fs::create_dir_all(folders.join("cd")).unwrap();
    fs::write(folders.join("not-a-shard"), "x").unwrap();
    fs::create_dir_all(dir.join("elsewhere/hash3")).unwrap();
    symlink(dir.join("elsewhere"), folders.join("ef")).unwrap();
    let mut found = folder_entries(&folders, me()).unwrap();
    found.sort();
    assert_eq!(found, [folders.join("ab/hash1"), folders.join("ab/hash2")]);
    assert!(folder_entries(&folders, other()).unwrap().is_empty());
    assert!(folder_entries(&dir.join("none"), me()).unwrap().is_empty());
    let why = folder_entries(&dir.join("folders/not-a-shard"), me())
        .unwrap_err()
        .to_string();
    assert!(
        why.starts_with(&format!("{}: ", folders.join("not-a-shard").display())),
        "{why}"
    );
}

/// Asserts that a shard that cannot be read fails the search naming the folders (a search
/// that skipped it would read what it did not see as clean).
#[test]
fn a_shard_that_cannot_be_read_fails_the_search() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("shard");
    let folders = dir.join("folders");
    fs::create_dir_all(folders.join("ab/hash")).unwrap();
    chmod(&folders.join("ab"), 0o000);
    let why = folder_entries(&folders, me()).unwrap_err().to_string();
    chmod(&folders.join("ab"), 0o755);
    assert!(
        why.starts_with(&format!("{}: ", folders.display())),
        "{why}"
    );
}

/// Asserts that a per-user folder is emptied and kept: the folder and its top-level
/// directories stay, everything below them goes whoever owns it, and so does a non-directory
/// at the top level; a link is removed and not followed (its target outside survives); a file,
/// a link, a directory of another uid and an entry whose parent is a link are refused and stay;
/// a missing entry or parent holds nothing; and a path that names no entry is refused.
#[test]
fn a_folder_is_emptied_and_kept_and_only_the_uids_directory() {
    let dir = scratch("folder-entry");
    let folders = dir.join("folders");
    let hash = folders.join("ab/hash");
    for top in ["0", "C", "T", "X"] {
        fs::create_dir_all(hash.join(top)).unwrap();
    }
    fs::create_dir_all(hash.join("T/inner")).unwrap();
    fs::write(hash.join("T/inner/f"), "x").unwrap();
    fs::write(hash.join("0/file"), "x").unwrap();
    fs::write(hash.join("stray"), "x").unwrap();
    fs::create_dir_all(dir.join("elsewhere")).unwrap();
    fs::write(dir.join("elsewhere/precious"), "x").unwrap();
    symlink(dir.join("elsewhere"), hash.join("C/link")).unwrap();
    symlink(dir.join("elsewhere"), hash.join("stray-link")).unwrap();
    fs::write(folders.join("ab/file"), "x").unwrap();
    symlink(dir.join("elsewhere"), folders.join("ab/link")).unwrap();
    fs::create_dir_all(folders.join("ab/theirs")).unwrap();

    empty_folder(&hash, me(), &System).unwrap();
    assert_eq!(names(&hash), ["0", "C", "T", "X"]);
    for top in ["0", "C", "T", "X"] {
        assert!(names(&hash.join(top)).is_empty(), "{top}");
    }
    assert_eq!(
        names(&folders.join("ab")),
        ["file", "hash", "link", "theirs"]
    );
    assert_eq!(names(&dir.join("elsewhere")), ["precious"]);
    // Emptied already: nothing to do, nothing wrong.
    empty_folder(&hash, me(), &System).unwrap();

    let refused = |path: &str, uid: u32| {
        empty_folder(&folders.join(path), uid, &System)
            .unwrap_err()
            .to_string()
    };
    let file = folders.join("ab/file");
    assert_eq!(
        refused("ab/file", me()),
        format!(
            "{}: not a directory on its parent's filesystem; not emptied",
            file.display()
        )
    );
    let link = folders.join("ab/link");
    assert_eq!(
        refused("ab/link", me()),
        format!(
            "{}: not a directory on its parent's filesystem; not emptied",
            link.display()
        )
    );
    let theirs = folders.join("ab/theirs");
    assert_eq!(
        refused("ab/theirs", other()),
        format!(
            "{}: owned by uid {}, not uid {}; not emptied",
            theirs.display(),
            me(),
            other()
        )
    );
    assert_eq!(
        names(&folders.join("ab")),
        ["file", "hash", "link", "theirs"]
    );
    assert_eq!(names(&dir.join("elsewhere")), ["precious"]);
    empty_folder(&folders.join("ab/none"), me(), &System).unwrap();
    empty_folder(&folders.join("none/none"), me(), &System).unwrap();
    assert_eq!(
        empty_folder(Path::new("/"), me(), &System)
            .unwrap_err()
            .to_string(),
        "not a path to an entry"
    );
    let why = empty_folder(&folders.join("ab/file/none"), me(), &System)
        .unwrap_err()
        .to_string();
    assert!(why.contains("file: "), "{why}");
}

/// Asserts that a folder or a top-level directory that cannot be opened or listed is named,
/// and that a look at it fails the same way (an error in looking is never "empty"), as does a
/// top-level directory on another filesystem than the folder, which is never entered.
#[test]
fn a_folder_that_cannot_be_read_is_named() {
    let dir = scratch("folder-unreadable");
    let hash = dir.join("folders/ab/hash");
    fs::create_dir_all(hash.join("T")).unwrap();
    fs::write(hash.join("T/f"), "x").unwrap();
    if !root() {
        chmod(&hash, 0o000);
        let why = empty_folder(&hash, me(), &System).unwrap_err().to_string();
        assert!(why.starts_with(&format!("{}: ", hash.display())), "{why}");
        let why = leftovers(&hash).unwrap_err().to_string();
        assert!(why.starts_with(&format!("{}: ", hash.display())), "{why}");
        chmod(&hash, 0o755);
        chmod(&hash.join("T"), 0o000);
        let why = leftovers(&hash).unwrap_err().to_string();
        assert!(
            why.starts_with(&format!("{}: ", hash.join("T").display())),
            "{why}"
        );
        chmod(&hash.join("T"), 0o755);
    }

    let folder = rustix::fs::openat(CWD, &hash, DIRECTORY, Mode::empty()).unwrap();
    let dev = rustix::fs::fstat(&folder).unwrap().st_dev;
    let top = stat_of(&hash.join("T"));
    let why = top_level(
        &folder,
        OsStr::new("T"),
        &hash.join("T"),
        &top,
        (me(), dev ^ 1),
        &System,
    )
    .unwrap_err();
    assert_eq!(
        why.to_string(),
        "another filesystem is mounted here; not crossed"
    );
    assert!(hash.join("T/f").is_file());
    let mut elsewhere = stat_of(&hash.join("T/f"));
    elsewhere.st_dev ^= 1;
    let inside = rustix::fs::openat(&folder, "T", DIRECTORY, Mode::empty()).unwrap();
    let why = top_level(
        &inside,
        OsStr::new("f"),
        &hash.join("T/f"),
        &elsewhere,
        (me(), dev),
        &System,
    )
    .unwrap_err();
    assert_eq!(
        why.to_string(),
        "another filesystem is mounted here; not crossed"
    );
    assert!(hash.join("T/f").is_file());
    // One that vanished after it was listed is emptied already.
    top_level(
        &folder,
        OsStr::new("gone"),
        &hash.join("gone"),
        &top,
        (me(), dev),
        &System,
    )
    .unwrap();
}

/// Asserts that a lifting removal the system still refuses after the ACL was stripped is an
/// error naming the entry, and the rest of the folder is emptied: a file at the top level of a
/// folder the owner cannot write in (the folder itself is not made writable: it is the
/// system's), whose removal is refused, and whose ACL strip, which off macOS does nothing,
/// changes nothing.
#[test]
fn a_removal_the_system_still_refuses_after_the_acl_is_stripped_is_named() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("folder-refused");
    let hash = dir.join("folders/ab/hash");
    for top in ["0", "T"] {
        fs::create_dir_all(hash.join(top)).unwrap();
    }
    fs::write(hash.join("0/file"), "x").unwrap();
    fs::write(hash.join("stray"), "x").unwrap();
    chmod(&hash, 0o500);
    let why = empty_folder(&hash, me(), &System).unwrap_err().to_string();
    chmod(&hash, 0o755);
    assert!(
        why.contains(&format!(
            "{}: Permission denied",
            hash.join("stray").display()
        )),
        "{why}"
    );
    assert!(names(&hash.join("0")).is_empty(), "the rest was emptied");
}

/// Asserts that a directory that is not the one that was examined is not changed: a directory
/// swapped in after the examination keeps its mode, and the refusal says so (Linux changes a
/// directory through a descriptor it opened without following a link and compares).
#[cfg(target_os = "linux")]
#[test]
fn a_directory_swapped_in_before_it_is_made_searchable_keeps_its_mode() {
    let dir = scratch("searchable");
    let base = dir.join("place");
    fs::create_dir_all(base.join("a")).unwrap();
    fs::create_dir_all(base.join("b")).unwrap();
    chmod(&base.join("a"), 0o000);
    chmod(&base.join("b"), 0o000);
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let examined = stat_of(&base.join("a"));
    let why = searchable(&place, OsStr::new("b"), &examined).unwrap_err();
    assert_eq!(why.to_string(), "replaced while being examined");
    assert_eq!(stat_of(&base.join("b")).st_mode & 0o777, 0);
    // The one examined is changed, and one that is searchable already is left alone.
    searchable(&place, OsStr::new("a"), &examined).unwrap();
    assert_eq!(stat_of(&base.join("a")).st_mode & 0o777, 0o700);
    let opened = stat_of(&base.join("a"));
    searchable(&place, OsStr::new("a"), &opened).unwrap();
    chmod(&base.join("b"), 0o755);
}

/// Asserts that a look at a folder passes over an entry that vanished after it was listed.
#[test]
fn a_look_passes_over_an_entry_that_vanished() {
    let dir = scratch("look-vanished");
    let hash = dir.join("hash");
    fs::create_dir_all(&hash).unwrap();
    let folder = rustix::fs::openat(CWD, &hash, DIRECTORY, Mode::empty()).unwrap();
    let mut left = Vec::new();
    top_leftovers(&folder, OsStr::new("gone"), &hash.join("gone"), &mut left).unwrap();
    assert!(left.is_empty());
}

/// Asserts what a look at a folder reports: each non-directory at its top level and each entry
/// directly below a top-level directory (the entry, not what is in it), none for the top-level
/// directories themselves or for a folder that is not there.
#[test]
fn a_look_at_a_folder_reports_what_an_emptied_one_does_not_hold() {
    let dir = scratch("folder-look");
    let hash = dir.join("folders/ab/hash");
    for top in ["0", "C", "T", "X"] {
        fs::create_dir_all(hash.join(top)).unwrap();
    }
    assert!(leftovers(&hash).unwrap().is_empty());
    assert!(leftovers(&dir.join("none")).unwrap().is_empty());
    fs::create_dir_all(hash.join("T/inner/deeper")).unwrap();
    fs::write(hash.join("C/file"), "x").unwrap();
    fs::write(hash.join("stray"), "x").unwrap();
    symlink(dir.join("elsewhere"), hash.join("X/link")).unwrap();
    let mut left = leftovers(&hash).unwrap();
    left.sort();
    assert_eq!(
        left,
        [
            hash.join("C/file"),
            hash.join("T/inner"),
            hash.join("X/link"),
            hash.join("stray"),
        ]
    );
}

/// Asserts that a tree of any depth is removed (a job that builds a deep tree cannot stop
/// its account's deletion) by the three removals: of a named entry, of every entry a uid owns
/// in a place, and the emptying of a per-user folder. At this depth a walk that held a
/// descriptor per level would run out of them.
#[test]
fn a_tree_thousands_deep_is_removed_by_every_removal() {
    let dir = scratch("deep");
    let levels = 3000;
    let base = dir.join("Shared");
    fs::create_dir_all(base.join("tree")).unwrap();
    build_deep(&base.join("tree"), levels);
    assert_eq!(sweep_owned(&base, me()).unwrap(), 1);
    assert!(names(&base).is_empty());

    fs::create_dir_all(dir.join("Users/home")).unwrap();
    build_deep(&dir.join("Users/home"), levels);
    assert!(remove_named(&dir.join("Users/home"), me()).unwrap());
    assert!(names(&dir.join("Users")).is_empty());

    fs::create_dir_all(dir.join("folders/ab/hash/T")).unwrap();
    build_deep(&dir.join("folders/ab/hash/T"), levels);
    empty_folder(&dir.join("folders/ab/hash"), me(), &System).unwrap();
    assert_eq!(names(&dir.join("folders/ab/hash")), ["T"]);
    assert!(names(&dir.join("folders/ab/hash/T")).is_empty());
}

/// Asserts that the directories of other owners are searched to a limit and no further, so
/// that no other account's deep tree can stop a deletion: a look finds nothing below the
/// limit and fails on nothing.
#[test]
fn the_directories_of_other_owners_are_searched_to_a_limit() {
    let dir = scratch("limit");
    fs::create_dir(dir.join("Shared")).unwrap();
    build_deep(&dir.join("Shared"), MAX_DEPTH + 10);
    assert!(owned_in(&dir.join("Shared"), other()).unwrap().is_empty());
    assert_eq!(sweep_owned(&dir.join("Shared"), other()).unwrap(), 0);
}

/// Asserts that a directory that cannot be searched fails the look, naming it: what was not
/// searched is never read as holding nothing of the uid. The sweep fails the same way and
/// still removes what it can.
#[test]
fn a_directory_that_cannot_be_searched_fails_the_walk_naming_it() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("unsearchable");
    let base = dir.join("Shared");
    populate(&base);
    fs::create_dir(base.join("locked")).unwrap();
    chmod(&base.join("locked"), 0o000);
    let locked = base.join("locked");
    let look = owned_in(&base, other()).unwrap_err().to_string();
    assert!(look.contains(&format!("{}: ", locked.display())), "{look}");
    let sweep = sweep_owned(&base, other()).unwrap_err().to_string();
    assert!(
        sweep.contains(&format!("{}: ", locked.display())),
        "{sweep}"
    );
    // The uid's own entries: the one that cannot be emptied is named, the others go.
    let sweep = sweep_owned(&base, me()).unwrap_err().to_string();
    assert!(
        sweep.contains(&format!("{}: ", locked.display())),
        "{sweep}"
    );
    assert_eq!(names(&base), ["locked"]);
    chmod(&locked, 0o755);
}

/// Asserts that a removal the system refuses is named for every entry it refused, and the
/// rest of the directory is still removed.
#[test]
fn a_refusal_is_named_for_every_entry_and_the_rest_goes() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("refusal");
    let base = dir.join("Shared");
    fs::create_dir_all(base.join("closed")).unwrap();
    fs::write(base.join("closed/a"), "x").unwrap();
    fs::write(base.join("closed/b"), "x").unwrap();
    fs::write(base.join("loose"), "x").unwrap();
    chmod(&base.join("closed"), 0o500);
    let why = remove_named(&base, me()).unwrap_err().to_string();
    assert!(
        why.contains("closed/a: ") && why.contains("closed/b: "),
        "{why}"
    );
    // Each path once: a directory that holds a failure is not named again at every level.
    let closed = format!("{}/closed", base.display());
    assert_eq!(why.matches(&closed).count(), 2, "{why}");
    assert_eq!(
        names(&base),
        ["closed"],
        "what the system allowed was removed"
    );
    chmod(&base.join("closed"), 0o755);
    assert!(remove_named(&base, me()).unwrap());
}

/// Asserts that an entry on another filesystem than the walk's is never entered or removed:
/// the walk skips it, and a removal that reaches one fails naming it.
#[test]
fn another_filesystem_is_never_entered() {
    let dir = scratch("device");
    let base = dir.join("Shared");
    populate(&base);
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let dev = rustix::fs::fstat(&place).unwrap().st_dev;
    let mut seen = Vec::new();
    let mut failed = Vec::new();
    let mut found = |entry: &Found<'_>| -> io::Result<()> {
        seen.push(entry.here.to_path_buf());
        Ok(())
    };
    walk(&place, &base, (me(), dev ^ 1, 0), &mut found, &mut failed).unwrap();
    assert!(seen.is_empty() && failed.is_empty());

    let park = Park::new(&place);
    let stat = stat_of(&base.join("file"));
    let why = remove_entry(
        &place,
        OsStr::new("file"),
        &base.join("file"),
        &stat,
        (me(), dev ^ 1, 1),
        &park,
    )
    .unwrap_err();
    assert_eq!(
        why.to_string(),
        "another filesystem is mounted here; not crossed"
    );
    assert!(base.join("file").is_file());

    let mut elsewhere = stat_of(&base.join("tree"));
    elsewhere.st_dev ^= 1;
    let why = remove_whole(
        &place,
        OsStr::new("tree"),
        &base.join("tree"),
        &elsewhere,
        me(),
    )
    .unwrap_err();
    assert_eq!(
        why.to_string(),
        "another filesystem is mounted here; not crossed"
    );
    assert!(base.join("tree/a/b/deep").is_file());
}

/// Asserts that an entry that vanishes between a step looking at it and acting on it is not
/// an error (a daemon removing its own temporary file): one gone after it was examined, or
/// whose directory was, is removed already; any other error is still one.
#[test]
fn an_entry_that_vanishes_is_not_an_error_and_other_errors_are() {
    let dir = scratch("vanish");
    let base = dir.join("Shared");
    populate(&base);
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let dev = rustix::fs::fstat(&place).unwrap().st_dev;
    let park = Park::new(&place);
    let file = stat_of(&base.join("file"));
    let tree = stat_of(&base.join("tree"));
    fs::remove_file(base.join("file")).unwrap();
    fs::remove_dir_all(base.join("tree")).unwrap();
    remove_entry(
        &place,
        OsStr::new("file"),
        &base.join("file"),
        &file,
        (me(), dev, 1),
        &park,
    )
    .unwrap();
    remove_entry(
        &place,
        OsStr::new("tree"),
        &base.join("tree"),
        &tree,
        (me(), dev, 1),
        &park,
    )
    .unwrap();
    let mut found = |_: &Found<'_>| -> io::Result<()> { panic!("nothing is there to find") };
    visit(
        &place,
        OsStr::new("file"),
        &base.join("file"),
        (me(), dev, 0),
        &mut found,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(
        open_child(&place, OsStr::new("tree"), &tree)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(vanished(Err(io::ErrorKind::NotFound.into())).is_ok());
    assert!(vanished(Err(io::ErrorKind::PermissionDenied.into())).is_err());
    assert!(vanished(Ok(())).is_ok());
    assert!(present::<()>(Err(Errno::NOENT)).unwrap().is_none());
    assert!(present::<()>(Err(Errno::ACCESS)).is_err());
    // One instantiation for every arm: arms split across types count as missed.
    assert_eq!(present::<()>(Ok(())).unwrap(), Some(()));
}

/// Asserts that a directory swapped in after it was examined is not entered: the walk opens
/// the entry it examined, or refuses, and never follows a link put in its place.
#[test]
fn a_directory_swapped_in_after_it_was_examined_is_refused() {
    let dir = scratch("swap");
    let base = dir.join("Shared");
    fs::create_dir_all(base.join("a")).unwrap();
    fs::create_dir_all(base.join("b")).unwrap();
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let examined_a = stat_of(&base.join("a"));
    // `b` is not the directory that was examined.
    let why = open_child(&place, OsStr::new("b"), &examined_a).unwrap_err();
    assert_eq!(why.to_string(), "replaced while being examined");
    // A link put where the directory was is not followed.
    fs::remove_dir(base.join("a")).unwrap();
    symlink(base.join("b"), base.join("a")).unwrap();
    assert!(open_child(&place, OsStr::new("a"), &examined_a).is_err());
}

/// Asserts how a deep tree is moved up: to a name of its own under the directory being
/// emptied, skipping a name already taken (never replacing it), refusing an entry replaced
/// since it was judged, giving up when no name is free, and failing on a directory it cannot
/// move into.
#[test]
fn a_deep_subtree_is_parked_under_a_free_name() {
    let dir = scratch("park");
    let base = dir.join("park");
    fs::create_dir_all(base.join("from/sub")).unwrap();
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let from = rustix::fs::openat(&place, "from", DIRECTORY, Mode::empty()).unwrap();
    let sub = stat_of(&base.join("from/sub"));
    let park = Park::new(&place);
    let first = format!(".kbf-deep-{}-0", std::process::id());
    fs::write(base.join(&first), "taken").unwrap();
    park.take(&from, OsStr::new("sub"), &sub).unwrap();
    assert_eq!(fs::read_to_string(base.join(&first)).unwrap(), "taken");
    assert!(
        base.join(format!(".kbf-deep-{}-1", std::process::id()))
            .is_dir()
    );
    assert!(!base.join("from/sub").exists());

    // An entry other than the one judged is not moved.
    fs::create_dir(base.join("from/other")).unwrap();
    let why = park.take(&from, OsStr::new("other"), &sub).unwrap_err();
    assert_eq!(why.to_string(), "replaced while being examined");
    let why = park.take(&from, OsStr::new("gone"), &sub).unwrap_err();
    assert_eq!(why.to_string(), "replaced while being examined");

    // No free name: every one the park may try is taken.
    let full = Park::new(&place);
    for n in 0..PARK_NAMES {
        let name = format!(".kbf-deep-{}-{n}", std::process::id());
        if !base.join(&name).exists() {
            fs::write(base.join(name), "").unwrap();
        }
    }
    let other = stat_of(&base.join("from/other"));
    let why = full.take(&from, OsStr::new("other"), &other).unwrap_err();
    assert_eq!(why.to_string(), "no free name to move a deep tree up under");

    // A directory it cannot move into: one removed while open.
    fs::create_dir(base.join("gone-park")).unwrap();
    let gone = rustix::fs::openat(&place, "gone-park", DIRECTORY, Mode::empty()).unwrap();
    fs::remove_dir(base.join("gone-park")).unwrap();
    let stuck = Park::new(&gone);
    let why = stuck.take(&from, OsStr::new("other"), &other).unwrap_err();
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
}

/// Asserts that a drain removes what was parked and what is parked while it drains, and one
/// that was removed already is not an error.
#[test]
fn a_drain_removes_what_was_parked() {
    let dir = scratch("drain");
    let base = dir.join("park");
    fs::create_dir_all(base.join("from/sub/inner")).unwrap();
    fs::write(base.join("from/sub/inner/f"), "x").unwrap();
    let place = rustix::fs::openat(CWD, &base, DIRECTORY, Mode::empty()).unwrap();
    let dev = rustix::fs::fstat(&place).unwrap().st_dev;
    let from = rustix::fs::openat(&place, "from", DIRECTORY, Mode::empty()).unwrap();
    let park = Park::new(&place);
    park.take(&from, OsStr::new("sub"), &stat_of(&base.join("from/sub")))
        .unwrap();
    park.names.borrow_mut().push(OsString::from("already-gone"));
    park.drain(me(), dev).unwrap();
    assert_eq!(names(&base), ["from"]);
    assert!(names(&base.join("from")).is_empty());
}

/// Asserts that an entry examined without following a link is `None` when it is not there,
/// and an error, never an absence, when its name cannot be looked up (too long for a name).
#[test]
fn an_entry_not_there_is_none_and_one_that_cannot_be_examined_is_an_error() {
    let dir = scratch("examine");
    let place = open_dir(&dir).unwrap().unwrap();
    assert!(examine(&place, OsStr::new("missing")).unwrap().is_none());
    let long = "n".repeat(300);
    let why = examine(&place, OsStr::new(&long)).unwrap_err();
    assert_eq!(why.raw_os_error(), Some(libc::ENAMETOOLONG));
}
