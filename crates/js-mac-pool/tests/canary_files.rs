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
