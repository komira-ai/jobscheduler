//! Which directories above a privileged work root are trusted: owned by root or by the
//! user who owns the root's parent, and not writable by others unless sticky.

mod common;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use common::Scratch;
use common::ancestry::{Reason, Refusal, allowed_owners, untrusted_ancestor};

fn chmod(dir: &Path, mode: u32) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// `<scratch>/above/middle/root`, all 0755, and the scratch directory as the top.
fn chain(scratch: &Scratch) -> (PathBuf, PathBuf, PathBuf) {
    let root = scratch.dir("above/middle/root");
    let middle = root.parent().expect("middle").to_owned();
    let above = middle.parent().expect("above").to_owned();
    for dir in [&root, &middle, &above, &scratch.root] {
        chmod(dir, 0o755);
    }
    (root, middle, above)
}

fn me(dir: &Path) -> u32 {
    std::fs::metadata(dir).expect("directory").uid()
}

#[test]
fn ancestors_owned_by_an_allowed_user_and_closed_to_others_are_trusted() {
    let scratch = Scratch::new("anc-fine");
    let (root, _, _) = chain(&scratch);
    assert_eq!(untrusted_ancestor(&root, &scratch.root, &[me(&root)]), None);
}

#[test]
fn an_ancestor_writable_by_the_group_without_sticky_is_refused() {
    let scratch = Scratch::new("anc-group");
    let (root, middle, _) = chain(&scratch);
    chmod(&middle, 0o775);
    assert_eq!(
        untrusted_ancestor(&root, &scratch.root, &[me(&root)]),
        Some(Refusal {
            path: middle,
            reason: Reason::Writable(0o775)
        })
    );
}

#[test]
fn an_ancestor_writable_by_others_without_sticky_is_refused_and_the_nearest_is_named() {
    let scratch = Scratch::new("anc-other");
    let (root, middle, above) = chain(&scratch);
    chmod(&above, 0o757);
    let refused = untrusted_ancestor(&root, &scratch.root, &[me(&root)]);
    assert_eq!(
        refused,
        Some(Refusal {
            path: above.clone(),
            reason: Reason::Writable(0o757)
        })
    );
    chmod(&middle, 0o777);
    let refused = untrusted_ancestor(&root, &scratch.root, &[me(&root)]);
    assert_eq!(
        refused,
        Some(Refusal {
            path: middle,
            reason: Reason::Writable(0o777)
        })
    );
}

#[test]
fn a_writable_ancestor_with_the_sticky_bit_is_trusted() {
    let scratch = Scratch::new("anc-sticky");
    let (root, middle, _) = chain(&scratch);
    chmod(&middle, 0o1777);
    assert_eq!(untrusted_ancestor(&root, &scratch.root, &[me(&root)]), None);
}

#[test]
fn an_ancestor_owned_by_a_user_not_allowed_is_refused() {
    let scratch = Scratch::new("anc-owner");
    let (root, middle, _) = chain(&scratch);
    let owner = me(&middle);
    let other = owner + 1;
    assert_eq!(
        untrusted_ancestor(&root, &scratch.root, &[other]),
        Some(Refusal {
            path: middle,
            reason: Reason::Owner(owner)
        })
    );
}

#[test]
fn the_walk_includes_the_top_and_stops_there() {
    let scratch = Scratch::new("anc-top");
    let (root, _, above) = chain(&scratch);
    let allowed = [me(&root)];
    chmod(&scratch.root, 0o775);
    assert_eq!(untrusted_ancestor(&root, &above, &allowed), None);
    chmod(&above, 0o775);
    assert_eq!(
        untrusted_ancestor(&root, &above, &allowed),
        Some(Refusal {
            path: above,
            reason: Reason::Writable(0o775)
        })
    );
}

#[test]
fn the_allowed_owners_are_root_and_the_uid_the_operator_gave() {
    assert_eq!(allowed_owners(501), vec![0, 501]);
}

#[test]
#[should_panic(expected = "not root")]
fn the_operators_uid_cannot_be_root() {
    let _ = allowed_owners(0);
}

#[test]
fn a_parent_owned_by_a_third_user_is_refused_even_though_it_is_the_parent() {
    let scratch = Scratch::new("anc-third");
    let (root, middle, _) = chain(&scratch);
    let third = me(&middle);
    // The operator's uid is someone else's: the directory's own owner earns no trust.
    let refused = untrusted_ancestor(&root, &scratch.root, &allowed_owners(third + 1));
    assert_eq!(
        refused,
        Some(Refusal {
            path: middle,
            reason: Reason::Owner(third)
        })
    );
}

#[test]
fn the_uid_the_operator_gave_is_trusted_as_an_owner() {
    let scratch = Scratch::new("anc-given");
    let (root, _, _) = chain(&scratch);
    assert_eq!(
        untrusted_ancestor(&root, &scratch.root, &allowed_owners(me(&root))),
        None
    );
}

/// Needs root to give a directory to someone else; run as root with
/// `--ignored` on a provisioned Mac.
#[test]
#[ignore = "needs root to chown a directory"]
fn an_ancestor_given_to_another_user_is_refused_as_root() {
    let scratch = Scratch::new("anc-chown");
    let (root, _, above) = chain(&scratch);
    std::os::unix::fs::chown(&above, Some(65534), None).expect("chown");
    assert_eq!(
        untrusted_ancestor(&root, &scratch.root, &allowed_owners(65533)),
        Some(Refusal {
            path: above,
            reason: Reason::Owner(65534)
        })
    );
}
