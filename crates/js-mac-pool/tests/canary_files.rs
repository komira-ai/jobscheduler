//! How the tests make files in world-writable directories: never through a link, under
//! names that are not predictable, and removed even when a test panics.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};

use common::Scratch;
use common::canary::{Cleanup, create_exclusive, unpredictable};

#[test]
fn a_link_planted_at_the_name_is_refused_and_its_target_is_untouched() {
    let scratch = Scratch::new("canary-link");
    let victim = scratch.file("victim", "precious\n");
    let target = scratch.root.join("js-mac-pool-guess");
    std::os::unix::fs::symlink(&victim, &target).expect("link");
    let refused = create_exclusive(&target, "secret\n").expect_err("a link is an existing entry");
    assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim"),
        "precious\n"
    );
}

#[test]
fn a_dangling_link_is_refused_too_and_nothing_is_made_through_it() {
    let scratch = Scratch::new("canary-dangling");
    let beyond = scratch.root.join("not-there");
    let target = scratch.root.join("js-mac-pool-guess");
    std::os::unix::fs::symlink(&beyond, &target).expect("link");
    assert!(create_exclusive(&target, "secret\n").is_err());
    assert!(!beyond.exists());
}

#[test]
fn a_plain_file_is_made_once_with_its_content_and_never_overwritten() {
    let scratch = Scratch::new("canary-once");
    let target = scratch.root.join("canary");
    create_exclusive(&target, "secret\n").expect("made");
    assert_eq!(
        std::fs::read_to_string(&target).expect("canary"),
        "secret\n"
    );
    assert!(create_exclusive(&target, "other\n").is_err());
    assert_eq!(
        std::fs::read_to_string(&target).expect("canary"),
        "secret\n"
    );
}

#[test]
fn unique_names_differ_and_the_file_is_recorded_and_removed_with_the_guard() {
    let scratch = Scratch::new("canary-unique");
    let (first, second);
    {
        let mut guard = Cleanup::default();
        first = guard.create_unique(&scratch.root, "js-mac-pool-x", "a");
        second = guard.create_unique(&scratch.root, "js-mac-pool-x", "b");
        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
        assert_eq!(unpredictable().len(), 16);
        assert_ne!(unpredictable(), unpredictable());
    }
    assert!(!first.exists());
    assert!(!second.exists());
}

#[test]
fn a_panic_after_a_file_was_made_still_removes_it() {
    let scratch = Scratch::new("canary-panic");
    let made = std::cell::RefCell::new(None);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = Cleanup::default();
        *made.borrow_mut() = Some(guard.create_unique(&scratch.root, "js-mac-pool-p", "a"));
        panic!("the Xcode is not ready");
    }));
    assert!(outcome.is_err());
    let path = made.borrow().clone().expect("a file was made");
    assert!(!path.exists());
}

#[test]
fn a_file_this_guard_did_not_make_is_not_removed() {
    let scratch = Scratch::new("canary-foreign");
    let foreign = scratch.file("foreign", "x");
    {
        let mut guard = Cleanup::default();
        let mine = guard.create_unique(&scratch.root, "js-mac-pool-f", "a");
        assert!(mine.exists());
    }
    assert!(foreign.exists());
}

fn panic_text(outcome: std::thread::Result<std::path::PathBuf>) -> String {
    let payload = outcome.expect_err("the call panics");
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(other) => (*other.downcast::<&str>().expect("a text panic")).to_owned(),
    }
}

fn names_in(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect()
}

#[test]
fn a_write_failure_after_the_file_was_made_leaves_no_file_and_fails_with_that_error() {
    let scratch = Scratch::new("canary-write-fails");
    let mut guard = Cleanup::default();
    let mut writes = 0;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        guard.create_unique_with(&scratch.root, "js-mac-pool-w", unpredictable, |_| {
            writes += 1;
            Err(std::io::Error::other("the disk is full"))
        })
    }));
    let text = panic_text(outcome);
    assert!(text.contains("the disk is full"), "{text}");
    assert_eq!(writes, 1, "a write failure is not retried under a new name");
    assert_eq!(names_in(&scratch.root), Vec::<std::ffi::OsString>::new());
}

#[test]
fn an_open_error_other_than_a_taken_name_fails_at_once_with_that_error() {
    let scratch = Scratch::new("canary-open-fails");
    let missing = scratch.root.join("not-there");
    let mut guard = Cleanup::default();
    let mut names = 0;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        guard.create_unique_with(
            &missing,
            "js-mac-pool-o",
            || {
                names += 1;
                unpredictable()
            },
            |_| Ok(()),
        )
    }));
    let text = panic_text(outcome);
    assert!(text.contains("No such file or directory"), "{text}");
    assert_eq!(names, 1, "no retry for an error that a new name cannot fix");
}

#[test]
fn running_out_of_names_panics_with_the_last_error() {
    let scratch = Scratch::new("canary-names-out");
    scratch.file("js-mac-pool-n-same", "taken");
    let mut guard = Cleanup::default();
    let mut names = 0;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        guard.create_unique_with(
            &scratch.root,
            "js-mac-pool-n",
            || {
                names += 1;
                "same".to_owned()
            },
            |_| Ok(()),
        )
    }));
    let text = panic_text(outcome);
    assert!(text.contains("File exists"), "{text}");
    assert_eq!(names, 16);
    assert_eq!(
        std::fs::read_to_string(scratch.root.join("js-mac-pool-n-same")).expect("kept"),
        "taken"
    );
}
