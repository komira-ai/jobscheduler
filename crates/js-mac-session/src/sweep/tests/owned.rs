//! The read-only look at what a uid left in a plan's places ([`owned_left`]): what it
//! reports, what it passes over as the sweep does, and that it changes nothing.

use super::*;
use crate::sweep::owned::look_in;

/// A plan with one named entry that names the user, one that names the uid, and the owned
/// place `base`.
fn plan(dir: &Path, base: &Path) -> SweepPlan {
    SweepPlan {
        named: vec![
            format!("{}/tabs/{{user}}", dir.display()),
            format!("{}/disabled.{{uid}}.plist", dir.display()),
        ],
        owned: vec![base.to_path_buf()],
    }
}

/// Catches: a look that misses a named entry that is there (the sweep removes exactly these
/// names), reports one that is not, reads a template that names the user when no user is
/// known, or takes a missing parent directory for an error.
#[test]
fn a_named_entry_is_left_when_it_is_there() {
    let dir = scratch("owned-named");
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    let plan = plan(&dir, &base);
    // Nothing is there, not even the folder the crontab would be in.
    assert_eq!(
        owned_left(&plan, Some("kbf-lease-1-1"), 650).unwrap(),
        Vec::<PathBuf>::new()
    );
    fs::create_dir(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/kbf-lease-1-1"), "x").unwrap();
    fs::write(dir.join("tabs/kbf-lease-1-2"), "x").unwrap();
    fs::write(dir.join("disabled.650.plist"), "x").unwrap();
    fs::write(dir.join("disabled.651.plist"), "x").unwrap();
    let mut left = owned_left(&plan, Some("kbf-lease-1-1"), 650).unwrap();
    left.sort();
    assert_eq!(
        left,
        [
            dir.join("disabled.650.plist"),
            dir.join("tabs/kbf-lease-1-1")
        ]
    );
    // With no user, a template that names the user is not looked at; the uid's is.
    assert_eq!(
        owned_left(&plan, None, 650).unwrap(),
        [dir.join("disabled.650.plist")]
    );
    assert_eq!(
        owned_left(&plan, Some("kbf-lease-1-3"), 652).unwrap(),
        Vec::<PathBuf>::new()
    );
    // A link counts, and is not followed.
    symlink(dir.join("nowhere"), dir.join("disabled.652.plist")).unwrap();
    assert_eq!(
        owned_left(&plan, None, 652).unwrap(),
        [dir.join("disabled.652.plist")]
    );
}

/// Catches: a template that is not a path to an entry taken as nothing left (the proof would
/// then pass over a place it could not look at).
#[test]
fn a_named_path_that_is_no_entry_is_an_error() {
    let plan = SweepPlan {
        named: vec!["/".to_owned()],
        owned: Vec::new(),
    };
    let error = owned_left(&plan, Some("u"), 650).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

/// Catches: a look that misses an entry the uid owns in a place (at the top, whatever it
/// is), reports another uid's, descends into what the uid owns (the entry is the answer),
/// stops at an absent place or fails for one, or changes anything.
#[test]
fn an_owned_place_names_what_the_uid_owns_and_changes_nothing() {
    let dir = scratch("owned-place");
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let plan = SweepPlan {
        named: Vec::new(),
        owned: vec![base.clone(), dir.join("absent")],
    };
    let mut left = owned_left(&plan, None, me()).unwrap();
    left.sort();
    assert_eq!(
        left,
        [base.join("empty"), base.join("file"), base.join("tree")]
    );
    assert_eq!(names(&base), ["empty", "file", "tree"]);
    assert!(base.join("tree/a/b/deep").is_file());
    // Another uid's view: every directory is searched, and nothing is the uid's.
    assert_eq!(
        owned_left(&plan, None, other()).unwrap(),
        Vec::<PathBuf>::new()
    );
    // A place that is not a directory cannot be looked in.
    let file = SweepPlan {
        named: Vec::new(),
        owned: vec![base.join("file")],
    };
    assert!(owned_left(&file, None, me()).is_err());
}

/// Catches: a look that goes on past the depth the sweep searches to, or fails for a tree
/// deeper than it, or one that never ends.
#[test]
fn a_tree_deeper_than_the_limit_is_searched_to_it_without_an_error() {
    let dir = scratch("owned-deep");
    let base = dir.join("Shared");
    let mut deep = base.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), "x").unwrap();
    let plan = owned_plan(&base);
    assert_eq!(
        owned_left(&plan, None, other()).unwrap(),
        Vec::<PathBuf>::new()
    );
    assert_eq!(owned_left(&plan, None, me()).unwrap(), [base.join("d")]);
}

/// Catches: an entry on another filesystem entered or named (a mounted volume is not
/// the sweep's to search, so nothing in it can be the uid's to answer for).
#[test]
fn another_filesystem_is_not_entered() {
    let dir = scratch("owned-dev");
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let opened = open_base(&base).unwrap().unwrap();
    let dev = rustix::fs::fstat(&opened).unwrap().st_dev;
    let mut left = Vec::new();
    look_in(&opened, &base, (me(), dev + 1), 0, &mut left).unwrap();
    assert_eq!(left, Vec::<PathBuf>::new());
    look_in(&opened, &base, (me(), dev), 0, &mut left).unwrap();
    assert_eq!(left.len(), 3);
}

/// Catches (as the sweep's): an entry that goes between the listing and its examination, or
/// a directory that goes between its examination and its opening, taken as an error: the
/// daemon that removes its own file is no reason to hold a uid.
#[test]
fn what_vanishes_while_it_is_looked_at_is_not_an_error() {
    let dir = scratch("owned-vanish");
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("daemon-temp"), "x").unwrap();
    fs::write(base.join("stays"), "x").unwrap();
    let temp = base.join("daemon-temp");
    let left = vanishing(after_listing(1, move || remove(&temp)), || {
        owned_left(&owned_plan(&base), None, me())
    });
    assert_eq!(left.unwrap(), [dir.join("Shared/stays")]);

    fs::create_dir_all(base.join("dir/sub")).unwrap();
    let left = vanishing(after_examining("dir", base.join("dir")), || {
        owned_left(&owned_plan(&base), None, other())
    });
    assert_eq!(left.unwrap(), Vec::<PathBuf>::new());
}

/// Catches: a directory of another owner that cannot be searched read as holding nothing
/// of the uid (the sweep passes it over; the look, which is a proof, fails and names it).
#[test]
fn a_directory_that_cannot_be_searched_fails_the_look_by_name() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("owned-closed");
    let base = dir.join("Shared");
    fs::create_dir_all(base.join("closed")).unwrap();
    fs::create_dir_all(base.join("open")).unwrap();
    chmod(&base.join("closed"), 0o000);
    let left = owned_left(&owned_plan(&base), None, other());
    chmod(&base.join("closed"), 0o700);
    let why = left.unwrap_err();
    assert_eq!(why.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        why.to_string(),
        format!(
            "{}: not searched: Permission denied (os error 13)",
            base.join("closed").display()
        )
    );
}
